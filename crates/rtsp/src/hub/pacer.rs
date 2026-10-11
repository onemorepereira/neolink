//! Pacer: when to release each frame, never what timestamp it carries (DESIGN §4.4)
//!
//! Frames arrive in bursts. Each is released at
//! `due = anchor_wall + (media_us − anchor_media) + latency`. Only release times change:
//! wire timestamps come from the WireClock, so anchor moves never show up on the wire.
//!
//! - The first frame after a start is released at once (the anchor is set so it is due
//!   now); later frames are spaced by media time.
//! - **Underrun** (a frame arrives more than one frame interval after its due time): the
//!   anchor moves forward so it is due on arrival. Latency grows; timestamps don't move.
//! - **Excess latency**: the minimum lead (`due − now` of each newest frame at arrival)
//!   over a 10 s window. If it stays above `latency + 1 frame` for 10 s, the anchor is
//!   pulled back by `min_lead − latency` (a burst of early releases). If it exceeds
//!   `latency + GOP`, queued frames up to the newest IRAP are skipped (media is dropped,
//!   timestamps don't change). This recovers from the Argus's congestion episodes, where
//!   arrivals fall up to 17 s behind and then catch up.
//! - **Latency 0**: release on arrival.
//!
//! Pure and clock-free: the caller passes `now` (µs) so tests run in virtual time.

use std::collections::VecDeque;

/// Something with a place on the media timeline
pub trait Paced {
    fn media_us(&self) -> u64;
    /// A video IRAP (a point the pacer may skip to)
    fn is_irap(&self) -> bool;
}

const WINDOW_US: u64 = 10_000_000;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PacerStats {
    pub underruns: u64,
    pub pullbacks: u64,
    pub skips: u64,
    pub skipped_frames: u64,
}

#[derive(Debug)]
pub struct Pacer<T> {
    latency_us: u64,
    frame_us: u64,
    gop_us: u64,
    /// (wall, media0) such that due = wall + (media − media0) + latency; wall may be
    /// negative early in virtual time
    anchor: Option<(i64, u64)>,
    queue: VecDeque<T>,
    /// (arrival wall, lead) of each newest frame, signed µs
    leads: VecDeque<(u64, i64)>,
    excess_since: Option<u64>,
    pub stats: PacerStats,
}

impl<T: Paced> Pacer<T> {
    pub fn new(latency_us: u64) -> Self {
        Self {
            latency_us,
            frame_us: 66_667,
            gop_us: 2_000_000,
            anchor: None,
            queue: VecDeque::new(),
            leads: VecDeque::new(),
            excess_since: None,
            stats: PacerStats::default(),
        }
    }

    /// Forget the anchor: the next frame is released at once (a source restart, or a
    /// MediaClock discontinuity)
    pub fn restart(&mut self) {
        self.anchor = None;
        self.leads.clear();
        self.excess_since = None;
    }

    fn due(&self, media_us: u64) -> u64 {
        let (wall, media0) = self.anchor.expect("anchored");
        (wall + (media_us as i64 - media0 as i64) + self.latency_us as i64).max(0) as u64
    }

    /// A frame arrived at `now`
    pub fn push(&mut self, now: u64, frame: T) {
        let media = frame.media_us();
        if self.latency_us == 0 {
            self.queue.push_back(frame);
            return;
        }
        match self.anchor {
            // The first frame is due now
            None => self.anchor = Some((now as i64 - self.latency_us as i64, media)),
            Some((wall, media0)) => {
                let due = self.due(media);
                if now > due + self.frame_us {
                    // Underrun: due on arrival
                    self.stats.underruns += 1;
                    self.anchor = Some((wall + (now - due) as i64, media0));
                }
            }
        }
        let lead = self.due(media) as i64 - now as i64;
        self.leads.push_back((now, lead));
        while self
            .leads
            .front()
            .is_some_and(|(t, _)| now.saturating_sub(*t) > WINDOW_US)
        {
            self.leads.pop_front();
        }
        self.queue.push_back(frame);
        self.recover(now);
    }

    fn recover(&mut self, now: u64) {
        let Some(min_lead) = self.leads.iter().map(|(_, l)| *l).min() else {
            return;
        };
        let latency = self.latency_us as i64;
        if min_lead > latency + self.gop_us as i64 {
            // Far behind live: skip to the newest queued IRAP
            if let Some(pos) = self.queue.iter().rposition(|f| f.is_irap()) {
                if pos > 0 {
                    self.stats.skips += 1;
                    self.stats.skipped_frames += pos as u64;
                    self.queue.drain(..pos);
                }
                // That IRAP is due now
                let media = self.queue[0].media_us();
                self.anchor = Some((now as i64 - latency, media));
                self.leads.clear();
                self.excess_since = None;
                return;
            }
        }
        if min_lead > latency + self.frame_us as i64 {
            let since = *self.excess_since.get_or_insert(now);
            if now.saturating_sub(since) >= WINDOW_US {
                let pull = (min_lead - latency) as u64;
                if let Some((wall, media0)) = self.anchor {
                    self.anchor = Some((wall - pull as i64, media0));
                    self.stats.pullbacks += 1;
                }
                for l in self.leads.iter_mut() {
                    l.1 -= pull as i64;
                }
                self.excess_since = None;
            }
        } else {
            self.excess_since = None;
        }
    }

    /// When the next frame is due, if any
    pub fn next_due(&self) -> Option<u64> {
        let f = self.queue.front()?;
        if self.latency_us == 0 {
            return Some(0);
        }
        Some(self.due(f.media_us()))
    }

    /// Frames due at or before `now`, in order
    pub fn pop_due(&mut self, now: u64) -> Vec<T> {
        let mut out = vec![];
        while let Some(due) = self.next_due() {
            if due > now {
                break;
            }
            out.push(self.queue.pop_front().unwrap());
        }
        out
    }

    pub fn queued(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neolink_core_free::*;

    /// Minimal frame for tests
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct F {
        media: u64,
        irap: bool,
    }
    impl Paced for F {
        fn media_us(&self) -> u64 {
            self.media
        }
        fn is_irap(&self) -> bool {
            self.irap
        }
    }

    /// The fixture's video frames as (arrival µs, media µs, irap)
    mod neolink_core_free {
        pub fn fixture(name: &str) -> Option<Vec<(u64, u64, bool)>> {
            let path = format!("{}/../../fixtures/synth/{name}", env!("CARGO_MANIFEST_DIR"));
            let d = std::fs::read(path).ok()?;
            let mut out = vec![];
            let mut i = 0;
            let mut first_cam = None;
            let mut media = 0u64;
            let mut last_cam = 0u32;
            while i + 21 <= d.len() {
                let host = u64::from_le_bytes(d[i..i + 8].try_into().unwrap());
                let kind = d[i + 8] & 0x7f;
                let cam = u32::from_le_bytes(d[i + 9..i + 13].try_into().unwrap());
                let len = u32::from_le_bytes(d[i + 17..i + 21].try_into().unwrap()) as usize;
                i += 21 + len;
                if kind == 3 || kind == 4 {
                    if first_cam.is_some() {
                        media += cam.wrapping_sub(last_cam) as u64;
                    }
                    first_cam.get_or_insert(cam);
                    last_cam = cam;
                    out.push((host, media, kind == 3));
                }
            }
            Some(out)
        }
    }

    /// Replay arrivals into a pacer, returning (release wall, media) per frame
    /// Like the Hub: wake at each due time (or on arrival), release at max(due, now)
    fn replay(p: &mut Pacer<F>, arrivals: &[(u64, u64, bool)]) -> Vec<(u64, u64)> {
        let mut out = vec![];
        let mut now = 0u64;
        for &(at, media, irap) in arrivals {
            while let Some(due) = p.next_due() {
                if due > at {
                    break;
                }
                now = now.max(due);
                for f in p.pop_due(now) {
                    out.push((now, f.media));
                }
            }
            now = now.max(at);
            p.push(now, F { media, irap });
            for f in p.pop_due(now) {
                out.push((now, f.media));
            }
        }
        while let Some(due) = p.next_due() {
            now = now.max(due);
            for f in p.pop_due(now) {
                out.push((now, f.media));
            }
        }
        out
    }

    fn jitter_p99(releases: &[(u64, u64)]) -> u64 {
        let mut j: Vec<u64> = releases
            .windows(2)
            .map(|w| {
                let wall = w[1].0 as i64 - w[0].0 as i64;
                let media = w[1].1 as i64 - w[0].1 as i64;
                (wall - media).unsigned_abs()
            })
            .collect();
        j.sort_unstable();
        j[j.len() * 99 / 100]
    }

    #[test]
    fn test_first_frame_is_released_at_once() {
        let mut p = Pacer::new(1_500_000);
        p.push(
            5_000_000,
            F {
                media: 0,
                irap: true,
            },
        );
        assert_eq!(p.next_due(), Some(5_000_000));
        p.push(
            5_010_000,
            F {
                media: 66_667,
                irap: false,
            },
        );
        assert_eq!(p.pop_due(5_000_000).len(), 1);
        assert_eq!(p.next_due(), Some(5_066_667));
    }

    #[test]
    fn test_argus_like_fixture_is_smooth() {
        let Some(arrivals) = fixture("plain_h265.bcm") else {
            panic!("run `cargo xtask mkfixture --standard --out fixtures/synth` first");
        };
        let mut p = Pacer::new(1_500_000);
        let releases = replay(&mut p, &arrivals);
        assert_eq!(releases.len(), arrivals.len());
        // Steady state: no underruns after the start-up backlog drains
        let steady: Vec<_> = releases.iter().skip(60).cloned().collect();
        let p99 = jitter_p99(&steady);
        assert!(p99 < 10_000, "release jitter p99 {} µs", p99);
        assert!(p.stats.underruns <= 1, "{:?}", p.stats);
    }

    #[test]
    fn test_congestion_recovers_to_latency() {
        let Some(mut arrivals) = fixture("congestion_h265.bcm") else {
            panic!("run `cargo xtask mkfixture --standard --out fixtures/synth` first");
        };
        // The fixture ends while the camera is still catching up: continue with 40 s of
        // steady real-time arrivals at the final lag
        let (last_at, last_media, _) = *arrivals.last().unwrap();
        for i in 1..=600u64 {
            arrivals.push((last_at + i * 66_667, last_media + i * 66_667, i % 30 == 0));
        }
        let mut p = Pacer::new(1_500_000);
        let releases = replay(&mut p, &arrivals);
        // Underruns during the episode, then a skip back towards live
        assert!(p.stats.underruns > 0, "{:?}", p.stats);
        assert!(p.stats.skips + p.stats.pullbacks > 0, "{:?}", p.stats);
        // At the end the delivery delay (release − arrival of the same frame) is back
        // near the latency
        let last = arrivals.len() - 1;
        let arrival_of_last = arrivals[last].0;
        let release_of_last = releases.iter().find(|r| r.1 == arrivals[last].1).unwrap().0;
        let delay = release_of_last as i64 - arrival_of_last as i64;
        if std::env::var_os("PACER_DEBUG").is_some() {
            let mut by_media = std::collections::HashMap::new();
            for (at, m, _) in &arrivals {
                by_media.insert(*m, *at);
            }
            for r in releases.iter().step_by(75) {
                let a = by_media[&r.1];
                eprintln!(
                    "t={:.1}s media={:.1}s arrival_lag={:.2}s delay={:.2}s",
                    r.0 as f64 / 1e6,
                    r.1 as f64 / 1e6,
                    (a as f64 - r.1 as f64) / 1e6,
                    (r.0 as f64 - a as f64) / 1e6
                );
            }
        }
        // Back to the latency, within one frame
        assert!(
            (delay - 1_500_000).abs() <= 70_000,
            "delivery delay at the end {} µs, {:?}",
            delay,
            p.stats
        );
    }

    #[test]
    fn test_latency_zero_releases_on_arrival() {
        let mut p = Pacer::new(0);
        p.push(
            10,
            F {
                media: 0,
                irap: true,
            },
        );
        p.push(
            20,
            F {
                media: 1_000_000,
                irap: false,
            },
        );
        assert_eq!(p.pop_due(20).len(), 2);
    }

    #[test]
    fn test_releases_are_in_order_and_never_stall() {
        // Random-ish burst schedule: all frames come out, in media order
        let mut arrivals = vec![];
        let mut t = 0u64;
        for i in 0..2000u64 {
            t += if i % 30 == 0 {
                900_000
            } else {
                5_000 + (i * 7919 % 50_000)
            };
            arrivals.push((t, i * 66_667, i % 30 == 0));
        }
        let mut p = Pacer::new(1_500_000);
        let r = replay(&mut p, &arrivals);
        let skipped = p.stats.skipped_frames as usize;
        assert_eq!(r.len() + skipped, arrivals.len());
        assert!(r.windows(2).all(|w| w[1].1 > w[0].1 && w[1].0 >= w[0].0));
    }
}
