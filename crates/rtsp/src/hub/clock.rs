//! MediaClock: camera time to one continuous media timeline (DESIGN §4.2, SPEC R19)
//!
//! Video time is the camera's u32 µs counter, unwrapped. A delta below 0 or above 2 s,
//! or a source restart, is a **discontinuity**: media time advances by one frame and the
//! event is flagged so that the WireClock re-latches. The clock never looks at host time
//! for video.
//!
//! Audio has no camera timestamp. It is counted in samples from an anchor (the latest
//! video media time, plus `skew_estimate`) and its offset to video is estimated, at each
//! video frame, as the median over a sliding 20 s window of
//! `err = audio position − video media time`. When `|median| > 80 ms` it is corrected by
//! at most one AU per second (drop an AU when audio is ahead, skip 1024 samples when
//! behind) until `|median| < 20 ms`, or until it is within half an AU (corrections move in
//! whole AUs, so a 32 ms offset at 16 kHz can't get closer). Corrections need 2 s of
//! samples. If the first 2 s show `|median| > 500 ms`, audio is
//! re-anchored once, before any session received audio.

use std::collections::VecDeque;

/// A video frame's place on the media timeline
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoTime {
    pub media_us: u64,
    /// Re-latch the wire offset (first frame, jump, wrap glitch or restart)
    pub discontinuity: bool,
}

/// What to do with an audio AU
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioTime {
    /// Send it at this media time, `samples` after the anchor
    Send { media_us: u64, samples: u64 },
    /// Drop it (audio is ahead of video)
    Drop,
}

const MAX_DELTA_US: i64 = 2_000_000;
const WINDOW_US: u64 = 20_000_000;
const CORRECT_ABOVE_US: i64 = 80_000;
const CORRECT_UNTIL_US: i64 = 20_000;
const REANCHOR_ABOVE_US: i64 = 500_000;
const INITIAL_US: u64 = 2_000_000;

#[derive(Debug)]
pub struct MediaClock {
    sample_rate: u32,
    media_us: u64,
    last_cam: Option<u32>,
    frame_us: u64,
    skew_estimate_us: i64,
    // Audio
    anchor_us: Option<u64>,
    samples: u64,
    anchored_at_host: u64,
    reanchored: bool,
    /// (host µs, err µs)
    errs: VecDeque<(u64, i64)>,
    correcting: bool,
    last_correction_host: Option<u64>,
    /// Corrections applied (stats)
    pub drift_corrections: u64,
    /// The last audio media time sent, so a re-anchor never goes backwards
    last_audio_us: Option<u64>,
}

impl MediaClock {
    pub fn new(sample_rate: u32, skew_estimate_us: i64) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            media_us: 0,
            last_cam: None,
            frame_us: 66_667,
            skew_estimate_us,
            anchor_us: None,
            samples: 0,
            anchored_at_host: 0,
            reanchored: false,
            errs: VecDeque::new(),
            correcting: false,
            last_correction_host: None,
            drift_corrections: 0,
            last_audio_us: None,
        }
    }

    /// The audio sample rate (from the ADTS header); a change re-anchors audio
    pub fn set_sample_rate(&mut self, rate: u32) {
        if rate > 0 && rate != self.sample_rate {
            self.sample_rate = rate;
            self.anchor_us = None;
        }
    }

    /// Duration of one 1024-sample AU, µs
    pub fn au_us(&self) -> u64 {
        1024 * 1_000_000 / self.sample_rate as u64
    }

    /// Current video media time
    pub fn media_us(&self) -> u64 {
        self.media_us
    }

    /// A video frame with camera time `cam_us`, arrived at `host_us`
    pub fn video(&mut self, cam_us: u32, restarted: bool, host_us: u64) -> VideoTime {
        let discontinuity = match (self.last_cam, restarted) {
            (Some(last), false) => {
                let d = cam_us.wrapping_sub(last) as i32 as i64;
                if (0..=MAX_DELTA_US).contains(&d) {
                    if d > 0 && d <= 200_000 {
                        // Track the frame interval for discontinuity steps
                        self.frame_us = (self.frame_us * 7 + d as u64) / 8;
                    }
                    self.media_us += d as u64;
                    false
                } else {
                    self.media_us += self.frame_us;
                    true
                }
            }
            (Some(_), true) => {
                self.media_us += self.frame_us;
                true
            }
            (None, _) => true,
        };
        self.last_cam = Some(cam_us);
        if discontinuity && self.anchor_us.is_some() {
            // Audio re-anchors to video after a discontinuity, and may re-anchor once more
            // if the new anchor is far off
            self.anchor_us = None;
            self.reanchored = false;
        }
        // Offset estimate, sampled at each video frame
        if let Some(pos) = self.audio_pos_us() {
            let err = pos as i64 - self.media_us as i64;
            self.errs.push_back((host_us, err));
            while self
                .errs
                .front()
                .is_some_and(|(t, _)| host_us.saturating_sub(*t) > WINDOW_US)
            {
                self.errs.pop_front();
            }
        }
        VideoTime {
            media_us: self.media_us,
            discontinuity,
        }
    }

    fn audio_pos_us(&self) -> Option<u64> {
        let anchor = self.anchor_us?;
        Some(anchor + self.samples * 1_000_000 / self.sample_rate as u64)
    }

    fn median_err(&self) -> Option<i64> {
        if self.errs.is_empty() {
            return None;
        }
        let mut v: Vec<i64> = self.errs.iter().map(|(_, e)| *e).collect();
        v.sort_unstable();
        Some(v[v.len() / 2])
    }

    /// An AAC AU of 1024 samples arrived at `host_us`. `audio_sent` says whether any
    /// session has received audio yet (the initial re-anchor is only allowed before).
    pub fn audio(&mut self, host_us: u64, audio_sent: bool) -> AudioTime {
        if self.anchor_us.is_none() {
            if self.last_cam.is_none() {
                // No video yet: nothing to anchor to
                return AudioTime::Drop;
            }
            let mut anchor = (self.media_us as i64 + self.skew_estimate_us).max(0) as u64;
            // Never behind what was already sent (DESIGN: audio media >= last + 1 AU)
            if let Some(last) = self.last_audio_us {
                anchor = anchor.max(last + self.au_us());
            }
            self.anchor_us = Some(anchor);
            self.samples = 0;
            self.anchored_at_host = host_us;
            self.errs.clear();
            self.correcting = false;
        }
        let median = self.median_err();
        // Initial error: re-anchor once, as a jump, before anyone heard audio
        if !self.reanchored
            && !audio_sent
            && host_us.saturating_sub(self.anchored_at_host) >= INITIAL_US
        {
            self.reanchored = true;
            if let Some(m) = median.filter(|m| m.abs() > REANCHOR_ABOVE_US) {
                // Nobody has heard audio yet, so this one jump may go backwards; audio
                // already queued is then dropped by the wire clock until it is in order
                let anchor = (self.anchor_us.unwrap() as i64 - m).max(0) as u64;
                self.anchor_us = Some(anchor);
                self.last_audio_us = None;
                self.errs.clear();
            }
        }
        let mut skip = false;
        let au_us = (1024 * 1_000_000 / self.sample_rate as u64) as i64;
        // Enough history for a median (2 s), else no correction yet
        let span = match (self.errs.front(), self.errs.back()) {
            (Some(a), Some(b)) => b.0.saturating_sub(a.0),
            _ => 0,
        };
        let median = (span >= INITIAL_US).then(|| self.median_err()).flatten();
        if let Some(m) = median {
            if m.abs() > CORRECT_ABOVE_US {
                self.correcting = true;
            } else if m.abs() < CORRECT_UNTIL_US || m.abs() <= au_us / 2 {
                // Corrections move in whole AUs: one more would not bring it closer
                self.correcting = false;
            }
            let due = self
                .last_correction_host
                .is_none_or(|t| host_us.saturating_sub(t) >= 1_000_000);
            if self.correcting && due {
                self.last_correction_host = Some(host_us);
                self.drift_corrections += 1;
                log::debug!(
                    "audio drift correction: median {m} µs over {} samples",
                    self.errs.len()
                );
                // The correction shifts every sample in the window by one AU
                if m > 0 {
                    for e in self.errs.iter_mut() {
                        e.1 -= au_us;
                    }
                    return AudioTime::Drop;
                }
                skip = true;
                for e in self.errs.iter_mut() {
                    e.1 += au_us;
                }
            }
        }
        if skip {
            // Behind: skip 1024 samples, so this AU's delta is 2048
            self.samples += 1024;
        }
        let media_us = self.audio_pos_us().unwrap();
        let samples = self.samples;
        self.samples += 1024;
        self.last_audio_us = Some(media_us);
        AudioTime::Send { media_us, samples }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: u32 = 66_667;

    #[test]
    fn test_unwrap_across_u32() {
        let mut c = MediaClock::new(16000, 0);
        let start = u32::MAX - 3 * FRAME;
        let mut cam = start;
        let mut last = None;
        for i in 0..10u64 {
            let t = c.video(cam, false, i * 66_667);
            if let Some(l) = last {
                assert_eq!(t.media_us - l, FRAME as u64, "frame {i}");
                assert!(!t.discontinuity);
            } else {
                assert!(t.discontinuity, "the first frame latches");
            }
            last = Some(t.media_us);
            cam = cam.wrapping_add(FRAME);
        }
    }

    #[test]
    fn test_reset_and_restart_are_discontinuities() {
        let mut c = MediaClock::new(16000, 0);
        c.video(5_000_000, false, 0);
        let a = c.video(5_066_667, false, 1);
        // The counter jumps back: one frame forward, flagged
        let b = c.video(1_000_000, false, 2);
        assert!(b.discontinuity);
        assert_eq!(b.media_us, a.media_us + 66_667);
        // A jump forward over 2 s too
        let d = c.video(9_000_000, false, 3);
        assert!(d.discontinuity);
        // A restart even with a plausible delta
        let e = c.video(9_066_667, true, 4);
        assert!(e.discontinuity);
        assert!(e.media_us > d.media_us);
    }

    /// Video at 15 fps and audio at 64 ms; `skew` µs of extra audio delay per AU
    fn run(
        c: &mut MediaClock,
        secs: u64,
        audio_rate_ppm: i64,
        audio_lead_us: i64,
        audio_sent: bool,
    ) -> Vec<AudioTime> {
        let mut out = vec![];
        let mut next_v = 0u64;
        let mut next_a = 0i64;
        let mut cam = 1_000u32;
        let end = secs * 1_000_000;
        while next_v < end {
            if (next_a as u64) < next_v {
                let t = (next_a + audio_lead_us).max(0) as u64;
                out.push(c.audio(t, audio_sent));
                // Audio clock running `ppm` fast relative to video
                next_a += 64_000 - 64_000 * audio_rate_ppm / 1_000_000;
            } else {
                c.video(cam, false, next_v);
                cam = cam.wrapping_add(FRAME);
                next_v += FRAME as u64;
            }
        }
        out
    }

    #[test]
    fn test_no_corrections_when_clocks_agree() {
        // The Argus: same clock, small skew (SPEC §9)
        let mut c = MediaClock::new(16000, 0);
        let out = run(&mut c, 120, 0, 8_000, true);
        assert_eq!(c.drift_corrections, 0);
        assert!(out.iter().all(|a| matches!(a, AudioTime::Send { .. })));
        // Samples advance by 1024 per AU
        let s: Vec<u64> = out
            .iter()
            .filter_map(|a| match a {
                AudioTime::Send { samples, .. } => Some(*samples),
                _ => None,
            })
            .collect();
        assert!(s.windows(2).all(|w| w[1] - w[0] == 1024));
    }

    #[test]
    fn test_drift_is_corrected_gradually() {
        // Audio 2000 ppm fast: 0.24 s ahead per 2 min without correction
        let mut c = MediaClock::new(16000, 0);
        let out = run(&mut c, 300, 2000, 0, true);
        assert!(c.drift_corrections > 0);
        assert!(out.contains(&AudioTime::Drop));
        // At the end the error is back within the correction band
        let m = c.median_err().unwrap();
        assert!(m.abs() <= 100_000, "median error {} µs", m);
    }

    #[test]
    fn test_initial_offset_reanchors_once() {
        // A bad skew_estimate puts audio 700 ms ahead; nobody has heard audio yet
        let mut c = MediaClock::new(16000, 700_000);
        run(&mut c, 5, 0, 0, false);
        assert!(c.reanchored);
        let m = c.median_err().unwrap();
        assert!(m.abs() < 100_000, "after re-anchor the error is {} µs", m);
    }

    #[test]
    fn test_audio_before_video_is_dropped() {
        let mut c = MediaClock::new(16000, 0);
        assert_eq!(c.audio(0, false), AudioTime::Drop);
    }

    #[test]
    fn test_fixture_needs_no_corrections() {
        let path = format!(
            "{}/../../fixtures/synth/plain_h265.bcm",
            env!("CARGO_MANIFEST_DIR")
        );
        let units = crate::source::parse_bcm(&std::fs::read(path).unwrap());
        let mut c = MediaClock::new(16000, 0);
        let mut errs = vec![];
        for u in units {
            match u.unit {
                crate::source::SourceUnit::Video { cam_us, .. } => {
                    c.video(cam_us, false, u.at_us);
                    if let Some(m) = c.median_err() {
                        errs.push(m);
                    }
                }
                crate::source::SourceUnit::Aac { .. } => {
                    c.audio(u.at_us, true);
                }
                _ => {}
            }
        }
        let (min, max) = (errs.iter().min().unwrap(), errs.iter().max().unwrap());
        assert_eq!(c.drift_corrections, 0, "median err range {min}..{max} µs");
    }
}
