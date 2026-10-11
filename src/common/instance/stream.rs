//! Camera streams that stop as soon as their consumer goes away (PLAN 1.3)
//!
//! `NeoInstance::stream` runs one task that holds the camera permit while it streams.
//! Dropping the returned [`StreamRx`] cancels and aborts that task at once, so the permit
//! is released immediately and not when the next frame fails to send, which never
//! happens if the camera has stalled.

use super::*;
use neolink_core::{bc_protocol::StreamKind, bcmedia::model::BcMedia};
use tokio::sync::mpsc::Receiver as MpscReceiver;
use tokio::task::AbortHandle;

/// What a stream delivers
#[derive(Debug)]
pub(crate) enum StreamEvent {
    /// A frame from the camera
    Media(BcMedia),
    /// The camera stream started again (a reconnect); timestamps may jump
    Restarted,
}

/// The receiving end of a camera stream; dropping it stops the stream
pub(crate) struct StreamRx {
    rx: MpscReceiver<StreamEvent>,
    cancel: CancellationToken,
    tasks: Vec<AbortHandle>,
}

impl StreamRx {
    pub(crate) fn new(
        rx: MpscReceiver<StreamEvent>,
        cancel: CancellationToken,
        tasks: Vec<AbortHandle>,
    ) -> Self {
        Self { rx, cancel, tasks }
    }

    /// The next frame, skipping other events; `None` when the stream ended
    pub(crate) async fn recv(&mut self) -> Option<BcMedia> {
        loop {
            match self.rx.recv().await? {
                StreamEvent::Media(m) => return Some(m),
                _ => continue,
            }
        }
    }

    /// [`Self::recv`] for a non-async thread
    pub(crate) fn blocking_recv(&mut self) -> Option<BcMedia> {
        loop {
            match self.rx.blocking_recv()? {
                StreamEvent::Media(m) => return Some(m),
                _ => continue,
            }
        }
    }

    /// The next event; `None` when the stream ended
    #[allow(dead_code)]
    pub(crate) async fn recv_event(&mut self) -> Option<StreamEvent> {
        self.rx.recv().await
    }
}

impl Drop for StreamRx {
    fn drop(&mut self) {
        self.cancel.cancel();
        for t in self.tasks.drain(..) {
            t.abort();
        }
    }
}

impl NeoInstance {
    /// Streams a camera source until the returned receiver is dropped
    pub(crate) async fn stream(&self, stream: StreamKind) -> AnyResult<StreamRx> {
        let (media_tx, media_rx) = tokio::sync::mpsc::channel(100);
        let config = self.config().await?.borrow().clone();
        let strict = config.strict;
        let thread_camera = self.clone();
        let cancel = CancellationToken::new();
        let thread_cancel = cancel.clone();
        let task = tokio::task::spawn(async move {
            let starts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
            let reason = format!("stream:{stream:?}");
            let res = tokio::select! {
                _ = thread_cancel.cancelled() => Ok(()),
                v = thread_camera.run_stream_task(&reason, move |cam| {
                    let media_tx = media_tx.clone();
                    let starts = starts.clone();
                    Box::pin(async move {
                        let mut media_stream = cam.start_video(stream, 0, strict).await?;
                        log::trace!("Camera started");
                        if starts.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 0 {
                            media_tx.send(StreamEvent::Restarted).await?;
                        }
                        while let Ok(media) = media_stream.get_data().await? {
                            media_tx.send(StreamEvent::Media(media)).await?;
                        }
                        AnyResult::Ok(())
                    })
                }) => v,
            };
            log::debug!("Camera finished streaming: {res:?}");
        });
        // SPEC P8: say so once if a battery camera streams for 30 min straight
        let warn = config.sleeps().then(|| {
            let name = config.name.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30 * 60)).await;
                log::warn!(
                    "{name} has streamed continuously for 30 min; battery cameras are not designed for continuous recording"
                );
            })
            .abort_handle()
        });
        let mut tasks = vec![task.abort_handle()];
        tasks.extend(warn);
        Ok(StreamRx::new(media_rx, cancel, tasks))
    }
}
