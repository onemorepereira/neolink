//! Annex-B access units: NAL splitting and the R2 whitelist

use bytes::Bytes;
use std::collections::HashSet;

/// Video codec of a stream
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    H264,
    H265,
}

/// The NAL units of an Annex-B buffer, without start codes, as zero-copy slices
///
/// Handles 3- and 4-byte start codes. Trailing zero bytes of a NAL (`trailing_zero_8bits`
/// before the next start code) are trimmed; empty NALs are skipped.
pub fn split_nals(data: &Bytes) -> Vec<Bytes> {
    let d = &data[..];
    let mut starts = vec![];
    let mut i = 0;
    while i + 3 <= d.len() {
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = vec![];
    for (n, &s) in starts.iter().enumerate() {
        let mut end = match starts.get(n + 1) {
            Some(&next) => next - 3,
            None => d.len(),
        };
        while end > s && d[end - 1] == 0 {
            end -= 1;
        }
        if end > s {
            out.push(data.slice(s..end));
        }
    }
    out
}

/// NAL unit type (and, for H.265, `nuh_layer_id`)
pub fn nal_type(codec: VideoCodec, nal: &[u8]) -> Option<(u8, u8)> {
    match codec {
        VideoCodec::H264 => nal.first().map(|b| (b & 0x1f, 0)),
        VideoCodec::H265 => {
            if nal.len() < 2 {
                return None;
            }
            Some(((nal[0] >> 1) & 0x3f, ((nal[0] & 1) << 5) | (nal[1] >> 3)))
        }
    }
}

/// Whether the NAL header is one a conforming encoder can produce: `forbidden_zero_bit`
/// clear, and for H.265 `nuh_temporal_id_plus1` non-zero. Anything else is corruption.
fn header_valid(codec: VideoCodec, nal: &[u8]) -> bool {
    match codec {
        VideoCodec::H264 => nal.first().is_some_and(|b| b & 0x80 == 0),
        VideoCodec::H265 => nal.len() >= 2 && nal[0] & 0x80 == 0 && nal[1] & 0x07 != 0,
    }
}

/// Whether a NAL may be sent (SPEC R2): H.264 types 1–13; H.265 types 0–9, 16–21 and
/// 32–40 with `nuh_layer_id = 0`; never a NAL with a corrupt header ([`header_valid`])
pub fn allowed(codec: VideoCodec, nal: &[u8]) -> bool {
    if !header_valid(codec, nal) {
        return false;
    }
    match (codec, nal_type(codec, nal)) {
        (VideoCodec::H264, Some((t, _))) => (1..=13).contains(&t),
        (VideoCodec::H265, Some((t, 0))) => {
            (0..=9).contains(&t) || (16..=21).contains(&t) || (32..=40).contains(&t)
        }
        _ => false,
    }
}

/// Whether the NAL is an IRAP/IDR picture slice
pub fn is_irap(codec: VideoCodec, nal: &[u8]) -> bool {
    match (codec, nal_type(codec, nal)) {
        (VideoCodec::H264, Some((5, _))) => true,
        (VideoCodec::H265, Some((t, _))) => (16..=23).contains(&t),
        _ => false,
    }
}

/// Parameter set kind of a NAL, if it is one
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    Vps,
    Sps,
    Pps,
}

pub fn param_kind(codec: VideoCodec, nal: &[u8]) -> Option<ParamKind> {
    match (codec, nal_type(codec, nal)?) {
        (VideoCodec::H264, (7, _)) => Some(ParamKind::Sps),
        (VideoCodec::H264, (8, _)) => Some(ParamKind::Pps),
        (VideoCodec::H265, (32, 0)) => Some(ParamKind::Vps),
        (VideoCodec::H265, (33, 0)) => Some(ParamKind::Sps),
        (VideoCodec::H265, (34, 0)) => Some(ParamKind::Pps),
        _ => None,
    }
}

/// Applies the whitelist and logs each filtered type once per stream
#[derive(Debug, Default)]
pub struct NalFilter {
    logged: HashSet<(u8, u8)>,
    /// How many NALs were dropped
    pub dropped: u64,
}

impl NalFilter {
    pub fn keep(&mut self, codec: VideoCodec, nal: &[u8], stream: &str) -> bool {
        if allowed(codec, nal) {
            return true;
        }
        self.dropped += 1;
        let t = nal_type(codec, nal).unwrap_or((255, 0));
        if self.logged.insert(t) {
            log::info!(
                "{stream}: dropping {codec:?} NAL type {} (layer {}); logged once",
                t.0,
                t.1
            );
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_nals() {
        let data = Bytes::from_static(&[
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 0, 1, 0x65, 9, 0, 0,
        ]);
        let nals = split_nals(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(&nals[0][..], &[0x67, 1, 2]);
        assert_eq!(&nals[1][..], &[0x68, 3]);
        assert_eq!(&nals[2][..], &[0x65, 9]);
        assert!(split_nals(&Bytes::from_static(&[1, 2, 3])).is_empty());
        assert!(split_nals(&Bytes::from_static(&[0, 0, 1])).is_empty());
    }

    #[test]
    fn test_whitelist() {
        assert!(allowed(VideoCodec::H264, &[0x65]));
        assert!(!allowed(VideoCodec::H264, &[0x0e])); // 14: prefix NAL (SVC)
        assert!(allowed(VideoCodec::H265, &[0x40, 0x01])); // VPS
        assert!(allowed(VideoCodec::H265, &[0x26, 0x01])); // IDR_W_RADL
        assert!(!allowed(VideoCodec::H265, &[0x7c, 0x01])); // 62: unspecified
        assert!(!allowed(VideoCodec::H265, &[0x26, 0x09])); // layer 1
        assert!(allowed(VideoCodec::H265, &[0x4e, 0x01])); // 39: prefix SEI
        assert!(!allowed(VideoCodec::H265, &[0x52, 0x01])); // 41: reserved
    }

    #[test]
    fn test_whitelist_rejects_corrupt_headers() {
        // H.264 forbidden_zero_bit
        assert!(allowed(VideoCodec::H264, &[0x67, 0x64]));
        assert!(!allowed(VideoCodec::H264, &[0xe7, 0x64])); // SPS
        assert!(!allowed(VideoCodec::H264, &[0xe5, 0x88])); // IDR
                                                            // H.265 forbidden_zero_bit (F)
        assert!(!allowed(VideoCodec::H265, &[0xc2, 0x01])); // SPS
        assert!(!allowed(VideoCodec::H265, &[0xa6, 0x01])); // IDR_W_RADL
                                                            // H.265 nuh_temporal_id_plus1 = 0 is forbidden
        assert!(!allowed(VideoCodec::H265, &[0x42, 0x00])); // SPS
        assert!(!allowed(VideoCodec::H265, &[0x26, 0x00])); // IDR_W_RADL
        assert!(allowed(VideoCodec::H265, &[0x02, 0x02])); // TRAIL_R, temporal id 1
    }

    #[test]
    fn test_filter_counts() {
        let mut f = NalFilter::default();
        assert!(!f.keep(VideoCodec::H264, &[0x0e], "t"));
        assert!(!f.keep(VideoCodec::H264, &[0x0e], "t"));
        assert_eq!(f.dropped, 2);
    }
}
