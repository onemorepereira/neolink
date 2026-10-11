//! Stream description: identity, readiness, generations and the on-disk cache
//! (DESIGN §2, §4.3)

use crate::codec::aac::AdtsHeader;
use crate::codec::annexb::{param_kind, ParamKind, VideoCodec};
use crate::codec::sps::{picture_size, PictureSize};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Parameter sets as NAL bodies (with NAL header, without start codes)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ParamSets {
    pub vps: Option<Vec<u8>>,
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

/// What a decoder depends on; two descs are "the same stream" iff these match
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VideoIdentity {
    pub codec: VideoCodecSer,
    pub params: ParamSets,
    pub width: u32,
    pub height: u32,
}

/// Serializable mirror of [`VideoCodec`]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VideoCodecSer {
    H264,
    H265,
}

impl From<VideoCodec> for VideoCodecSer {
    fn from(c: VideoCodec) -> Self {
        match c {
            VideoCodec::H264 => Self::H264,
            VideoCodec::H265 => Self::H265,
        }
    }
}

impl From<VideoCodecSer> for VideoCodec {
    fn from(c: VideoCodecSer) -> Self {
        match c {
            VideoCodecSer::H264 => Self::H264,
            VideoCodecSer::H265 => Self::H265,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AudioDesc {
    /// AudioSpecificConfig
    pub asc: Vec<u8>,
    pub sample_rate: u32,
    pub channels: u8,
}

impl AudioDesc {
    pub fn from_adts(h: &AdtsHeader) -> Self {
        Self {
            asc: h.audio_specific_config().to_vec(),
            sample_rate: h.sample_rate(),
            channels: h.channel_config,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamDesc {
    pub video: VideoIdentity,
    pub audio: Option<AudioDesc>,
    /// Informational (a=framerate); not part of the identity
    pub framerate: Option<u32>,
    /// Bumped when `video` or the audio ASC changes
    pub gen: u32,
}

impl StreamDesc {
    /// Identity hash, used to tie `irap.bin` to `desc.json`
    pub fn hash(&self) -> String {
        let json = serde_json::to_vec(&(&self.video, &self.audio)).unwrap_or_default();
        Sha256::digest(json)
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// What the learner concluded from a unit
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescEvent {
    /// The first desc is ready
    Ready(StreamDesc),
    /// The stream changed in a way decoders care about: sessions on the old gen close
    Changed(StreamDesc),
    /// Audio appeared after a desc without audio was published; for the next DESCRIBE
    LateAudio(StreamDesc),
}

/// Learns the desc from the media and tracks changes (DESIGN §4.3)
#[derive(Debug)]
pub struct DescLearner {
    audio_wanted: bool,
    current: Option<StreamDesc>,
    /// Waiting for audio after an IRAP: (video identity, host µs of that IRAP)
    pending: Option<(VideoIdentity, u64)>,
    audio: Option<AudioDesc>,
    intervals: Vec<u64>,
    last_media_us: Option<u64>,
    framerate: Option<u32>,
    gen: u32,
}

const AUDIO_WAIT_US: u64 = 1_000_000;

impl DescLearner {
    /// `audio_wanted` is false with `audio = "none"` (ready at the first IRAP)
    pub fn new(audio_wanted: bool, cached: Option<StreamDesc>) -> Self {
        let gen = cached.as_ref().map(|d| d.gen).unwrap_or(0);
        Self {
            audio_wanted,
            framerate: cached.as_ref().and_then(|d| d.framerate),
            current: cached,
            pending: None,
            audio: None,
            intervals: vec![],
            last_media_us: None,
            gen,
        }
    }

    pub fn current(&self) -> Option<&StreamDesc> {
        self.current.as_ref()
    }

    /// A video AU (its NALs). Returns an event when the desc becomes ready or changes.
    pub fn video(
        &mut self,
        codec: VideoCodec,
        nals: &[Bytes],
        irap: bool,
        media_us: u64,
        host_us: u64,
    ) -> Option<DescEvent> {
        if self.framerate.is_none() {
            if let Some(last) = self.last_media_us {
                if media_us > last {
                    self.intervals.push(media_us - last);
                }
            }
            if self.intervals.len() >= 30 {
                let mut v = self.intervals.clone();
                v.sort_unstable();
                let median = v[v.len() / 2].max(1);
                self.framerate = Some(((1_000_000 + median / 2) / median) as u32);
            }
        }
        self.last_media_us = Some(media_us);

        if irap {
            if let Some(identity) = identity_from(codec, nals) {
                match &self.current {
                    Some(cur) if cur.video == identity => {}
                    Some(_) => {
                        self.gen += 1;
                        let d = self.make(identity, self.audio.clone());
                        self.current = Some(d.clone());
                        return Some(DescEvent::Changed(d));
                    }
                    None => {
                        if !self.audio_wanted || self.audio.is_some() {
                            let d = self.make(identity, self.audio.clone());
                            self.current = Some(d.clone());
                            return Some(DescEvent::Ready(d));
                        }
                        self.pending.get_or_insert((identity, host_us));
                    }
                }
            }
        }
        self.check_timeout(host_us)
    }

    /// The header of an AAC frame
    pub fn audio(&mut self, adts: &AdtsHeader, host_us: u64) -> Option<DescEvent> {
        let desc = AudioDesc::from_adts(adts);
        let first = self.audio.is_none();
        // Compare with what we saw, or else with the cached desc's audio
        let known = self
            .audio
            .as_ref()
            .or_else(|| self.current.as_ref().and_then(|c| c.audio.as_ref()));
        let changed = known.is_some_and(|a| a.asc != desc.asc);
        self.audio = Some(desc.clone());
        if let Some((identity, _)) = self.pending.take() {
            let d = self.make(identity, Some(desc));
            self.current = Some(d.clone());
            return Some(DescEvent::Ready(d));
        }
        match self.current.clone() {
            Some(mut cur) if changed => {
                self.gen += 1;
                cur.audio = Some(desc);
                cur.gen = self.gen;
                self.current = Some(cur.clone());
                Some(DescEvent::Changed(cur))
            }
            Some(mut cur) if first && cur.audio.is_none() => {
                cur.audio = Some(desc);
                self.current = Some(cur.clone());
                Some(DescEvent::LateAudio(cur))
            }
            _ => self.check_timeout(host_us),
        }
    }

    /// Ready without audio 1 s after the first IRAP
    pub fn check_timeout(&mut self, host_us: u64) -> Option<DescEvent> {
        let (_, since) = self.pending.as_ref()?;
        if host_us.saturating_sub(*since) < AUDIO_WAIT_US {
            return None;
        }
        let (identity, _) = self.pending.take().unwrap();
        let d = self.make(identity, None);
        self.current = Some(d.clone());
        Some(DescEvent::Ready(d))
    }

    fn make(&self, video: VideoIdentity, audio: Option<AudioDesc>) -> StreamDesc {
        StreamDesc {
            video,
            audio,
            framerate: self.framerate,
            gen: self.gen,
        }
    }
}

/// Identity from an IRAP AU's parameter sets; `None` if they are missing or unparsable
pub fn identity_from(codec: VideoCodec, nals: &[Bytes]) -> Option<VideoIdentity> {
    let (mut vps, mut sps, mut pps) = (None, None, None);
    for nal in nals {
        match param_kind(codec, nal) {
            Some(ParamKind::Vps) => vps = Some(nal.to_vec()),
            Some(ParamKind::Sps) => sps = Some(nal.to_vec()),
            Some(ParamKind::Pps) => pps = Some(nal.to_vec()),
            None => {}
        }
    }
    if codec == VideoCodec::H265 && vps.is_none() {
        return None;
    }
    let sps = sps?;
    let PictureSize { width, height } = picture_size(codec, &sps).ok()?;
    Some(VideoIdentity {
        codec: codec.into(),
        params: ParamSets {
            vps,
            sps,
            pps: pps?,
        },
        width,
        height,
    })
}

// ---------------------------------------------------------------------------------------
// Cache

/// `{state_dir}/cache/{sha256(camera)[..16]}-{kind}/` holding `desc.json` and `irap.bin`
#[derive(Debug, Clone)]
pub struct DescCache {
    dir: PathBuf,
}

/// `irap.bin`: `NLIRAP1\n`, the desc hash (16 hex chars), `\n`, then per NAL a u32 BE
/// length and the bytes
const IRAP_MAGIC: &[u8] = b"NLIRAP1\n";

const IRAP_INTERVAL_US: u64 = 3_600_000_000;

impl DescCache {
    pub fn new(state_dir: &Path, camera: &str, kind: &str) -> Self {
        let id: String = Sha256::digest(camera.as_bytes())
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect();
        Self {
            dir: state_dir.join("cache").join(format!("{id}-{kind}")),
        }
    }

    pub fn load_desc(&self) -> Option<StreamDesc> {
        let b = std::fs::read(self.dir.join("desc.json")).ok()?;
        serde_json::from_slice(&b).ok()
    }

    pub fn save_desc(&self, desc: &StreamDesc) -> std::io::Result<()> {
        write_atomic(
            &self.dir.join("desc.json"),
            &serde_json::to_vec_pretty(desc)?,
        )
    }

    /// The cached IRAP, only if it belongs to `desc`
    pub fn load_irap(&self, desc: &StreamDesc) -> Option<Vec<Bytes>> {
        let b = Bytes::from(std::fs::read(self.dir.join("irap.bin")).ok()?);
        let hash = desc.hash();
        let head = IRAP_MAGIC.len() + hash.len() + 1;
        if b.len() < head || &b[..IRAP_MAGIC.len()] != IRAP_MAGIC {
            return None;
        }
        if &b[IRAP_MAGIC.len()..head - 1] != hash.as_bytes() {
            return None;
        }
        let mut nals = vec![];
        let mut i = head;
        while i + 4 <= b.len() {
            let len = u32::from_be_bytes(b[i..i + 4].try_into().unwrap()) as usize;
            i += 4;
            if i + len > b.len() {
                return None;
            }
            nals.push(b.slice(i..i + len));
            i += len;
        }
        (i == b.len() && !nals.is_empty()).then_some(nals)
    }

    pub fn save_irap(&self, desc: &StreamDesc, nals: &[Bytes]) -> std::io::Result<()> {
        let mut out = IRAP_MAGIC.to_vec();
        out.extend_from_slice(desc.hash().as_bytes());
        out.push(b'\n');
        for n in nals {
            out.extend_from_slice(&(n.len() as u32).to_be_bytes());
            out.extend_from_slice(n);
        }
        write_atomic(&self.dir.join("irap.bin"), &out)
    }
}

/// When to write `irap.bin`: on a desc change, at most hourly, and at shutdown
/// (SD-card wear, F-ds 11)
#[derive(Debug, Default)]
pub struct IrapWritePolicy {
    last_us: Option<u64>,
}

impl IrapWritePolicy {
    pub fn should_write(&mut self, now_us: u64, desc_changed: bool, shutdown: bool) -> bool {
        let due = match self.last_us {
            None => true,
            Some(t) => now_us.saturating_sub(t) >= IRAP_INTERVAL_US,
        };
        if desc_changed || shutdown || due {
            self.last_us = Some(now_us);
            true
        } else {
            false
        }
    }
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn argus_sub_irap() -> Vec<Bytes> {
        vec![
            Bytes::from(hex("67640029ac1514a0e01064")),
            Bytes::from(hex("68ee3cb0")),
            Bytes::from_static(&[0x65, 0x88, 0x84]),
        ]
    }

    fn adts_mono_16k() -> AdtsHeader {
        AdtsHeader::parse(&[0xff, 0xf1, 0x60, 0x40, 0x41, 0x7f, 0xfc]).unwrap()
    }

    #[test]
    fn test_ready_with_audio() {
        let mut l = DescLearner::new(true, None);
        assert_eq!(
            l.video(VideoCodec::H264, &argus_sub_irap(), true, 0, 0),
            None
        );
        match l.audio(&adts_mono_16k(), 30_000) {
            Some(DescEvent::Ready(d)) => {
                assert_eq!((d.video.width, d.video.height), (896, 512));
                assert_eq!(d.audio.unwrap().asc, vec![0x14, 0x08]);
                assert_eq!(d.gen, 0);
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn test_ready_without_audio_after_1s() {
        let mut l = DescLearner::new(true, None);
        assert_eq!(
            l.video(VideoCodec::H264, &argus_sub_irap(), true, 0, 0),
            None
        );
        let p = [Bytes::from_static(&[0x41, 0x9a])];
        assert_eq!(l.video(VideoCodec::H264, &p, false, 66_667, 500_000), None);
        assert!(matches!(
            l.video(VideoCodec::H264, &p, false, 133_333, 1_000_000),
            Some(DescEvent::Ready(StreamDesc { audio: None, .. }))
        ));
        // Audio showing up later is for the next DESCRIBE
        assert!(matches!(
            l.audio(&adts_mono_16k(), 1_100_000),
            Some(DescEvent::LateAudio(StreamDesc { audio: Some(_), .. }))
        ));
    }

    #[test]
    fn test_audio_none_ready_at_irap() {
        let mut l = DescLearner::new(false, None);
        assert!(matches!(
            l.video(VideoCodec::H264, &argus_sub_irap(), true, 0, 0),
            Some(DescEvent::Ready(_))
        ));
    }

    #[test]
    fn test_param_change_bumps_gen() {
        let mut l = DescLearner::new(false, None);
        l.video(VideoCodec::H264, &argus_sub_irap(), true, 0, 0);
        // Same params again: nothing
        assert_eq!(
            l.video(
                VideoCodec::H264,
                &argus_sub_irap(),
                true,
                2_000_000,
                2_000_000
            ),
            None
        );
        let mut other = argus_sub_irap();
        other[1] = Bytes::from(hex("68ee3cb1"));
        match l.video(VideoCodec::H264, &other, true, 4_000_000, 4_000_000) {
            Some(DescEvent::Changed(d)) => assert_eq!(d.gen, 1),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn test_framerate_from_first_30_frames() {
        let mut l = DescLearner::new(false, None);
        let p = [Bytes::from_static(&[0x41, 0x9a])];
        for i in 0..31u64 {
            l.video(VideoCodec::H264, &p, false, i * 66_667, i * 66_667);
        }
        l.video(
            VideoCodec::H264,
            &argus_sub_irap(),
            true,
            31 * 66_667,
            31 * 66_667,
        );
        assert_eq!(l.current().unwrap().framerate, Some(15));
    }

    #[test]
    fn test_cache_roundtrip_and_irap_binding() {
        let dir = std::env::temp_dir().join(format!("neolink-rtsp-cache-{}", std::process::id()));
        let cache = DescCache::new(&dir, "argus", "main");
        let mut l = DescLearner::new(false, None);
        let Some(DescEvent::Ready(desc)) = l.video(VideoCodec::H264, &argus_sub_irap(), true, 0, 0)
        else {
            panic!()
        };
        assert!(cache.load_desc().is_none());
        cache.save_desc(&desc).unwrap();
        cache.save_irap(&desc, &argus_sub_irap()).unwrap();
        assert_eq!(cache.load_desc().unwrap(), desc);
        assert_eq!(cache.load_irap(&desc).unwrap(), argus_sub_irap());
        // An IRAP of another desc is ignored
        let mut other = desc.clone();
        other.video.width = 640;
        assert!(cache.load_irap(&other).is_none());
        // A learner seeded from the cache keeps the gen and doesn't re-announce
        let mut l2 = DescLearner::new(false, Some(desc.clone()));
        assert_eq!(
            l2.video(VideoCodec::H264, &argus_sub_irap(), true, 0, 0),
            None
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_cached_audio_change_is_detected() {
        let mut l = DescLearner::new(true, None);
        l.video(VideoCodec::H264, &argus_sub_irap(), true, 0, 0);
        let Some(DescEvent::Ready(d)) = l.audio(&adts_mono_16k(), 1) else {
            panic!()
        };
        // A new process, the cache says 16 kHz, the camera now sends 48 kHz
        let mut l2 = DescLearner::new(true, Some(d));
        let h48 = AdtsHeader::parse(&[0xff, 0xf1, 0x4c, 0x40, 0x41, 0x7f, 0xfc]).unwrap();
        assert_eq!(h48.sample_rate(), 48000);
        assert!(matches!(l2.audio(&h48, 2), Some(DescEvent::Changed(_))));
    }

    #[test]
    fn test_irap_write_policy() {
        let mut p = IrapWritePolicy::default();
        assert!(p.should_write(0, false, false));
        assert!(!p.should_write(60_000_000, false, false));
        assert!(p.should_write(61_000_000, true, false));
        assert!(!p.should_write(62_000_000, false, false));
        assert!(p.should_write(63_000_000, false, true));
        assert!(p.should_write(63_000_000 + IRAP_INTERVAL_US, false, false));
    }
}
