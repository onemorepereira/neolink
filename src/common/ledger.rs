//! The wake ledger (SPEC P6): every connection of a sleeping camera, why, and for how long

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime};

/// Entries kept per camera
const MAX_WAKES: usize = 1000;

/// One connection of the camera
#[derive(Debug, Clone)]
pub(crate) struct Wake {
    /// Why it was connected: the reasons of the permits held at the time
    pub(crate) reason: String,
    /// When it was connected
    #[allow(dead_code)]
    pub(crate) started: SystemTime,
    /// How long it stayed connected, `None` while it still is
    pub(crate) awake: Option<Duration>,
}

#[derive(Debug, Default)]
pub(crate) struct WakeLedger {
    wakes: VecDeque<Wake>,
    current: Option<Instant>,
    day: Option<u64>,
    day_wakes: u32,
    day_awake: Duration,
}

fn day_of(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86400
}

impl WakeLedger {
    /// The camera is being connected for `reason`
    pub(crate) fn wake(&mut self, reason: String) {
        let now = SystemTime::now();
        if self.day != Some(day_of(now)) {
            self.day = Some(day_of(now));
            self.day_wakes = 0;
            self.day_awake = Duration::ZERO;
        }
        self.day_wakes += 1;
        if self.wakes.len() == MAX_WAKES {
            self.wakes.pop_front();
        }
        self.wakes.push_back(Wake {
            reason,
            started: now,
            awake: None,
        });
        self.current = Some(Instant::now());
    }

    /// The camera was disconnected; returns the summary line to log
    pub(crate) fn sleep(&mut self) -> Option<String> {
        let awake = self.current.take()?.elapsed();
        self.day_awake += awake;
        let wake = self.wakes.back_mut()?;
        wake.awake = Some(awake);
        Some(format!(
            "awake {:.1} s for {}; today: {} wakes, {:.0} s awake",
            awake.as_secs_f32(),
            wake.reason,
            self.day_wakes,
            self.day_awake.as_secs_f32()
        ))
    }

    /// All recorded wakes, oldest first
    #[allow(dead_code)]
    pub(crate) fn wakes(&self) -> Vec<Wake> {
        self.wakes.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ledger() {
        let mut l = WakeLedger::default();
        assert!(l.sleep().is_none());
        l.wake("rtsp:main".to_string());
        let s = l.sleep().unwrap();
        assert!(s.contains("for rtsp:main; today: 1 wakes"), "{}", s);
        for _ in 0..MAX_WAKES + 5 {
            l.wake("x".to_string());
            l.sleep();
        }
        assert_eq!(l.wakes().len(), MAX_WAKES);
        assert!(l.wakes().iter().all(|w| w.awake.is_some()));
    }
}
