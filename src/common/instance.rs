//! The sharable instance
//!
//! This communicaes with the [`NeoCam`] over channels
//!
//! The camera watch is used as an event to be triggered
//! whenever the camera is lost/updated
use anyhow::{anyhow, Context};
use futures::TryFutureExt;
use std::sync::{Arc, Weak};
use tokio::{
    sync::{
        mpsc::Sender as MpscSender, oneshot::channel as oneshot, watch::Receiver as WatchReceiver,
    },
    time::{sleep, Duration},
};
use tokio_util::sync::CancellationToken;

use super::{MdState, NeoCamCommand, NeoCamThreadState, Permit};
use crate::{config::CameraConfig, AnyResult, Result};
use neolink_core::bc_protocol::BcCamera;

#[cfg(feature = "gstreamer")]
mod gst;

/// How long a [`NeoInstance::run_task`] may take before it fails (SPEC P11)
pub(crate) const TASK_DEADLINE: Duration = Duration::from_secs(60);

#[cfg(feature = "pushnoti")]
mod pushnoti;

/// This instance is the primary interface used throughout the app
///
/// It uses channels to run all tasks on the actual shared `[NeoCam]`
#[derive(Clone)]
pub(crate) struct NeoInstance {
    camera_watch: WatchReceiver<Weak<BcCamera>>,
    camera_control: MpscSender<NeoCamCommand>,
    cancel: CancellationToken,
}

impl NeoInstance {
    pub(crate) fn new(
        camera_watch: WatchReceiver<Weak<BcCamera>>,
        camera_control: MpscSender<NeoCamCommand>,
        cancel: CancellationToken,
    ) -> Result<Self> {
        Ok(Self {
            camera_watch,
            camera_control,
            cancel,
        })
    }

    /// Create a new instance to the same camera
    ///
    /// Unlike clone this one will contact the NeoCam and grab it from
    /// there. There is no real benifit to this, other then being
    /// able to check if the thread is alive. Which is why it can
    /// fail.
    pub(crate) async fn subscribe(&self) -> Result<Self> {
        let (instance_tx, instance_rx) = oneshot();
        self.camera_control
            .send(NeoCamCommand::Instance(instance_tx))
            .await?;
        instance_rx.await?
    }

    /// This is a helpful convience function
    ///
    /// Given an async task it will:
    /// - Run the task with a reference to a BcCamera
    /// - If the camera instance is changed: Rerun the task with the new instance
    /// - If the camera returns a retryable error, wait for camera instance to change then rerun
    /// - else return the result of the function
    ///
    /// The caller's source location is the permit's reason (SPEC P6), and the task fails
    /// after [`TASK_DEADLINE`] (SPEC P11). Use [`Self::run_task_for`] to name the reason or
    /// [`Self::run_stream_task`] for long-running streams.
    #[track_caller]
    pub(crate) fn run_task<'s, F, T>(
        &'s self,
        task: F,
    ) -> impl futures::Future<Output = AnyResult<T>> + 's
    where
        F: for<'a> Fn(
                &'a BcCamera,
            )
                -> std::pin::Pin<Box<dyn futures::Future<Output = AnyResult<T>> + Send + 'a>>
            + 's,
        T: 's,
    {
        let caller = std::panic::Location::caller();
        let reason = format!("task:{}:{}", caller.file(), caller.line());
        async move { self.run_task_for(&reason, task).await }
    }

    /// [`Self::run_task`] with an explicit reason
    pub(crate) async fn run_task_for<F, T>(&self, reason: &str, task: F) -> AnyResult<T>
    where
        F: for<'a> Fn(
            &'a BcCamera,
        )
            -> std::pin::Pin<Box<dyn futures::Future<Output = AnyResult<T>> + Send + 'a>>,
    {
        let _permit = self.permit(reason).await?;
        tokio::time::timeout(TASK_DEADLINE, self.run_passive_task(task))
            .await
            .map_err(|_| anyhow!("{reason}: no result within {TASK_DEADLINE:?}"))?
    }

    /// Like [`Self::run_task_for`] but without a deadline, for streams that end when their
    /// consumer goes away
    pub(crate) async fn run_stream_task<F, T>(&self, reason: &str, task: F) -> AnyResult<T>
    where
        F: for<'a> Fn(
            &'a BcCamera,
        )
            -> std::pin::Pin<Box<dyn futures::Future<Output = AnyResult<T>> + Send + 'a>>,
    {
        let _permit = self.permit(reason).await?;
        self.run_passive_task(task).await
    }

    /// This is a helpful convience function
    ///
    /// Given an async task it will:
    /// - Run the task with a reference to a BcCamera
    /// - If the camera instance is changed: Rerun the task with the new instance
    /// - If the camera returns a retryable error, wait for camera instance to change then rerun
    /// - else return the result of the function
    ///
    /// This variant does NOT take out a use permit so the camera can disconnect
    /// for inactvitity during its call. It is meant to be used for non-critial
    /// background tasks that we want to stop during certain times like low battery
    ///
    /// The streams and MD use this
    pub(crate) async fn run_passive_task<F, T>(&self, task: F) -> AnyResult<T>
    where
        F: for<'a> Fn(
            &'a BcCamera,
        )
            -> std::pin::Pin<Box<dyn futures::Future<Output = AnyResult<T>> + Send + 'a>>,
    {
        let mut camera_watch = self.camera_watch.clone();
        let mut camera = None;

        loop {
            camera = camera_watch
                .wait_for(|new_cam| {
                    let curr_as_weak = camera.as_ref().map(Arc::downgrade).unwrap_or_default();
                    !Weak::ptr_eq(new_cam, &curr_as_weak)
                })
                .map_ok(|new_cam| new_cam.upgrade())
                .await
                .with_context(|| "Camera is disconnecting")?;
            break tokio::select! {
                _ = self.cancel.cancelled() => {
                    Err(anyhow!("Camera is disconnecting"))
                }
                _ = camera_watch.wait_for(|new_cam| !Weak::ptr_eq(new_cam, &camera.as_ref().map(Arc::downgrade).unwrap_or_default())).map_ok(|new_cam| new_cam.upgrade()) => {
                    // Camera value has changed!
                    // Go back and see how it changed
                    continue;
                },
                v = async {
                    if let Some(cam) = camera.clone() {
                        let cam_ref = cam.as_ref();
                        let mut r = Err(anyhow!("No run"));
                        for i in 0..5 {
                            r = task(cam_ref).await;
                            if let Err(e) = &r {
                                log::debug!("- Task Error: {e:?}");
                            }
                            if let Err(Some(e @ neolink_core::Error::CameraServiceUnavailable{code: 400, ..})) = r.as_ref().map_err(|e| e.downcast_ref::<neolink_core::Error>()) {
                                // Retryable without a reconnect
                                // Usually occurs when camera is starting up
                                // or the connection is initialising
                                log::debug!("Got a 400 code for {e:?} retry {i}/5, ");

                                sleep(Duration::from_secs(1)).await;
                                continue;
                            } else {
                                break;
                            }
                        }
                        r
                    } else {
                        unreachable!()
                    }
                }, if camera.is_some() => {
                    match v {
                        // Ok means we are done
                        Ok(v) => Ok(v),
                        // If error we check for retryable errors
                        Err(e) => {
                            match e.downcast::<neolink_core::Error>() {
                                Ok(neolink_core::Error::DroppedConnection) | Ok(neolink_core::Error::TimeoutDisconnected) | Ok(neolink_core::Error::DroppedSubscriber) => {
                                    // The connection went away mid-request: wait for the
                                    // next camera and run it again
                                    continue;
                                },
                                Ok(neolink_core::Error::TokioBcSendError) => {
                                    continue;
                                },
                                Ok(neolink_core::Error::Io(e)) => {
                                    use std::io::ErrorKind::*;
                                    if let ConnectionReset | ConnectionAborted | BrokenPipe | TimedOut =  e.kind() {
                                        // Resetable IO
                                        log::trace!("    - Neolink Std IO Error: Continue");
                                        continue;
                                    } else {
                                        // Check if  the inner error is the Other type and then the discomnect
                                        let is_dropped = e.get_ref().is_some_and(|e| {
                                            log::trace!("Std IO Error: Inner: {:?}", e);
                                            matches!(e.downcast_ref::<neolink_core::Error>(),
                                                    Some(neolink_core::Error::DroppedConnection) | Some(neolink_core::Error::TimeoutDisconnected)
                                            )
                                        });
                                        if is_dropped {
                                            // Retry is a None
                                            log::trace!("    - Neolink Std IO Error => Neolink: Continue");
                                            continue;
                                        } else {
                                            log::trace!("    - Neolink Std IO Error: Other");
                                            Err(e.into())
                                        }
                                    }
                                }
                                Ok(e) => {
                                    log::trace!("  - Neolink Error: Other");
                                    Err(e.into())
                                },
                                Err(e) => {
                                    // Check if it is an io error
                                    log::trace!("  - Other Error: {:?}", e);
                                    match e.downcast::<std::io::Error>() {
                                        Ok(e) => {
                                            log::trace!("    - Std IO Error");
                                            // Check if  the inner error is the Other type and then the discomnect
                                            use std::io::ErrorKind::*;
                                            if let ConnectionReset | ConnectionAborted | BrokenPipe | TimedOut =  e.kind() {
                                                // Resetable IO
                                                log::trace!("      - Std IO Error: Continue");
                                                continue;
                                            } else {
                                                let is_dropped = e.get_ref().is_some_and(|e| {
                                                    log::trace!("Std IO Error: Inner: {:?}", e);
                                                    matches!(e.downcast_ref::<neolink_core::Error>(),
                                                            Some(neolink_core::Error::DroppedConnection) | Some(neolink_core::Error::TimeoutDisconnected) | Some(neolink_core::Error::TokioBcSendError)
                                                    )
                                                });
                                                if is_dropped {
                                                    // Retry is a None
                                                    log::trace!("      - Std IO Error => Neolink Error: Continue");
                                                    continue;
                                                } else {
                                                    log::trace!("      - Std IO Error: Other");
                                                    Err(e.into())
                                                }
                                            }
                                        },
                                        Err(e) => {
                                            log::trace!("  - Other Error: {:?}", e);
                                            Err(e)
                                        }
                                    }
                                },
                            }
                        }
                    }
                },
            };
        }
    }

    pub(crate) async fn motion(&self) -> Result<WatchReceiver<MdState>> {
        let (instance_tx, instance_rx) = oneshot();
        self.camera_control
            .send(NeoCamCommand::Motion(instance_tx))
            .await?;
        Ok(instance_rx.await?)
    }

    pub(crate) async fn config(&self) -> Result<WatchReceiver<CameraConfig>> {
        let (instance_tx, instance_rx) = oneshot();
        self.camera_control
            .send(NeoCamCommand::Config(instance_tx))
            .await?;
        Ok(instance_rx.await?)
    }

    /// Wait for the next connection: a different camera than the current one, or any
    /// camera if there is none now. Never causes a connection
    pub(crate) async fn next_connection(&self) -> Result<()> {
        let mut watch = self.camera_watch.clone();
        let current = watch.borrow_and_update().clone();
        watch
            .wait_for(|cam| !Weak::ptr_eq(cam, &current) && cam.upgrade().is_some())
            .await?;
        Ok(())
    }

    pub(crate) fn camera(&self) -> WatchReceiver<Weak<BcCamera>> {
        self.camera_watch.clone()
    }

    pub(crate) async fn connect(&self) -> Result<()> {
        let (instance_tx, instance_rx) = oneshot();
        self.camera_control
            .send(NeoCamCommand::Connect(instance_tx))
            .await?;
        Ok(instance_rx.await?)
    }

    pub(crate) async fn disconnect(&self) -> Result<()> {
        let (instance_tx, instance_rx) = oneshot();
        self.camera_control
            .send(NeoCamCommand::Disconnect(instance_tx))
            .await?;
        Ok(instance_rx.await?)
    }

    #[allow(dead_code)]
    pub(crate) async fn get_state(&self) -> Result<NeoCamThreadState> {
        let (instance_tx, instance_rx) = oneshot();
        self.camera_control
            .send(NeoCamCommand::State(instance_tx))
            .await?;
        Ok(instance_rx.await?)
    }

    /// A permit keeps the camera connected while held. `reason` is recorded in the wake
    /// ledger (SPEC P6)
    pub(crate) async fn permit(&self, reason: &str) -> Result<Permit> {
        let (instance_tx, instance_rx) = oneshot();
        self.camera_control
            .send(NeoCamCommand::GetPermit(reason.to_string(), instance_tx))
            .await?;
        Ok(instance_rx.await?)
    }

    pub(crate) fn drop_command<F>(self, task: F, timeout: tokio::time::Duration) -> DropRunTask<F>
    where
        F: for<'a> Fn(
                &'a BcCamera,
            )
                -> std::pin::Pin<Box<dyn futures::Future<Output = Result<()>> + Send + 'a>>
            + Send
            + Sync
            + 'static,
    {
        DropRunTask {
            instance: Some(self),
            command: Some(Box::new(task)),
            timeout,
        }
    }
}

// A task that is run on a camera when the structure is dropped
pub(crate) struct DropRunTask<F>
where
    F: for<'a> Fn(
            &'a BcCamera,
        )
            -> std::pin::Pin<Box<dyn futures::Future<Output = Result<()>> + Send + 'a>>
        + Send
        + Sync
        + 'static,
{
    instance: Option<NeoInstance>,
    command: Option<Box<F>>,
    timeout: tokio::time::Duration,
}

impl<F> Drop for DropRunTask<F>
where
    F: for<'a> Fn(
            &'a BcCamera,
        )
            -> std::pin::Pin<Box<dyn futures::Future<Output = Result<()>> + Send + 'a>>
        + Send
        + Sync
        + 'static,
{
    fn drop(&mut self) {
        if let (Some(command), Some(instance)) = (self.command.take(), self.instance.take()) {
            let _gt = tokio::runtime::Handle::current().enter();
            let timeout = self.timeout;
            tokio::task::spawn(async move {
                tokio::time::timeout(timeout, instance.run_passive_task(*command)).await??;
                crate::AnyResult::Ok(())
            });
        }
    }
}
