//! Wire time against the Hub's wall time through repeated congestion episodes (DESIGN
//! §4.4, §4.5; SPEC N5). Its own test binary, since it installs a capturing logger to
//! match on-wire jumps with the rebases the Hub logs.

use neolink_rtsp::hub::task::{spawn, HubConfig};
use neolink_rtsp::hub::wire::Track;
use neolink_rtsp::source::{parse_bcm, FixtureSource, FixtureUnit, SourceUnit};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tokio::time::{Duration, Instant};

static LINES: Mutex<Vec<String>> = Mutex::new(vec![]);

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        LINES
            .lock()
            .unwrap()
            .push(format!("{} {}", r.level(), r.args()));
    }
    fn flush(&self) {}
}

fn capture_logs() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        log::set_logger(&Capture).unwrap();
        log::set_max_level(log::LevelFilter::Debug);
    });
}

/// The fixture is 120 s long with one congestion episode from about 58 s; looped three
/// times this is three episodes
const LAP_S: u64 = 120;
const LAPS: u64 = 3;
/// One GOP of the fixture (30 frames at 15 fps)
const GOP_US: i64 = 2_000_000;
/// SPEC N5: no on-wire jump above this outside a logged rebase
const JUMP_US: i64 = 1_000_000;

/// A video frame as the session saw it
struct Out {
    wall: u64,
    wire: u64,
    irap: bool,
}

fn fixture() -> Vec<FixtureUnit> {
    let path = format!(
        "{}/../../fixtures/synth/congestion_h265.bcm",
        env!("CARGO_MANIFEST_DIR")
    );
    let data = std::fs::read(&path)
        .unwrap_or_else(|_| panic!("{path}: run `cargo xtask mkfixture --standard` first"));
    parse_bcm(&data)
}

/// How much later than the earliest one the latest video frame arrives, against its
/// camera time (µs): the congestion episode's lag (about 15.8 s, start backlog included)
fn peak_lag(units: &[FixtureUnit]) -> i64 {
    let mut first: Option<u32> = None;
    let (mut lo, mut hi) = (i64::MAX, i64::MIN);
    for u in units {
        if let SourceUnit::Video { cam_us, .. } = u.unit {
            let media = cam_us.wrapping_sub(*first.get_or_insert(cam_us)) as i64;
            lo = lo.min(u.at_us as i64 - media);
            hi = hi.max(u.at_us as i64 - media);
        }
    }
    hi - lo
}

/// Play the looped congestion fixture through a Hub at `latency_us`, with bridging on
async fn run(name: &str, latency_us: u64) -> (Vec<Out>, Vec<String>) {
    capture_logs();
    let source = Arc::new(FixtureSource::new(fixture(), 1.0, Some(false)));
    source.looping.store(true, Ordering::SeqCst);
    let mut cfg = HubConfig::new(name);
    cfg.battery = false;
    cfg.latency_us = latency_us;
    assert!(cfg.use_splash, "bridging is on");
    // The Hub's wire clock starts at its creation: virtual time stands still until
    // something sleeps
    let t0 = Instant::now();
    let h = spawn(cfg, source);
    h.describe().await.unwrap();
    let mut sub = h.subscribe(1).await.unwrap();
    h.play(1).await;
    // And the steady part of a fourth lap, after the third episode
    let end = Instant::now() + Duration::from_secs(LAP_S * LAPS + 50);
    let mut video = vec![];
    let mut last: HashMap<Track, u64> = HashMap::new();
    loop {
        tokio::select! {
            f = sub.recv() => match f {
                Some(f) => {
                    if let Some(l) = last.get(&f.track) {
                        assert!(f.wire_us > *l, "{name}: {:?} went from {l} to {}", f.track, f.wire_us);
                    }
                    last.insert(f.track, f.wire_us);
                    if f.track == Track::Video {
                        let wall = (Instant::now() - t0).as_micros() as u64;
                        video.push(Out { wall, wire: f.wire_us, irap: f.irap });
                    }
                }
                None => panic!("{name}: the session was closed"),
            },
            _ = tokio::time::sleep_until(end) => break,
        }
    }
    let st = h.stats().await.unwrap();
    assert_eq!(st.guard_engaged, 0, "{name}: {st:?}");
    h.shutdown().await;
    let prefix = format!("{name}: ");
    let lines = LINES
        .lock()
        .unwrap()
        .iter()
        .filter(|l| {
            l.split_once(' ')
                .is_some_and(|(_, m)| m.starts_with(&prefix))
        })
        .cloned()
        .collect();
    (video, lines)
}

/// Check one run; every violation is reported, not just the first
fn check(name: &str, latency_us: u64, video: &[Out], lines: &[String]) {
    let mut errors = vec![];
    assert!(video.len() > 3000, "{name}: {} video frames", video.len());
    let lead = |o: &Out| o.wire as i64 - o.wall as i64;

    // 1. Wire time stays near wall time. Once latched, wire − wall changes only by pacer
    //    underruns (behind), pullbacks (ahead) and rebases, which aim at wall time and
    //    never step back. Ahead: a pullback is under a GOP (a larger excess skips
    //    instead), and pulls in at most the latency's worth of queued frames early; plus
    //    a margin. Behind: during an episode every late frame is an underrun, so wire
    //    time falls behind by the camera's arrival lag (timestamps don't move, DESIGN
    //    §4.4) until the catch-up skip or a bridge resume rebases it: the fixture's lag
    //    spread, plus the latency and the margin. Neither depends on how many episodes
    //    came before
    let ahead = latency_us as i64 + GOP_US + 500_000;
    let behind = peak_lag(&fixture()) + latency_us as i64 + 500_000;
    let (max_at, max) = video
        .iter()
        .map(|o| (o.wall, lead(o)))
        .max_by_key(|l| l.1)
        .unwrap();
    let (min_at, min) = video
        .iter()
        .map(|o| (o.wall, lead(o)))
        .min_by_key(|l| l.1)
        .unwrap();
    if max > ahead {
        errors.push(format!(
            "wire time {:.2} s ahead of wall time at {:.1} s (bound {:.2} s)",
            max as f64 / 1e6,
            max_at as f64 / 1e6,
            ahead as f64 / 1e6
        ));
    }
    if -min > behind {
        errors.push(format!(
            "wire time {:.2} s behind wall time at {:.1} s (bound {:.2} s)",
            -min as f64 / 1e6,
            min_at as f64 / 1e6,
            behind as f64 / 1e6
        ));
    }

    // 2. No ratchet: between episodes (10–50 s into each lap: steady arrivals, before the
    //    lap's episode and after the previous one) wire time is back within latency + GOP
    //    of wall time, before the first episode and after each one
    let settled = latency_us as i64 + GOP_US;
    let mut between = vec![];
    for lap in 0..=LAPS {
        let from = (lap * LAP_S + 10) * 1_000_000;
        let to = (lap * LAP_S + 50) * 1_000_000;
        let leads: Vec<i64> = video
            .iter()
            .filter(|o| (from..to).contains(&o.wall))
            .map(lead)
            .collect();
        let worst = leads
            .iter()
            .copied()
            .max_by_key(|l| l.abs())
            .unwrap_or(i64::MAX);
        between.push(format!("{:.2}", worst as f64 / 1e6));
        if worst.abs() > settled {
            errors.push(format!(
                "after {lap} episodes: wire − wall {:.2} s (bound ±{:.2} s)",
                worst as f64 / 1e6,
                settled as f64 / 1e6
            ));
        }
    }

    // 3. SPEC N5: a forward on-wire jump over 1 s (the wire step less the wall step) only
    //    at an IRAP the Hub logged a rebase for, at info level
    let rebased: Vec<u64> = lines
        .iter()
        .filter(|l| l.starts_with("INFO") && l.contains("wire clock rebased"))
        .filter_map(|l| l.rsplit_once("at wire ")?.1.split(' ').next()?.parse().ok())
        .collect();
    let mut largest_unlogged = 0i64;
    let mut jumps = 0;
    for w in video.windows(2) {
        let jump = (w[1].wire as i64 - w[0].wire as i64) - (w[1].wall as i64 - w[0].wall as i64);
        if jump <= JUMP_US {
            continue;
        }
        jumps += 1;
        if !w[1].irap || !rebased.contains(&w[1].wire) {
            largest_unlogged = largest_unlogged.max(jump);
            errors.push(format!(
                "unlogged forward jump of {:.2} s at {:.1} s (irap: {})",
                jump as f64 / 1e6,
                w[1].wall as f64 / 1e6,
                w[1].irap
            ));
        }
    }

    // 4. Within a GOP, frames keep their media spacing (one frame at 15 fps)
    let mut spacing = 0;
    for w in video.windows(2) {
        let d = w[1].wire - w[0].wire;
        if !w[1].irap && !(66_000..=67_500).contains(&d) {
            spacing += 1;
            if spacing <= 5 {
                errors.push(format!(
                    "P-frame {d} µs after the frame before it at {:.1} s",
                    w[1].wall as f64 / 1e6
                ));
            }
        }
    }

    eprintln!(
        "{name}: wire − wall min {:.2} s, max {:.2} s; {jumps} jumps > 1 s, largest unlogged \
         {:.2} s; {} logged rebases > 1 s; {spacing} P-frames off their spacing; between \
         episodes {}",
        min as f64 / 1e6,
        max as f64 / 1e6,
        largest_unlogged as f64 / 1e6,
        rebased.len(),
        between.join(", ")
    );
    assert!(errors.is_empty(), "{name}:\n{}", errors.join("\n"));
}

/// Mains default latency: the episodes' longest arrival gaps bridge, and each resume and
/// catch-up skip rebases. Wire time used to ratchet ahead by ~4 s per episode
#[tokio::test(start_paused = true)]
async fn test_congestion_wire_tracks_wall_300ms() {
    let (video, lines) = run("drift300", 300_000).await;
    check("drift300", 300_000, &video, &lines);
}

/// Battery default latency: no bridge, the catch-up skip rebases
#[tokio::test(start_paused = true)]
async fn test_congestion_wire_tracks_wall_1500ms() {
    let (video, lines) = run("drift1500", 1_500_000).await;
    check("drift1500", 1_500_000, &video, &lines);
}
