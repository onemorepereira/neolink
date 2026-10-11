//! The source boundary (DESIGN §3.1): the only input of the RTSP server
//!
//! A source delivers camera media for one camera stream. The Hub starts it on demand and
//! stops it only by cancelling the token it passed.

use crate::codec::annexb::VideoCodec;
use bytes::Bytes;
use futures::future::BoxFuture;
use std::sync::atomic::Ordering::SeqCst;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

pub use crate::hub::wire::SourceState;

/// Why the source is started
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartMode {
    /// A session is playing; honour `pause.on_motion`
    Demand,
    /// A bounded learning start for a desc cache miss; ignores `pause.on_motion`
    Learn,
}

/// What a source sends
#[derive(Debug, Clone)]
pub enum SourceUnit {
    /// One access unit, Annex-B
    Video {
        codec: VideoCodec,
        irap: bool,
        cam_us: u32,
        annexb: Bytes,
    },
    /// One ADTS frame
    Aac { adts: Bytes },
    /// The camera stream was re-established (not sent on the first start)
    Restarted,
    /// Why media is or isn't flowing
    State(SourceState),
}

/// Whether to serve the stream at all, and whether waking the camera is costly.
/// `battery: None` (unknown) is treated as battery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Availability {
    pub supported: Option<bool>,
    pub battery: Option<bool>,
}

pub type StartResult = Result<mpsc::Receiver<SourceUnit>, String>;

pub trait StreamSource: Send + Sync + 'static {
    /// Start camera media. The source MUST stop and release every camera resource
    /// promptly when `cancel` fires.
    fn start(&self, mode: StartMode, cancel: CancellationToken) -> BoxFuture<'static, StartResult>;
    /// Watched; changes apply live
    fn availability(&self) -> watch::Receiver<Availability>;
}

// ---------------------------------------------------------------------------------------
// Fixture source (PLAN 2.9)

/// One `.bcm` record as the fixture source needs it
#[derive(Debug, Clone)]
pub struct FixtureUnit {
    /// Arrival time from the start, µs
    pub at_us: u64,
    pub unit: SourceUnit,
}

/// Parse a `.bcm` file (`neolink_core::fixture` format) without depending on neolink_core
pub fn parse_bcm(data: &[u8]) -> Vec<FixtureUnit> {
    let mut out = vec![];
    let mut i = 0;
    while i + 21 <= data.len() {
        let at_us = u64::from_le_bytes(data[i..i + 8].try_into().unwrap());
        let kind = data[i + 8];
        let cam_us = u32::from_le_bytes(data[i + 9..i + 13].try_into().unwrap());
        let len = u32::from_le_bytes(data[i + 17..i + 21].try_into().unwrap()) as usize;
        let start = i + 21;
        if start + len > data.len() {
            break;
        }
        let payload = Bytes::copy_from_slice(&data[start..start + len]);
        i = start + len;
        let codec = if kind & 0x80 != 0 {
            VideoCodec::H265
        } else {
            VideoCodec::H264
        };
        let unit = match kind & 0x7f {
            3 | 4 => SourceUnit::Video {
                codec,
                irap: kind & 0x7f == 3,
                cam_us,
                annexb: payload,
            },
            5 => SourceUnit::Aac { adts: payload },
            0x7f => SourceUnit::Restarted,
            _ => continue,
        };
        out.push(FixtureUnit { at_us, unit });
    }
    out
}

/// Replays fixture units with their original timing (scaled by `speed`), from the start
/// each time it is started. Works under `tokio::time::pause` for virtual-time tests.
pub struct FixtureSource {
    units: std::sync::Arc<Vec<FixtureUnit>>,
    speed: f64,
    availability: watch::Sender<Availability>,
    /// How many times it was started, by mode
    pub starts: std::sync::Arc<std::sync::Mutex<Vec<StartMode>>>,
    /// Sources currently running
    pub running: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Fail this many of the next starts
    pub fail_starts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Close the stream at the end of the fixture instead of holding it open
    pub end_at_eof: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Replay the fixture forever with a continuous camera clock (long client tests)
    pub looping: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// While set, nothing is sent and the replay clock stands still (a stalled camera)
    pub stalled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Simulated wake time: each start takes this long, in ms, before media can flow
    pub start_delay_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl FixtureSource {
    pub fn new(units: Vec<FixtureUnit>, speed: f64, battery: Option<bool>) -> Self {
        Self {
            units: std::sync::Arc::new(units),
            speed,
            availability: watch::channel(Availability {
                supported: Some(true),
                battery,
            })
            .0,
            starts: Default::default(),
            running: Default::default(),
            fail_starts: Default::default(),
            end_at_eof: Default::default(),
            looping: Default::default(),
            stalled: Default::default(),
            start_delay_ms: Default::default(),
        }
    }
}

impl StreamSource for FixtureSource {
    fn start(&self, mode: StartMode, cancel: CancellationToken) -> BoxFuture<'static, StartResult> {
        self.starts.lock().unwrap().push(mode);
        let units = self.units.clone();
        let speed = self.speed;
        let running = self.running.clone();
        let mut fail = false;
        let mut n = self.fail_starts.load(SeqCst);
        while n > 0 {
            match self.fail_starts.compare_exchange(n, n - 1, SeqCst, SeqCst) {
                Ok(_) => {
                    fail = true;
                    break;
                }
                Err(v) => n = v,
            }
        }
        let end_at_eof = self.end_at_eof.load(SeqCst);
        let looping = self.looping.load(SeqCst);
        let stalled = self.stalled.clone();
        let delay = std::time::Duration::from_millis(self.start_delay_ms.load(SeqCst));
        Box::pin(async move {
            if fail {
                return Err("fixture: start failed".to_string());
            }
            tokio::select! {
                _ = cancel.cancelled() => return Err("fixture: cancelled while waking".into()),
                _ = tokio::time::sleep(delay) => {}
            }
            let (tx, rx) = mpsc::channel(256);
            running.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                let mut start = tokio::time::Instant::now();
                let first = units.first().map(|u| u.at_us).unwrap_or(0);
                // Each lap moves both clocks by the media span, so wall time and media
                // time advance together; audio is topped up to the same length
                let period = lap_period(&units);
                let (at_period, cam_period) = (period, period);
                let last_adts = units.iter().rev().find_map(|u| match &u.unit {
                    SourceUnit::Aac { adts } => Some(adts.clone()),
                    _ => None,
                });
                let au_us = last_adts
                    .as_ref()
                    .and_then(|a| crate::codec::aac::AdtsHeader::parse(a).ok())
                    .map(|h| 1024 * 1_000_000 / h.sample_rate().max(1) as u64)
                    .unwrap_or(0);
                let mut audio_us = 0u64;
                let mut lap = 0u64;
                'replay: loop {
                    for u in units.iter() {
                        let rel = u.at_us.saturating_sub(first) + lap * at_period;
                        let at =
                            std::time::Duration::from_micros((rel as f64 / speed.max(1e-6)) as u64);
                        tokio::select! {
                            _ = cancel.cancelled() => break 'replay,
                            _ = tokio::time::sleep_until(start + at) => {}
                        }
                        while stalled.load(SeqCst) {
                            let step = std::time::Duration::from_millis(50);
                            tokio::select! {
                                _ = cancel.cancelled() => break 'replay,
                                _ = tokio::time::sleep(step) => start += step,
                            }
                        }
                        let mut unit = u.unit.clone();
                        if let SourceUnit::Video { cam_us, .. } = &mut unit {
                            *cam_us = cam_us.wrapping_add((lap * cam_period) as u32);
                        }
                        if lap > 0 && matches!(unit, SourceUnit::Restarted) {
                            continue;
                        }
                        if matches!(unit, SourceUnit::Aac { .. }) {
                            audio_us += au_us;
                        }
                        if tx.send(unit).await.is_err() {
                            break 'replay;
                        }
                    }
                    if !looping {
                        break;
                    }
                    lap += 1;
                    if let Some(adts) = &last_adts {
                        while au_us > 0 && audio_us + au_us / 2 < lap * period {
                            audio_us += au_us;
                            if tx
                                .send(SourceUnit::Aac { adts: adts.clone() })
                                .await
                                .is_err()
                            {
                                break 'replay;
                            }
                        }
                    }
                }
                // Hold the stream open until cancelled, like a camera with nothing new
                if end_at_eof {
                    drop(tx);
                }
                cancel.cancelled().await;
                running.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            });
            Ok(rx)
        })
    }

    fn availability(&self) -> watch::Receiver<Availability> {
        self.availability.subscribe()
    }
}

/// How far each lap of a looped fixture moves the clocks: the video span on the camera
/// clock plus one mean frame interval (the arrival span is shorter: a start-up burst)
fn lap_period(units: &[FixtureUnit]) -> u64 {
    let cams: Vec<u32> = units
        .iter()
        .filter_map(|u| match &u.unit {
            SourceUnit::Video { cam_us, .. } => Some(*cam_us),
            _ => None,
        })
        .collect();
    if cams.len() < 2 {
        let at = match (units.first(), units.last()) {
            (Some(a), Some(b)) => b.at_us.saturating_sub(a.at_us),
            _ => 0,
        };
        return at.max(1);
    }
    let span = cams.last().unwrap().wrapping_sub(cams[0]) as u64;
    span + span / (cams.len() as u64 - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A looped fixture keeps audio and video the same length lap after lap, and its
    /// arrivals keep pace with media time (no drift for A16, no jump for the pacer)
    #[tokio::test(start_paused = true)]
    async fn test_looping_keeps_tracks_and_wall_time_together() {
        let path = format!(
            "{}/../../fixtures/synth/plain_h265.bcm",
            env!("CARGO_MANIFEST_DIR")
        );
        let units = parse_bcm(&std::fs::read(&path).expect("cargo xtask mkfixture --standard"));
        let src = FixtureSource::new(units, 1.0, Some(false));
        src.looping.store(true, SeqCst);
        let mut rx = src
            .start(StartMode::Demand, CancellationToken::new())
            .await
            .unwrap();
        let t0 = tokio::time::Instant::now();
        let (mut frames, mut aus) = (0u64, 0u64);
        let mut last_cam: Option<u32> = None;
        while t0.elapsed() < std::time::Duration::from_secs(65) {
            match rx.recv().await.unwrap() {
                SourceUnit::Video { cam_us, .. } => {
                    if let Some(l) = last_cam {
                        let d = cam_us.wrapping_sub(l);
                        assert!((60_000..=75_000).contains(&d), "camera step {d} µs");
                    }
                    last_cam = Some(cam_us);
                    frames += 1;
                }
                SourceUnit::Aac { .. } => aus += 1,
                _ => {}
            }
        }
        let video_us = frames * 1_000_000 / 15;
        let audio_us = aus * 64_000;
        assert!(
            video_us.abs_diff(audio_us) <= 2 * 64_000,
            "video {video_us} µs vs audio {audio_us} µs after 3 laps"
        );
        // Media sent by 65 s of wall time: at most the 1.9 s start-up burst ahead
        assert!(
            video_us.abs_diff(65_000_000) <= 2_500_000,
            "{video_us} µs of video in 65 s"
        );
    }
}
