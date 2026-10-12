//! The wake ledger (SPEC P6): every connection of a sleeping camera, why, and for how long

use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant, SystemTime};

/// Entries kept per camera
const MAX_WAKES: usize = 1000;

/// More wakes than this within an hour is logged as a warning: on average one every 5 min,
/// far more than a battery camera serving occasional viewers needs
pub(crate) const WAKE_WARN_PER_HOUR: usize = 12;

const HOUR: Duration = Duration::from_secs(3600);

/// One connection of the camera
#[derive(Debug, Clone)]
pub(crate) struct Wake {
    /// Why it was connected: the reasons of the permits held at the time
    pub(crate) reason: String,
    /// When it was connected
    pub(crate) started: SystemTime,
    /// How long it stayed connected, `None` while it still is
    pub(crate) awake: Option<Duration>,
}

/// What the ledger publishes (MQTT `status/wakes`, SPEC P6)
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(crate) struct LedgerStatus {
    /// Connected now
    pub(crate) awake: bool,
    /// Why the camera was last woken
    pub(crate) reason: Option<String>,
    /// When it was last woken (Unix seconds)
    pub(crate) since: Option<u64>,
    /// How long the last finished wake lasted, in seconds
    pub(crate) last_awake_s: Option<f32>,
    /// Wakes since midnight UTC
    pub(crate) today_wakes: u32,
    /// Seconds awake since midnight UTC, finished wakes only
    pub(crate) today_awake_s: f32,
    /// Wakes within the last hour
    pub(crate) last_hour_wakes: usize,
}

#[derive(Debug, Default)]
pub(crate) struct WakeLedger {
    wakes: VecDeque<Wake>,
    current: Option<Instant>,
    day: Option<u64>,
    day_wakes: u32,
    day_awake: Duration,
    warned_at: Option<SystemTime>,
}

fn day_of(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86400
}

impl WakeLedger {
    /// The camera is being connected for `reason`. Returns a warning to log when it has been
    /// woken more than [`WAKE_WARN_PER_HOUR`] times within the hour (at most one warning an
    /// hour): the safety net that makes anything thrashing a battery camera visible
    pub(crate) fn wake(&mut self, reason: String) -> Option<String> {
        self.wake_at(reason, SystemTime::now())
    }

    fn wake_at(&mut self, reason: String, now: SystemTime) -> Option<String> {
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

        let recent = self.within_hour(now);
        let warned_recently = self
            .warned_at
            .is_some_and(|t| now.duration_since(t).unwrap_or_default() < HOUR);
        if recent.len() <= WAKE_WARN_PER_HOUR || warned_recently {
            return None;
        }
        let mut by_reason: BTreeMap<&str, u32> = BTreeMap::new();
        for w in &recent {
            *by_reason.entry(w.reason.as_str()).or_default() += 1;
        }
        let mut by_reason: Vec<_> = by_reason.into_iter().collect();
        by_reason.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let reasons: Vec<String> = by_reason
            .iter()
            .map(|(r, n)| format!("{r} x{n}"))
            .collect();
        let warning = format!(
            "woken {} times in the last hour ({}); a battery camera woken this often will drain. \
             Check what sends these requests",
            recent.len(),
            reasons.join(", ")
        );
        self.warned_at = Some(now);
        Some(warning)
    }

    fn within_hour(&self, now: SystemTime) -> Vec<&Wake> {
        self.wakes
            .iter()
            .filter(|w| now.duration_since(w.started).unwrap_or_default() < HOUR)
            .collect()
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

    /// The current state, for publishing
    pub(crate) fn status(&self) -> LedgerStatus {
        let last = self.wakes.back();
        LedgerStatus {
            awake: self.current.is_some(),
            reason: last.map(|w| w.reason.clone()),
            since: last.map(|w| {
                w.started
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            }),
            last_awake_s: self
                .wakes
                .iter()
                .rev()
                .find_map(|w| w.awake)
                .map(|d| d.as_secs_f32()),
            today_wakes: self.day_wakes,
            today_awake_s: self.day_awake.as_secs_f32(),
            last_hour_wakes: self.within_hour(SystemTime::now()).len(),
        }
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

    /// The safety net: too many wakes within an hour are logged loudly, once an hour, with
    /// their reasons
    #[test]
    fn test_warns_when_woken_too_often() {
        let mut l = WakeLedger::default();
        let t0 = SystemTime::now();
        let at = |m: u64| t0 + Duration::from_secs(m * 60);
        for i in 0..WAKE_WARN_PER_HOUR as u64 {
            assert!(l.wake_at("mqtt:control/pir".to_string(), at(i)).is_none());
            l.sleep();
        }
        let w = l
            .wake_at("rtsp:x:/a/main".to_string(), at(13))
            .expect("the 13th wake within the hour warns");
        assert!(w.contains("13 times"), "{}", w);
        assert!(w.contains("mqtt:control/pir x12, rtsp:x:/a/main x1"), "{}", w);
        // Not again within the hour
        assert!(l.wake_at("x".to_string(), at(14)).is_none());
        // Spread out wakes never warn
        let mut l = WakeLedger::default();
        for i in 0..100 {
            assert!(l.wake_at("x".to_string(), at(i * 6)).is_none());
        }
    }

    #[test]
    fn test_status() {
        let mut l = WakeLedger::default();
        assert_eq!(l.status(), LedgerStatus::default());
        l.wake("mqtt:control/wakeup".to_string());
        let s = l.status();
        assert!(s.awake);
        assert_eq!(s.reason.as_deref(), Some("mqtt:control/wakeup"));
        assert_eq!(s.today_wakes, 1);
        assert_eq!(s.last_hour_wakes, 1);
        l.sleep();
        let s = l.status();
        assert!(!s.awake);
        assert!(s.last_awake_s.is_some());
    }
}
