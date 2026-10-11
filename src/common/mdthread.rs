//! This thread will listen to motion messages
//! from the camera.

use anyhow::Context;
use std::sync::Arc;
use tokio::{
    sync::{
        mpsc::Receiver as MpscReceiver,
        oneshot::Sender as OneshotSender,
        watch::{channel as watch, Receiver as WatchReceiver, Sender as WatchSender},
    },
    time::{sleep, Duration, Instant},
};
use tokio_util::sync::CancellationToken;

use super::NeoInstance;
use crate::{AnyResult, Result};
use neolink_core::bc_protocol::MotionStatus;

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) enum MdState {
    Start(Instant),
    Stop(Instant),
    Unknown,
}

pub(crate) struct NeoCamMdThread {
    md_watcher: Arc<WatchSender<MdState>>,
    md_request_rx: MpscReceiver<MdRequest>,
    cancel: CancellationToken,
    instance: NeoInstance,
}

impl NeoCamMdThread {
    pub(crate) async fn new(
        md_request_rx: MpscReceiver<MdRequest>,
        instance: NeoInstance,
    ) -> Result<Self> {
        let (md_watcher, _) = watch(MdState::Unknown);
        let md_watcher = Arc::new(md_watcher);
        Ok(Self {
            md_watcher,
            md_request_rx,
            cancel: CancellationToken::new(),
            instance,
        })
    }

    pub(crate) async fn run(&mut self) -> Result<()> {
        let thread_cancel = self.cancel.clone();
        let watcher = self.md_watcher.clone();
        let md_instance = self.instance.clone();
        let reset_watcher = self.md_watcher.clone();
        let mut camera_watch = self.instance.camera();
        tokio::select! {
            _ = thread_cancel.cancelled() => {
                Ok(())
            },
            v = async {
                while let Some(request) = self.md_request_rx.recv().await {
                    match request {
                        MdRequest::Get {
                            sender
                        } => {
                          let _ = sender.send(self.md_watcher.subscribe());
                        },
                    }
                }
                Ok(())
            } => v,
            v = async {
                loop {
                    let r: AnyResult<()> = md_instance.run_passive_task(|cam| {
                        let watcher = watcher.clone();
                        Box::pin(
                        async move {
                            let mut md = cam.listen_on_motion().await.with_context(|| "Error in getting MD listen_on_motion")?;
                            loop {
                                let event = md.next_motion().await.with_context(|| "Error in getting MD next_motion")?;
                                match event {
                                    MotionStatus::Start(at) => {
                                        watcher.send_replace(
                                            MdState::Start(at.into())
                                        );
                                    }
                                    MotionStatus::Stop(at) => {
                                        watcher.send_replace(
                                            MdState::Stop(at.into())
                                        );
                                    }
                                    MotionStatus::NoChange(_) => {},
                                }
                            }
                        }
                    )}).await;
                    log::debug!("Error in MD task Restarting: {:?}", r);
                    sleep(Duration::from_secs(1)).await;
                }
            } => v,
            // Motion is unknown while disconnected: a Start from before must not keep a
            // motion permit (and the camera) alive after a drop (SPEC P12)
            v = async {
                loop {
                    // Any change of connection (gone, or replaced by a quick reconnect
                    // that this may not see as "gone") ends the old connection's motion
                    let current = camera_watch.borrow_and_update().clone();
                    camera_watch
                        .wait_for(|cam| !std::sync::Weak::ptr_eq(cam, &current))
                        .await?;
                    if current.upgrade().is_some() || camera_watch.borrow().upgrade().is_none() {
                        reset_watcher.send_replace(MdState::Unknown);
                    }
                }
            } => v,
        }
    }
}

impl Drop for NeoCamMdThread {
    fn drop(&mut self) {
        log::trace!("Drop NeoCamMdThread");
        self.cancel.cancel();
        log::trace!("Dropped NeoCamMdThread");
    }
}

/// Used to pass messages to the MdThread
pub(crate) enum MdRequest {
    Get {
        sender: OneshotSender<WatchReceiver<MdState>>,
    },
}
