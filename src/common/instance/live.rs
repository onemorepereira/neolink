use super::*;

use crate::common::UseCounter;
use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use neolink_core::bc_protocol::StreamKind;

#[cfg(feature = "pushnoti")]
use crate::common::PushNoti;

impl NeoInstance {
    /// Streams a camera source while not paused (`pause.on_motion`)
    ///
    /// Every task this starts belongs to the returned [`StreamRx`] and stops when it is
    /// dropped. Before, the motion and push-notification watchers outlived the client.
    /// `init_permit` streams that long on every start regardless of motion: the GStreamer
    /// backend needs 5 s to negotiate; the native one learns streams itself (`None`).
    /// While paused it emits `StreamEvent::Paused`.
    pub(crate) async fn stream_while_live(
        &self,
        stream: StreamKind,
        init_permit: Option<tokio::time::Duration>,
    ) -> AnyResult<StreamRx> {
        let config = self.config().await?.borrow().clone();
        let name = config.name.clone();

        if !config.pause.on_motion {
            return self.stream(stream).await;
        }
        let (media_tx, media_rx) = tokio::sync::mpsc::channel(100);
        let cancel = CancellationToken::new();
        let counter = UseCounter::new().await;
        let mut handles = vec![];

        let mut md = self.motion().await?;
        let mut tasks = FuturesUnordered::new();
        // Stream for a while on a new client regardless of motion (GStreamer: 5 s, to
        // negotiate the stream type)
        if let Some(d) = init_permit {
            let init_permit = counter.create_activated().await?;
            handles.push(
                tokio::spawn(async move {
                    tokio::time::sleep(d).await;
                    drop(init_permit);
                })
                .abort_handle(),
            );
        }

        // Create the permit for controlling the motion
        let mut md_permit = {
            let md_state = md.borrow_and_update().clone();
            match md_state {
                MdState::Start(_) => {
                    log::info!("{name}::{stream:?}: Starting with Motion");
                    counter.create_activated().await?
                }
                MdState::Stop(_) | MdState::Unknown => {
                    log::info!("{name}::{stream:?}: Waiting with Motion");
                    counter.create_deactivated().await?
                }
            }
        };
        // Now listen to the motion
        let thread_name = name.clone();
        let hold_max = tokio::time::Duration::from_secs(config.motion_hold_max_s);
        let md_task = tokio::spawn(
            async move {
                // A hold ends at Stop, at Unknown (the connection dropped) or after
                // motion_hold_max_s (SPEC P12)
                let mut hold_until: Option<tokio::time::Instant> = None;
                loop {
                    tokio::select! {
                        v = md.changed() => {
                            if let Err(e) = v {
                                break AnyResult::Err(e.into());
                            }
                            let md_state: MdState = md.borrow_and_update().clone();
                            match md_state {
                                MdState::Start(_) => {
                                    log::info!("{thread_name}::{stream:?}: Motion Started");
                                    md_permit.activate().await?;
                                    hold_until = Some(tokio::time::Instant::now() + hold_max);
                                }
                                MdState::Stop(_) | MdState::Unknown => {
                                    log::info!("{thread_name}::{stream:?}: Motion Stopped");
                                    md_permit.deactivate().await?;
                                    hold_until = None;
                                }
                            }
                        }
                        _ = tokio::time::sleep_until(hold_until.unwrap_or_else(tokio::time::Instant::now)), if hold_until.is_some() => {
                            log::info!("{thread_name}::{stream:?}: Motion held the stream for {hold_max:?}, pausing");
                            md_permit.deactivate().await?;
                            hold_until = None;
                        }
                    }
                }?;
                AnyResult::Ok(())
            }
            .map(|e| {
                log::debug!("Motion thread stopped {e:?}");
                e
            }),
        );
        handles.push(md_task.abort_handle());
        tasks.push(md_task);

        #[cfg(feature = "pushnoti")]
        {
            // Creates a permit for controlling based on the PN
            let pn_permit = counter.create_deactivated().await?;
            let mut pn = self.push_notifications().await?;
            pn.borrow_and_update(); // Ignore any PNs that have already been sent before this
            let thread_name = name.clone();
            let pn_task = tokio::spawn(
                async move {
                    loop {
                        let noti: Option<PushNoti> = pn.borrow_and_update().clone();
                        if let Some(noti) = noti {
                            if noti.message.contains("Motion Alert from") {
                                log::info!("{thread_name}::{stream:?}: Push Notification Recieved");
                                let mut new_pn_permit = pn_permit.subscribe();
                                new_pn_permit.activate().await?;
                                tokio::spawn(async move {
                                    tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                                    drop(new_pn_permit);
                                });
                            }
                        }
                        if let Err(e) = pn.changed().await {
                            break Err(e);
                        }
                    }?;
                    AnyResult::Ok(())
                }
                .map(|e| {
                    log::debug!("PN thread stopped {e:?}");
                    e
                }),
            );
            handles.push(pn_task.abort_handle());
            tasks.push(pn_task);
        }

        // Send the camera when the pemit is active
        let camera_permit = counter.create_deactivated().await?;
        let thread_camera = self.clone();
        let thread_cancel = cancel.clone();
        let camera_task = tokio::spawn(async move {
            let r = tokio::select! {
                _ = thread_cancel.cancelled() => AnyResult::Ok(()),
                v = async {
                    loop {
                        camera_permit.aquired_users().await?;
                        log::debug!("Starting stream");
                        tokio::select! {
                            v = camera_permit.dropped_users() => {
                                log::debug!("Dropped users: {v:?}");
                                v
                            },
                            v = async {
                                let mut stream = thread_camera.stream(stream).await?;
                                while let Some(event) = stream.recv_event().await {
                                    media_tx.send(event).await?;
                                }
                                AnyResult::Ok(())
                            } => {
                                log::debug!("Stopped stream: {v:?}");
                                v
                            },
                            v = tasks.next() => {
                                log::debug!("Task failed: {v:?}");
                                Err(anyhow!("Task ended prematurly: {v:?}"))
                            }
                        }?;
                        log::debug!("Pausing stream");
                        if media_tx.send(StreamEvent::Paused).await.is_err() {
                            break AnyResult::Ok(());
                        }
                    }
                } => v,
            };
            // The watchers go with the stream
            for t in tasks.iter() {
                t.abort();
            }
            drop(counter);
            log::debug!("Stream thread stopped {r:?}");
        });
        handles.push(camera_task.abort_handle());

        Ok(StreamRx::new(media_rx, cancel, handles))
    }
}
