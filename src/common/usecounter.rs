//! Used to track number of users of a service
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use tokio::{
    sync::{
        mpsc::{channel as mpsc, Sender as MpscSender},
        watch::{channel as watch, Receiver as WatchReceiver},
    },
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

use crate::{AnyResult, Result};

/// Why each active permit is held, by permit id (SPEC P6, P10)
pub(crate) type Reasons = Arc<Mutex<BTreeMap<u64, String>>>;

static NEXT_PERMIT_ID: AtomicU64 = AtomicU64::new(1);

/// Counts the active users of the stream
pub(crate) struct UseCounter {
    value: WatchReceiver<u32>,
    notifier_tx: MpscSender<bool>,
    reasons: Reasons,
    cancel: CancellationToken,
    set: JoinSet<AnyResult<()>>,
}

impl UseCounter {
    pub(crate) async fn new() -> Self {
        let (notifier_tx, mut notifier) = mpsc(100);
        let (value_tx, value) = watch(0);
        let cancel = CancellationToken::new();
        let mut set = JoinSet::new();

        let thread_cancel = cancel.clone();
        set.spawn(async move {
            let r = tokio::select! {
                _ = thread_cancel.cancelled() => {
                    AnyResult::Ok(())
                },
                v = async {
                    while let Some(noti) = notifier.recv().await {
                        value_tx.send_modify(|value| {
                            if noti {
                                log::trace!("Usecounter: {}->{}", *value, (*value) + 1);
                                *value += 1;
                            } else {
                                log::trace!("Usecounter: {}->{}", *value, (*value) - 1);
                                *value -= 1;
                            }
                        });
                    }
                    AnyResult::Ok(())
                } => v,
            };
            log::trace!("End Use Counter: {r:?}");
            r
        });
        Self {
            value,
            notifier_tx,
            reasons: Default::default(),
            cancel,
            set,
        }
    }

    #[cfg(feature = "gstreamer")]
    pub(crate) async fn create_activated(&self) -> Result<Permit> {
        self.create_activated_for("").await
    }

    /// An active permit, recorded with why it is held
    pub(crate) async fn create_activated_for(&self, reason: &str) -> Result<Permit> {
        let mut res = Permit::new(self, reason);
        res.activate().await?;
        Ok(res)
    }

    #[cfg(feature = "gstreamer")]
    #[allow(dead_code)]
    pub(crate) async fn create_deactivated(&self) -> Result<Permit> {
        Ok(Permit::new(self, ""))
    }

    /// Observe the number of active permits without holding one (SPEC P10)
    pub(crate) fn watch(&self) -> WatchReceiver<u32> {
        self.value.clone()
    }

    /// The reasons of the active permits
    pub(crate) fn reasons(&self) -> Reasons {
        self.reasons.clone()
    }
}

impl Drop for UseCounter {
    fn drop(&mut self) {
        log::trace!("Drop UseCounter");
        self.cancel.cancel();

        let mut set = std::mem::take(&mut self.set);
        let _gt = tokio::runtime::Handle::current().enter();
        tokio::task::spawn(async move { while set.join_next().await.is_some() {} });
        log::trace!("Dropped UseCounter");
    }
}

pub(crate) struct Permit {
    id: u64,
    reason: String,
    is_active: bool,
    value: WatchReceiver<u32>,
    notifier: MpscSender<bool>,
    reasons: Reasons,
}

impl Permit {
    #[cfg(feature = "gstreamer")]
    #[allow(dead_code)]
    pub(crate) fn subscribe(&self) -> Self {
        Self {
            id: NEXT_PERMIT_ID.fetch_add(1, Ordering::Relaxed),
            reason: self.reason.clone(),
            is_active: false,
            value: self.value.clone(),
            notifier: self.notifier.clone(),
            reasons: self.reasons.clone(),
        }
    }

    fn new(source: &UseCounter, reason: &str) -> Self {
        Self {
            id: NEXT_PERMIT_ID.fetch_add(1, Ordering::Relaxed),
            reason: reason.to_string(),
            is_active: false,
            value: source.value.clone(),
            notifier: source.notifier_tx.clone(),
            reasons: source.reasons.clone(),
        }
    }

    pub(crate) async fn activate(&mut self) -> Result<()> {
        if !self.is_active {
            self.is_active = true;
            self.reasons
                .lock()
                .unwrap()
                .insert(self.id, self.reason.clone());
            self.notifier.send(self.is_active).await?;
        }
        Ok(())
    }

    pub(crate) async fn deactivate(&mut self) -> Result<()> {
        if self.is_active {
            self.is_active = false;
            self.reasons.lock().unwrap().remove(&self.id);
            self.notifier.send(self.is_active).await?;
        }
        Ok(())
    }

    pub(crate) async fn aquired_users(&self) -> Result<()> {
        self.value
            .clone()
            .wait_for(|curr| {
                log::trace!("aquired_users: {}", *curr);
                *curr > 0
            })
            .await?;
        Ok(())
    }

    pub(crate) async fn dropped_users(&self) -> Result<()> {
        self.value
            .clone()
            .wait_for(|curr| {
                log::trace!("dropped_users: {}", *curr);
                *curr == 0
            })
            .await?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn get_counter(&self) -> WatchReceiver<u32> {
        self.value.clone()
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if self.is_active {
            self.is_active = false;
            self.reasons.lock().unwrap().remove(&self.id);
            let _gt = tokio::runtime::Handle::current().enter();
            let notifier = self.notifier.clone();
            let is_active = self.is_active;
            tokio::task::spawn(async move {
                let _ = notifier.send(is_active).await;
            });
        }
    }
}
