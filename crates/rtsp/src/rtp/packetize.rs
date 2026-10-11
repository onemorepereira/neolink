//! RTP payload packetizers (SPEC R2, R3; DESIGN §5.1)
//!
//! Frames are packetized once in the Hub and shared by every session (DESIGN §2): a chunk
//! is the RTP payload minus nothing but the 12-byte header, as a ≤ 4-byte prefix plus a
//! zero-copy slice of the NAL or AU.

use crate::codec::annexb::VideoCodec;
use bytes::Bytes;

/// One RTP packet's payload: `prefix[..prefix_len]` followed by `body`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpChunk {
    pub marker: bool,
    pub prefix: [u8; 4],
    pub prefix_len: u8,
    pub body: Bytes,
}

impl RtpChunk {
    fn new(marker: bool, prefix: &[u8], body: Bytes) -> Self {
        let mut p = [0u8; 4];
        p[..prefix.len()].copy_from_slice(prefix);
        Self {
            marker,
            prefix: p,
            prefix_len: prefix.len() as u8,
            body,
        }
    }

    pub fn prefix(&self) -> &[u8] {
        &self.prefix[..self.prefix_len as usize]
    }

    /// Payload length (prefix + body)
    pub fn len(&self) -> usize {
        self.prefix_len as usize + self.body.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// RTP header size
pub const RTP_HEADER: usize = 12;

/// Packetize one video access unit (its NALs, without start codes). The marker is set on
/// the last packet of the AU. `mtu` is the whole RTP packet (header + payload) limit.
pub fn video(codec: VideoCodec, nals: &[Bytes], mtu: usize) -> Vec<RtpChunk> {
    let max = mtu.saturating_sub(RTP_HEADER).max(16);
    let mut out = vec![];
    for (i, nal) in nals.iter().enumerate() {
        let last_nal = i + 1 == nals.len();
        let hdr = match codec {
            VideoCodec::H264 => 1,
            VideoCodec::H265 => 2,
        };
        if nal.len() < hdr {
            continue;
        }
        if nal.len() <= max {
            out.push(RtpChunk::new(last_nal, &[], nal.clone()));
            continue;
        }
        // Fragmentation: H.264 FU-A (RFC 6184 §5.8), H.265 FU (RFC 7798 §4.4.3)
        let (prefix_base, nal_type): (Vec<u8>, u8) = match codec {
            VideoCodec::H264 => (vec![(nal[0] & 0xe0) | 28], nal[0] & 0x1f),
            VideoCodec::H265 => (
                vec![(nal[0] & 0x81) | (49 << 1), nal[1]],
                (nal[0] >> 1) & 0x3f,
            ),
        };
        let payload = nal.slice(hdr..);
        let room = max - prefix_base.len() - 1;
        let mut off = 0;
        while off < payload.len() {
            let end = (off + room).min(payload.len());
            let start = off == 0;
            let stop = end == payload.len();
            let fu = (if start { 0x80 } else { 0 }) | (if stop { 0x40 } else { 0 }) | nal_type;
            let mut prefix = prefix_base.clone();
            prefix.push(fu);
            out.push(RtpChunk::new(
                last_nal && stop,
                &prefix,
                payload.slice(off..end),
            ));
            off = end;
        }
    }
    out
}

/// Packetize one raw AAC AU as RFC 3640 AAC-hbr (`sizelength=13; indexlength=3;
/// indexdeltalength=3`): one AU per packet, fragmented over several packets (same AU
/// header, marker on the last) when it doesn't fit. AUs over 8191 bytes are dropped.
pub fn aac(au: &Bytes, mtu: usize) -> Vec<RtpChunk> {
    if au.is_empty() || au.len() > 8191 {
        return vec![];
    }
    let size = au.len() as u16;
    // AU-headers-length = 16 bits, then AU-size (13) | AU-Index (3) = 0
    let prefix = [0x00, 0x10, (size >> 5) as u8, ((size & 0x1f) << 3) as u8];
    let room = mtu.saturating_sub(RTP_HEADER).max(16) - prefix.len();
    let mut out = vec![];
    let mut off = 0;
    while off < au.len() {
        let end = (off + room).min(au.len());
        out.push(RtpChunk::new(end == au.len(), &prefix, au.slice(off..end)));
        off = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reassemble what a receiver would: (NAL list, marker positions)
    fn depacketize(codec: VideoCodec, chunks: &[RtpChunk]) -> Vec<Vec<u8>> {
        let mut nals = vec![];
        let mut cur: Option<Vec<u8>> = None;
        for c in chunks {
            let p: Vec<u8> = [c.prefix(), &c.body[..]].concat();
            let (is_fu, fu_hdr_len) = match codec {
                VideoCodec::H264 => (p[0] & 0x1f == 28, 2),
                VideoCodec::H265 => ((p[0] >> 1) & 0x3f == 49, 3),
            };
            if !is_fu {
                nals.push(p);
                continue;
            }
            let fu = p[fu_hdr_len - 1];
            if fu & 0x80 != 0 {
                let header = match codec {
                    VideoCodec::H264 => vec![(p[0] & 0xe0) | (fu & 0x1f)],
                    VideoCodec::H265 => vec![(p[0] & 0x81) | ((fu & 0x3f) << 1), p[1]],
                };
                cur = Some(header);
            }
            cur.as_mut().unwrap().extend_from_slice(&p[fu_hdr_len..]);
            if fu & 0x40 != 0 {
                nals.push(cur.take().unwrap());
            }
        }
        nals
    }

    fn nal(codec: VideoCodec, len: usize) -> Bytes {
        let mut v: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        match codec {
            VideoCodec::H264 => v[0] = 0x65,
            VideoCodec::H265 => {
                v[0] = 0x26;
                v[1] = 0x01;
            }
        }
        Bytes::from(v)
    }

    #[test]
    fn test_video_roundtrip_and_limits() {
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            for mtu in [200, 1400] {
                let nals = vec![nal(codec, 50), nal(codec, 5000), nal(codec, 1388)];
                let chunks = video(codec, &nals, mtu);
                assert!(
                    chunks.iter().all(|c| c.len() + RTP_HEADER <= mtu),
                    "{:?} {}",
                    codec,
                    mtu
                );
                let markers: Vec<bool> = chunks.iter().map(|c| c.marker).collect();
                assert_eq!(markers.iter().filter(|m| **m).count(), 1);
                assert!(*markers.last().unwrap());
                let back = depacketize(codec, &chunks);
                assert_eq!(back, nals.iter().map(|n| n.to_vec()).collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn test_fu_headers() {
        // H.264 FU-A indicator keeps F/NRI, type 28; FU header S/E bits and type
        let n = nal(VideoCodec::H264, 3000);
        let c = video(VideoCodec::H264, &[n], 1400);
        assert_eq!(c[0].prefix(), &[0x7c, 0x85]);
        assert_eq!(c[1].prefix(), &[0x7c, 0x05]);
        assert_eq!(c.last().unwrap().prefix(), &[0x7c, 0x45]);
        // H.265 FU: PayloadHdr type 49 with the layer/TID bits, FU header type 19
        let n = nal(VideoCodec::H265, 3000);
        let c = video(VideoCodec::H265, &[n], 1400);
        assert_eq!(c[0].prefix(), &[0x62, 0x01, 0x93]);
        assert_eq!(c.last().unwrap().prefix(), &[0x62, 0x01, 0x53]);
    }

    #[test]
    fn test_body_is_zero_copy() {
        let n = nal(VideoCodec::H264, 3000);
        let c = video(VideoCodec::H264, std::slice::from_ref(&n), 1400);
        let base = n.as_ptr() as usize;
        let p = c[1].body.as_ptr() as usize;
        assert!(
            p > base && p < base + n.len(),
            "FU body is not a slice of the NAL"
        );
    }

    #[test]
    fn test_aac() {
        let au = Bytes::from(vec![7u8; 300]);
        let c = aac(&au, 1400);
        assert_eq!(c.len(), 1);
        assert!(c[0].marker);
        // 300 = 0b1_0010_1100: size<<3 = 0x0960
        assert_eq!(c[0].prefix(), &[0x00, 0x10, 0x09, 0x60]);
        let big = Bytes::from(vec![1u8; 3000]);
        let c = aac(&big, 1400);
        assert_eq!(c.len(), 3);
        assert_eq!(c.iter().filter(|c| c.marker).count(), 1);
        assert_eq!(c.iter().map(|c| c.body.len()).sum::<usize>(), 3000);
        assert!(aac(&Bytes::from(vec![0u8; 9000]), 1400).is_empty());
    }
}
