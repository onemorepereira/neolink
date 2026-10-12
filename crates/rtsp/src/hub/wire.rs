//! WireClock, Bridge and the per-session guard: the only source of on-wire time
//! (DESIGN §4.5)
//!
//! One wire clock per Hub, in µs, shared by every track and session:
//! `wire_us = media_us + offset`. The offset changes only at a re-latch (a rebase): when
//! live media resumes after a bridge, at a MediaClock discontinuity, or at the IRAP a
//! pacer skip goes to. A re-latch puts that frame at the Hub's wall time, like the first
//! latch, so the drift pacer underruns and pullbacks leave on the wire ends there instead
//! of being carried on; it never steps back from what was sent. Between re-latches, the
//! bridge's frames follow `virtual_now = last_wire_out + (now − last_wire_out_wall)`.

use crate::codec::annexb::VideoCodec;
use bytes::Bytes;

/// Default smallest step between the last frame sent and the first one after a re-latch:
/// one frame at 15 fps plus 1 ms. Less collides in clients that count in frame ticks
/// (ffmpeg: "non monotonically increasing dts"); the Hub sets it from the stream's fps.
pub const RELATCH_MIN_GAP_US: u64 = 67_667;

/// Seconds between 1900 (NTP) and 1970
const NTP_UNIX_OFFSET_S: u64 = 2_208_988_800;

/// The system clock as NTP µs (since 1900). A Hub takes it with its wire clock's epoch,
/// so that SR NTP times (epoch + wire time) start at real time (DESIGN §5.3)
pub fn ntp_now_us() -> u64 {
    let unix_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    unix_us + NTP_UNIX_OFFSET_S * 1_000_000
}

/// A track of a stream
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Track {
    Video,
    Audio,
}

impl Track {
    fn index(self) -> usize {
        match self {
            Track::Video => 0,
            Track::Audio => 1,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WireStats {
    /// Re-latches after the first latch (rebases)
    pub relatches: u64,
    /// The monotonic guard had to engage: always a bug, so counted and logged
    pub guard_engaged: u64,
}

/// A re-latch: where it put the frame, and how far that moved wire time against wall time
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rebase {
    /// The wire time of the frame re-latched on
    pub wire_us: u64,
    /// That wire time less `virtual_now` (where wire time would be had it followed wall
    /// time since the last release): positive is a forward on-wire jump
    pub shift_us: i64,
}

#[derive(Debug)]
pub struct WireClock {
    offset: Option<i64>,
    last_out: Option<(u64, u64)>, // (wire, wall)
    last_track: [Option<u64>; 2],
    pub stats: WireStats,
    /// The smallest step after a re-latch (about one frame interval)
    pub min_gap_us: u64,
    /// The latest wire time given to a late joiner's restamped IRAP: it goes to one
    /// session only, but whatever is latched next must come a real step after it
    last_restamp: Option<u64>,
}

impl Default for WireClock {
    fn default() -> Self {
        Self::new()
    }
}

impl WireClock {
    pub fn new() -> Self {
        Self {
            offset: None,
            last_out: None,
            last_track: [None, None],
            stats: WireStats::default(),
            min_gap_us: RELATCH_MIN_GAP_US,
            last_restamp: None,
        }
    }

    /// The last wire time released, on any track
    pub fn last_out_wire(&self) -> Option<u64> {
        self.last_out.map(|(w, _)| w)
    }

    /// Wire time that tracks wall time since the last release
    pub fn virtual_now(&self, now_wall: u64) -> u64 {
        match self.last_out {
            Some((wire, wall)) => wire + now_wall.saturating_sub(wall),
            None => now_wall,
        }
    }

    /// Re-latch so that the video frame at `media_us` is on the wire at the wall time
    /// `now_wall` (resume after a bridge, a MediaClock discontinuity, a pacer skip). Not
    /// at `virtual_now`: that keeps whatever wire time has drifted from wall time (pacer
    /// underruns and pullbacks, the media a skip drops), and re-latching there carried
    /// the drift on, more at every congestion episode (DESIGN §4.5)
    pub fn relatch(&mut self, media_us: u64, now_wall: u64) -> Rebase {
        // After the last video frame by a real step, even when no wall time has passed
        // (frames released together after a camera reconnect): a 1 µs step is below the
        // 90 kHz RTP tick and the millisecond of recordings (duplicate DTS in ffmpeg).
        // The same step keeps a wire clock that is ahead of wall time from going back.
        // Not after the last audio AU: that one is usually released moments earlier, and
        // a frame interval after it puts video ahead of the wall clock for good, a little
        // more at every discontinuity. Live audio that would not follow its last AU is
        // dropped by `stamp_if_after`
        let mut v = now_wall;
        if let Some(last) = self.last_track[Track::Video.index()] {
            v = v.max(last + self.min_gap_us);
        }
        if let Some(r) = self.last_restamp {
            v = v.max(r + self.min_gap_us);
        }
        self.offset = Some(v as i64 - media_us as i64);
        self.stats.relatches += 1;
        Rebase {
            wire_us: v,
            shift_us: v as i64 - self.virtual_now(now_wall) as i64,
        }
    }

    /// The wire time of a live frame released at `now_wall`
    pub fn stamp(&mut self, track: Track, media_us: u64, now_wall: u64) -> u64 {
        if self.offset.is_none() {
            self.relatch(media_us, now_wall);
            self.stats.relatches -= 1; // the first latch is not a re-latch
        }
        let wire = (media_us as i64 + self.offset.unwrap()).max(0) as u64;
        self.out(track, wire, now_wall)
    }

    /// Like [`Self::stamp`], but `None` when the frame would not be after the track's last
    /// one (live audio overlapping the bridge's silent audio after a resume): the caller
    /// drops it instead of nudging it forward
    pub fn stamp_if_after(&mut self, track: Track, media_us: u64, now_wall: u64) -> Option<u64> {
        if self.offset.is_none() {
            return Some(self.stamp(track, media_us, now_wall));
        }
        let wire = (media_us as i64 + self.offset.unwrap()).max(0) as u64;
        if self.last_track[track.index()].is_some_and(|l| wire <= l) {
            return None;
        }
        Some(self.out(track, wire, now_wall))
    }

    /// The wire time for a late joiner's restamped IRAP at `now_wall`: the wire time of
    /// now, remembered so the next latch lands at least `min_gap_us` after it
    pub fn restamp_now(&mut self, now_wall: u64) -> u64 {
        let w = self.virtual_now(now_wall);
        self.last_restamp = Some(self.last_restamp.map_or(w, |r| r.max(w)));
        w
    }

    /// The wire time of a synthetic (bridge) frame released at `now_wall`
    pub fn stamp_synthetic(&mut self, track: Track, now_wall: u64) -> u64 {
        let wire = self.virtual_now(now_wall);
        self.out(track, wire, now_wall)
    }

    fn out(&mut self, track: Track, wire: u64, now_wall: u64) -> u64 {
        let i = track.index();
        let wire = match self.last_track[i] {
            Some(last) if wire <= last => {
                self.stats.guard_engaged += 1;
                log::warn!("wire clock guard engaged on {track:?}: {wire} <= {last}");
                last + 1
            }
            _ => wire,
        };
        self.last_track[i] = Some(wire);
        if self.last_out.is_none_or(|(w, _)| wire >= w) {
            self.last_out = Some((wire, now_wall));
        }
        wire
    }
}

/// The end-of-sequence NAL to append to a repeated picture (bridge, late joiner), so each
/// repeat starts a new coded video sequence: H.264 needs it for every IDR repeat (the same
/// idr_pic_id twice is invalid); H.265 only after a CRA/BLA (a repeated IDR resets POC).
/// The H.265 EOS is a 2-byte packet that ffmpeg's depacketizer rejects ("Too short
/// RTP/HEVC packet"), so it is sent only where it is needed. The Argus sends IDR_W_RADL.
pub fn eos_nal(codec: VideoCodec, nals: &[Bytes]) -> Option<Bytes> {
    match codec {
        VideoCodec::H264 => Some(Bytes::from_static(&[0x0a])),
        VideoCodec::H265 => {
            let cra_or_bla = nals.iter().any(|n| {
                n.first().is_some_and(|b| {
                    (16..=18).contains(&((b >> 1) & 0x3f)) || (b >> 1) & 0x3f == 21
                })
            });
            cra_or_bla.then(|| Bytes::from_static(&[0x48, 0x01]))
        }
    }
}

/// Why media isn't flowing, from the source (SPEC: SourceState)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceState {
    Streaming,
    /// `pause.on_motion` idle: bridging is unlimited
    Paused,
    /// Outage: bridging is capped by `bridge_max_s`
    Retrying,
}

/// A frame the bridge wants sent now
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeOut {
    /// The cached IRAP plus EOS, at `wire_us`
    Video { wire_us: u64 },
    /// A silent AAC AU at `wire_us`
    Audio { wire_us: u64 },
    /// `bridge_max_s` ran out during an outage: close the sessions
    Expired,
}

#[derive(Debug)]
pub struct Bridge {
    engage_after_us: u64,
    interval_us: u64,
    au_us: u64,
    max_us: u64,
    active_since: Option<u64>,
    retrying_since: Option<u64>,
    next_video: u64,
    next_audio: u64,
    pub bridged_us: u64,
}

impl Bridge {
    /// `latency_us` is the pacer's; bridge_fps (default 0.2); audio AU duration (0 when
    /// there is no audio); bridge_max_s (default 120)
    pub fn new(latency_us: u64, bridge_fps: f64, au_us: u64, max_s: u64) -> Self {
        Self {
            engage_after_us: 3_000_000 + latency_us,
            interval_us: (1_000_000.0 / bridge_fps.max(0.01)) as u64,
            au_us,
            max_us: max_s * 1_000_000,
            active_since: None,
            retrying_since: None,
            next_video: 0,
            next_audio: 0,
            bridged_us: 0,
        }
    }

    pub fn active(&self) -> bool {
        self.active_since.is_some()
    }

    /// The silent-audio cadence: the AU duration of the stream's sample rate (0: none)
    pub fn set_au_us(&mut self, au_us: u64) {
        self.au_us = au_us;
    }

    /// Time spent bridging so far, including a bridge still running
    pub fn bridged_total(&self, now_wall: u64) -> u64 {
        self.bridged_us + self.active_since.map_or(0, |s| now_wall.saturating_sub(s))
    }

    /// Forget bridge and outage state (after `Expired`, or a new start)
    pub fn reset(&mut self) {
        self.active_since = None;
        self.retrying_since = None;
    }

    /// Note an outage even before any video was released (so bridge_max still applies)
    pub fn retrying(&mut self, now_wall: u64) -> bool {
        let since = *self.retrying_since.get_or_insert(now_wall);
        now_wall.saturating_sub(since) >= self.max_us
    }

    /// Live video was released: leave the bridge. Returns true if it was active, which
    /// means the caller must re-latch the wire clock on this (IRAP) frame
    pub fn live(&mut self, now_wall: u64) -> bool {
        self.retrying_since = None;
        match self.active_since.take() {
            Some(since) => {
                self.bridged_us += now_wall.saturating_sub(since);
                true
            }
            None => false,
        }
    }

    /// Called whenever the Hub wakes. `last_video_wall` is the last live video release
    pub fn tick(
        &mut self,
        now_wall: u64,
        last_video_wall: u64,
        state: SourceState,
        clock: &mut WireClock,
    ) -> Vec<BridgeOut> {
        let mut out = vec![];
        if state == SourceState::Retrying {
            let since = *self.retrying_since.get_or_insert(now_wall);
            if now_wall.saturating_sub(since) >= self.max_us {
                return vec![BridgeOut::Expired];
            }
        } else {
            self.retrying_since = None;
        }
        if self.active_since.is_none() {
            if now_wall.saturating_sub(last_video_wall) < self.engage_after_us {
                return out;
            }
            self.active_since = Some(now_wall);
            self.next_video = now_wall;
            self.next_audio = now_wall;
        }
        while self.au_us > 0 && self.next_audio <= now_wall {
            out.push(BridgeOut::Audio {
                wire_us: clock.stamp_synthetic(Track::Audio, self.next_audio),
            });
            self.next_audio += self.au_us;
        }
        if self.next_video <= now_wall {
            out.push(BridgeOut::Video {
                wire_us: clock.stamp_synthetic(Track::Video, now_wall),
            });
            self.next_video = now_wall + self.interval_us;
        }
        out
    }

    /// When the bridge next needs a wake-up
    pub fn next_wake(&self, last_video_wall: u64) -> u64 {
        match self.active_since {
            None => last_video_wall + self.engage_after_us,
            Some(_) if self.au_us > 0 => self.next_audio.min(self.next_video),
            Some(_) => self.next_video,
        }
    }
}

/// Per-session ordering guard (F-ds 7): each session remembers its last wire time per
/// track. Video below it is skipped until the next IRAP above; audio below it is skipped.
/// This keeps late-joiner restamps safe even during a pacer underrun.
#[derive(Debug)]
pub struct SessionGuard {
    last: [Option<u64>; 2],
    waiting_irap: bool,
    pub skipped: u64,
}

impl Default for SessionGuard {
    /// A new session starts on an IRAP: no P-frames (or audio) before its first picture
    fn default() -> Self {
        Self {
            last: [None, None],
            waiting_irap: true,
            skipped: 0,
        }
    }
}

impl SessionGuard {
    /// Waiting for an IRAP (no video and no audio until one is admitted)
    pub fn waiting(&self) -> bool {
        self.waiting_irap
    }

    /// Whether to send this frame to the session
    pub fn admit(&mut self, track: Track, wire_us: u64, irap: bool) -> bool {
        let i = track.index();
        let ok = self.last[i].is_none_or(|l| wire_us > l);
        match track {
            Track::Video => {
                if !ok {
                    self.waiting_irap = true;
                    self.skipped += 1;
                    return false;
                }
                if self.waiting_irap && !irap {
                    self.skipped += 1;
                    return false;
                }
                self.waiting_irap = false;
            }
            Track::Audio => {
                // No audio until the picture it belongs with (resync, late join)
                if !ok || self.waiting_irap {
                    self.skipped += 1;
                    return false;
                }
            }
        }
        self.last[i] = Some(wire_us);
        true
    }

    /// The session must resync: no video until the next IRAP (byte budget, R13), and no
    /// audio until then either
    pub fn resync(&mut self) {
        self.waiting_irap = true;
    }

    /// After a restamped IRAP (late joiner): live video only from the next IRAP
    pub fn after_restamp(&mut self) {
        self.waiting_irap = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bridge_engages_and_resumes_without_backsteps() {
        let mut clock = WireClock::new();
        let mut bridge = Bridge::new(1_500_000, 0.2, 64_000, 120);
        // Live frames at 15 fps for 2 s
        let mut wall = 0u64;
        let mut last_v = 0u64;
        for i in 0..30u64 {
            wall = i * 66_667;
            last_v = clock.stamp(Track::Video, 1_000_000 + i * 66_667, wall);
        }
        let last_video_wall = wall;
        // Silence: not before 4.5 s
        assert!(bridge
            .tick(
                wall + 4_000_000,
                last_video_wall,
                SourceState::Retrying,
                &mut clock
            )
            .is_empty());
        let out = bridge.tick(
            wall + 4_500_000,
            last_video_wall,
            SourceState::Retrying,
            &mut clock,
        );
        let v = out.iter().find_map(|o| match o {
            BridgeOut::Video { wire_us } => Some(*wire_us),
            _ => None,
        });
        assert!(v.unwrap() > last_v);
        // Wire time follows wall time while bridging
        assert_eq!(v.unwrap(), last_v + 4_500_000);
        // Live resumes with a much older camera time: re-latch, still forward
        let resume = wall + 10_000_000;
        assert!(bridge.live(resume));
        clock.relatch(500_000, resume);
        let w = clock.stamp(Track::Video, 500_000, resume);
        assert!(w > v.unwrap());
        assert_eq!(clock.stats.guard_engaged, 0);
    }

    #[test]
    fn test_bridge_max_only_for_outages() {
        let mut clock = WireClock::new();
        let mut b = Bridge::new(0, 0.2, 0, 120);
        assert!(b
            .tick(130_000_000, 0, SourceState::Paused, &mut clock)
            .iter()
            .all(|o| *o != BridgeOut::Expired));
        let mut b = Bridge::new(0, 0.2, 0, 120);
        b.tick(10_000_000, 0, SourceState::Retrying, &mut clock);
        assert_eq!(
            b.tick(131_000_000, 0, SourceState::Retrying, &mut clock),
            vec![BridgeOut::Expired]
        );
    }

    #[test]
    fn test_silent_audio_cadence() {
        let mut clock = WireClock::new();
        let mut b = Bridge::new(0, 0.2, 64_000, 120);
        let out = b.tick(3_000_000, 0, SourceState::Paused, &mut clock);
        assert_eq!(
            out.iter()
                .filter(|o| matches!(o, BridgeOut::Audio { .. }))
                .count(),
            1
        );
        let out = b.tick(4_000_000, 0, SourceState::Paused, &mut clock);
        // 1 s later: about 16 AUs (64 ms each)
        let n = out
            .iter()
            .filter(|o| matches!(o, BridgeOut::Audio { .. }))
            .count();
        assert!((15..=16).contains(&n), "{n}");
    }

    /// A re-latch with no wall time passed (frames released together after a reconnect)
    /// still moves the wire clock by a real step: no duplicate timestamps downstream
    #[test]
    fn test_relatch_without_wall_time_steps_forward() {
        let mut clock = WireClock::new();
        let a = clock.stamp(Track::Video, 1_000_000, 5_000_000);
        // A discontinuity at the same wall instant, with media time going back
        clock.relatch(10_000, 5_000_000);
        let b = clock.stamp(Track::Video, 10_000, 5_000_000);
        assert!(b >= a + RELATCH_MIN_GAP_US, "{a} then {b}");
        assert_eq!(clock.stats.guard_engaged, 0);
    }

    /// A discontinuity frame released on schedule, one frame after the last video frame
    /// and just after an audio AU: it lands one step after the last video frame, at wall
    /// time, not a frame interval after the audio AU (which ratcheted wire time ahead of
    /// the wall clock at every discontinuity)
    #[test]
    fn test_relatch_gap_is_to_the_last_video_frame() {
        let mut clock = WireClock::new();
        let v = clock.stamp(Track::Video, 1_000_000, 5_000_000);
        assert_eq!(v, 5_000_000);
        let a = clock.stamp(Track::Audio, 1_060_000, 5_060_000);
        assert_eq!(a, 5_060_000);
        clock.relatch(10_000, 5_066_667);
        let w = clock.stamp(Track::Video, 10_000, 5_066_667);
        assert_eq!(
            w,
            v + RELATCH_MIN_GAP_US,
            "lead {} µs",
            w as i64 - 5_066_667
        );
        // Audio from the old timeline that would not follow its last AU is dropped
        assert_eq!(clock.stamp_if_after(Track::Audio, 0, 5_070_000), None);
        assert_eq!(clock.stats.guard_engaged, 0);
    }

    /// A re-latch puts its frame at wall time: wire time that fell behind (pacer underruns)
    /// jumps forward there, by the reported shift; wire time ahead of wall time comes back
    /// towards it without stepping back from the last video frame
    #[test]
    fn test_relatch_aims_at_wall_time() {
        let mut clock = WireClock::new();
        assert_eq!(clock.stamp(Track::Video, 0, 1_000_000), 1_000_000);
        // Frames released 5 s late (underruns): wire time 5 s behind wall time
        let late = clock.stamp(Track::Video, 66_667, 6_066_667);
        assert_eq!(late, 1_066_667);
        let r = clock.relatch(133_333, 6_133_333);
        assert_eq!(r.wire_us, 6_133_333);
        assert_eq!(r.shift_us, 5_000_000);
        assert_eq!(clock.stamp(Track::Video, 133_333, 6_133_333), 6_133_333);
        assert_eq!(clock.stamp(Track::Video, 200_000, 6_200_000), 6_200_000);

        // Wire time 2 s ahead (frames released early, a pullback): back to the wall clock
        // after a gap, never before the last video frame
        let mut clock = WireClock::new();
        clock.stamp(Track::Video, 2_000_000, 0);
        let ahead = clock.stamp(Track::Video, 4_000_000, 0);
        assert_eq!(ahead, 2_000_000);
        let r = clock.relatch(0, 1_000_000);
        assert_eq!(r.wire_us, ahead + RELATCH_MIN_GAP_US);
        assert!(r.shift_us < 0, "{r:?}");
        let r = clock.relatch(0, 10_000_000);
        assert_eq!(r.wire_us, 10_000_000);
        assert_eq!(clock.stats.relatches, 2);
        assert_eq!(clock.stats.guard_engaged, 0);
    }

    /// A late joiner's restamped IRAP, then the source's first latch in the same instant
    /// (a cold PLAY with a cached IRAP): live video comes a frame interval later, not
    /// microseconds (duplicate timestamps in wall-clock clients like Frigate)
    #[test]
    fn test_latch_after_restamp_keeps_a_gap() {
        let mut clock = WireClock::new();
        let r = clock.restamp_now(5_000_000);
        let live = clock.stamp(Track::Video, 123_456, 5_000_020);
        assert!(live >= r + RELATCH_MIN_GAP_US, "{r} then {live}");
    }

    #[test]
    fn test_session_guard() {
        let mut g = SessionGuard::default();
        assert!(g.admit(Track::Video, 100, true));
        assert!(g.admit(Track::Video, 200, false));
        // A restamped frame below what the session has: skipped, then wait for an IRAP
        assert!(!g.admit(Track::Video, 150, false));
        assert!(!g.admit(Track::Video, 300, false));
        assert!(g.admit(Track::Video, 400, true));
        assert!(g.admit(Track::Audio, 50, false));
        assert!(!g.admit(Track::Audio, 50, false));
    }

    /// A22 (wire part): under random schedules of live frames, outages and pauses (the
    /// bridge), resumes, camera-time discontinuities, underruns and late joiners, applying
    /// the Hub's rules (resume only on an IRAP, live media dropped while bridging, re-latch
    /// on resume and on discontinuities), wire time is monotonic per track **without the
    /// guard ever engaging**, and per session after the session guard.
    #[test]
    fn test_a22_wire_monotonic_property() {
        for seed in 0..300u64 {
            let mut rng = Rng(seed);
            let mut clock = WireClock::new();
            let mut bridge = Bridge::new(rng.below(3_000_000), 0.2, 64_000, 120);
            let mut sessions: Vec<SessionGuard> = vec![];
            let mut session_last: Vec<[Option<u64>; 2]> = vec![];
            let mut wall = 0u64;
            let mut media = rng.below(10_000_000);
            let mut audio_media = media;
            let mut last_video_wall = 0u64;
            let mut discontinuity = false;
            let mut gop = 0u64;
            for _ in 0..400 {
                let mut out: Vec<(Track, u64, bool)> = vec![];
                match rng.below(10) {
                    // Live video (and its audio)
                    0..=4 => {
                        wall += rng.below(200_000);
                        media += 66_667;
                        gop += 1;
                        let irap = gop % 30 == 0;
                        if bridge.active() {
                            if !irap {
                                continue;
                            }
                            bridge.live(wall);
                            clock.relatch(media, wall);
                            discontinuity = false;
                        } else if discontinuity {
                            clock.relatch(media, wall);
                            discontinuity = false;
                        }
                        last_video_wall = wall;
                        out.push((Track::Video, clock.stamp(Track::Video, media, wall), irap));
                        if !bridge.active() {
                            audio_media = audio_media.max(media.saturating_sub(rng.below(80_000)));
                            if let Some(w) = clock.stamp_if_after(Track::Audio, audio_media, wall) {
                                out.push((Track::Audio, w, false));
                            }
                            audio_media += 64_000;
                        }
                    }
                    // A gap: pause or outage, the bridge runs
                    5 | 6 => {
                        wall += rng.below(10_000_000);
                        let state = if rng.below(2) == 0 {
                            SourceState::Paused
                        } else {
                            SourceState::Retrying
                        };
                        for o in bridge.tick(wall, last_video_wall, state, &mut clock) {
                            match o {
                                BridgeOut::Video { wire_us } => {
                                    out.push((Track::Video, wire_us, true))
                                }
                                BridgeOut::Audio { wire_us } => {
                                    out.push((Track::Audio, wire_us, false))
                                }
                                BridgeOut::Expired => bridge.reset(),
                            }
                        }
                    }
                    // Discontinuity: camera time jumps either way (re-latched on the next frame)
                    7 => {
                        media = if rng.below(2) == 0 {
                            media / 2
                        } else {
                            media + 5_000_000
                        };
                        audio_media = media;
                        discontinuity = true;
                    }
                    // Late joiner: the cached IRAP at the last wire time, then the next IRAP
                    8 => {
                        let mut g = SessionGuard::default();
                        let mut last = [None, None];
                        if let Some(w) = clock.last_out_wire() {
                            if g.admit(Track::Video, w, true) {
                                last[0] = Some(w);
                            }
                            g.after_restamp();
                        }
                        sessions.push(g);
                        session_last.push(last);
                    }
                    // Underrun: a long wait, then a burst
                    _ => wall += 2_000_000 + rng.below(3_000_000),
                }
                for (track, wire, irap) in out {
                    for (g, last) in sessions.iter_mut().zip(session_last.iter_mut()) {
                        let i = track.index();
                        if g.admit(track, wire, irap) {
                            assert!(
                                last[i].is_none_or(|l| wire > l),
                                "seed {seed}: session went backwards"
                            );
                            last[i] = Some(wire);
                        }
                    }
                }
            }
            assert_eq!(
                clock.stats.guard_engaged, 0,
                "seed {seed}: the wire guard engaged"
            );
        }
    }

    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (z ^ (z >> 31)) % n.max(1)
        }
    }
}
