//! The source boundary (DESIGN §3.1): the only input of the RTSP server
//!
//! A source delivers camera media for one camera stream. The Hub starts it on demand and
//! stops it only by cancelling the token it passed.

use crate::codec::annexb::VideoCodec;
use bytes::Bytes;
use futures::future::BoxFuture;
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
        }
    }
}

impl StreamSource for FixtureSource {
    fn start(&self, mode: StartMode, cancel: CancellationToken) -> BoxFuture<'static, StartResult> {
        self.starts.lock().unwrap().push(mode);
        let units = self.units.clone();
        let speed = self.speed;
        let running = self.running.clone();
        Box::pin(async move {
            let (tx, rx) = mpsc::channel(256);
            running.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                let start = tokio::time::Instant::now();
                let first = units.first().map(|u| u.at_us).unwrap_or(0);
                for u in units.iter() {
                    let at = std::time::Duration::from_micros(
                        ((u.at_us - first) as f64 / speed.max(1e-6)) as u64,
                    );
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = tokio::time::sleep_until(start + at) => {}
                    }
                    if tx.send(u.unit.clone()).await.is_err() {
                        break;
                    }
                }
                // Hold the stream open until cancelled, like a camera with nothing new
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
