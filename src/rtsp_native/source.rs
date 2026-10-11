//! A camera stream as a `neolink_rtsp` source (DESIGN §3.2)
//!
//! `Demand` starts honour `pause.on_motion` (`stream_while_live`, with no initial permit:
//! the Hub learns the stream itself); `Learn` starts are plain bounded streams. Events map
//! to `SourceUnit`s:
//!
//! | camera | source |
//! |---|---|
//! | I/P frame | `Video` (`cam_us` = the frame's microseconds) |
//! | AAC | `Aac` |
//! | ADPCM | dropped, warned once (SPEC R3) |
//! | Info | ignored |
//! | `Restarted` | `Restarted` |
//! | `Paused` (no motion) | `State(Paused)`, then `Restarted` + `State(Streaming)` with media |
//! | camera connection lost | `State(Retrying)` until media flows again |

use crate::common::{NeoInstance, StreamEvent};
use bytes::Bytes;
use futures::future::BoxFuture;
use neolink_core::bc_protocol::StreamKind;
use neolink_core::bcmedia::model::{BcMedia, VideoType};
use neolink_rtsp::codec::annexb::VideoCodec;
use neolink_rtsp::source::{
    Availability, SourceState, SourceUnit, StartMode, StartResult, StreamSource,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

pub(crate) struct NativeSource {
    instance: NeoInstance,
    kind: StreamKind,
    name: String,
    /// `pause.on_motion` at Hub creation; the Hub is recreated when it changes
    on_motion: bool,
    availability: watch::Receiver<Availability>,
    adpcm_warned: Arc<AtomicBool>,
}

impl NativeSource {
    pub(crate) fn new(
        instance: NeoInstance,
        kind: StreamKind,
        name: String,
        on_motion: bool,
        availability: watch::Receiver<Availability>,
    ) -> Self {
        Self {
            instance,
            kind,
            name,
            on_motion,
            availability,
            adpcm_warned: Default::default(),
        }
    }
}

impl StreamSource for NativeSource {
    fn start(&self, mode: StartMode, cancel: CancellationToken) -> BoxFuture<'static, StartResult> {
        let instance = self.instance.clone();
        let kind = self.kind;
        let name = self.name.clone();
        let on_motion = self.on_motion && mode == StartMode::Demand;
        let adpcm_warned = self.adpcm_warned.clone();
        Box::pin(async move {
            log::info!("{name}::{kind:?}: camera stream starting ({mode:?})");
            let rx = match mode {
                StartMode::Demand => instance.stream_while_live(kind, None).await,
                StartMode::Learn => instance.stream(kind).await,
            }
            .map_err(|e| format!("{e:#}"))?;
            let (tx, out) = mpsc::channel(128);
            tokio::spawn(pump(
                rx,
                tx,
                cancel,
                instance.camera(),
                on_motion,
                adpcm_warned,
                name,
            ));
            Ok(out)
        })
    }

    fn availability(&self) -> watch::Receiver<Availability> {
        self.availability.clone()
    }

    /// A learning start ignores `pause.on_motion`; demand must honour it
    fn restart_for_demand(&self) -> bool {
        self.on_motion
    }
}

fn codec(v: &VideoType) -> VideoCodec {
    match v {
        VideoType::H264 => VideoCodec::H264,
        VideoType::H265 => VideoCodec::H265,
    }
}

/// Maps one event; `None` for what the Hub doesn't need
pub(crate) fn map_media(
    media: BcMedia,
    adpcm_warned: &AtomicBool,
    name: &str,
) -> Option<SourceUnit> {
    match media {
        BcMedia::Iframe(f) => Some(SourceUnit::Video {
            codec: codec(&f.video_type),
            irap: true,
            cam_us: f.microseconds,
            annexb: Bytes::from(f.data),
        }),
        BcMedia::Pframe(f) => Some(SourceUnit::Video {
            codec: codec(&f.video_type),
            irap: false,
            cam_us: f.microseconds,
            annexb: Bytes::from(f.data),
        }),
        BcMedia::Aac(a) => Some(SourceUnit::Aac {
            adts: Bytes::from(a.data),
        }),
        BcMedia::Adpcm(_) => {
            if !adpcm_warned.swap(true, Ordering::Relaxed) {
                log::warn!(
                    "{name}: the camera sends ADPCM audio, which the native backend does not \
                     carry yet; the stream is served without audio"
                );
            }
            None
        }
        BcMedia::InfoV1(_) | BcMedia::InfoV2(_) => None,
    }
}

async fn pump(
    mut rx: crate::common::StreamRx,
    tx: mpsc::Sender<SourceUnit>,
    cancel: CancellationToken,
    mut camera: watch::Receiver<std::sync::Weak<neolink_core::bc_protocol::BcCamera>>,
    on_motion: bool,
    adpcm_warned: Arc<AtomicBool>,
    name: String,
) {
    let mut state = SourceState::Streaming;
    let mut had_media = false;
    if on_motion {
        // Until motion starts the camera stays asleep: bridging is uncapped
        state = SourceState::Paused;
        if tx.send(SourceUnit::State(state)).await.is_err() {
            return;
        }
    }
    loop {
        let event = tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tx.closed() => break,
            e = rx.recv_event() => match e {
                Some(e) => e,
                None => break, // the stream ended: the Hub retries with backoff
            },
            r = camera.changed() => {
                if r.is_err() {
                    break;
                }
                let lost = camera.borrow_and_update().upgrade().is_none();
                if lost && state == SourceState::Streaming {
                    state = SourceState::Retrying;
                    if tx.send(SourceUnit::State(state)).await.is_err() {
                        break;
                    }
                }
                continue;
            }
        };
        let unit = match event {
            StreamEvent::Restarted => Some(SourceUnit::Restarted),
            StreamEvent::Paused => {
                state = SourceState::Paused;
                Some(SourceUnit::State(state))
            }
            StreamEvent::Media(m) => {
                let Some(unit) = map_media(m, &adpcm_warned, &name) else {
                    continue;
                };
                if state != SourceState::Streaming {
                    // A motion resume is a new camera stream: a discontinuity
                    if state == SourceState::Paused
                        && had_media
                        && tx.send(SourceUnit::Restarted).await.is_err()
                    {
                        break;
                    }
                    state = SourceState::Streaming;
                    if tx.send(SourceUnit::State(state)).await.is_err() {
                        break;
                    }
                }
                had_media = true;
                Some(unit)
            }
        };
        if let Some(unit) = unit {
            if tx.send(unit).await.is_err() {
                break;
            }
        }
    }
    // Dropping `rx` stops the camera stream and releases its permit
    log::debug!("{name}: camera stream stopped");
}
