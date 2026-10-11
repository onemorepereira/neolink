//! SDP for DESCRIBE (DESIGN §5.4)
//!
//! Matches the GStreamer backend's structure (golden transcripts) except for the listed
//! deltas: `s=`/`a=tool` say neolink, no `a=ts-refclk`/`a=mediaclk`/`a=ssrc`,
//! `a=framerate` added, and audio is MPEG4-GENERIC (AAC passthrough) instead of L16.

use crate::hub::desc::{StreamDesc, VideoCodecSer};
use base64::{engine::general_purpose::STANDARD as B64, Engine};

/// The SDP body. `session_id` goes in `o=`; `server_ip` is the address the client used.
pub fn sdp(desc: &StreamDesc, session_id: u64, server_ip: &str) -> String {
    let ip_kind = if server_ip.contains(':') {
        "IP6"
    } else {
        "IP4"
    };
    let mut s = format!(
        "v=0\r\no=- {session_id} 1 IN {ip_kind} {server_ip}\r\ns=Session streamed by neolink\r\n\
i=rtsp-server\r\nt=0 0\r\na=tool:neolink\r\na=type:broadcast\r\na=control:*\r\na=range:npt=0-\r\n"
    );
    let v = &desc.video;
    s.push_str("m=video 0 RTP/AVP 96\r\nc=IN IP4 0.0.0.0\r\n");
    match v.codec {
        VideoCodecSer::H264 => {
            let sps = &v.params.sps;
            let profile_level_id = if sps.len() >= 4 {
                format!("{:02X}{:02X}{:02X}", sps[1], sps[2], sps[3])
            } else {
                "42E01F".to_string()
            };
            s.push_str("a=rtpmap:96 H264/90000\r\n");
            s.push_str(&format!(
                "a=fmtp:96 packetization-mode=1;sprop-parameter-sets={},{};profile-level-id={};level-asymmetry-allowed=1\r\n",
                B64.encode(sps),
                B64.encode(&v.params.pps),
                profile_level_id
            ));
        }
        VideoCodecSer::H265 => {
            s.push_str("a=rtpmap:96 H265/90000\r\n");
            s.push_str(&format!(
                "a=fmtp:96 sprop-vps={};sprop-sps={};sprop-pps={}\r\n",
                B64.encode(v.params.vps.as_deref().unwrap_or(&[])),
                B64.encode(&v.params.sps),
                B64.encode(&v.params.pps)
            ));
        }
    }
    if let Some(fps) = desc.framerate {
        s.push_str(&format!("a=framerate:{fps}\r\n"));
    }
    s.push_str("a=control:stream=0\r\n");
    if let Some(a) = &desc.audio {
        let config: String = a.asc.iter().map(|b| format!("{b:02x}")).collect();
        s.push_str("m=audio 0 RTP/AVP 96\r\nc=IN IP4 0.0.0.0\r\n");
        s.push_str(&format!(
            "a=rtpmap:96 MPEG4-GENERIC/{}/{}\r\n",
            a.sample_rate, a.channels
        ));
        s.push_str(&format!(
            "a=fmtp:96 streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config={config}\r\n"
        ));
        s.push_str("a=control:stream=1\r\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::desc::{AudioDesc, ParamSets, VideoIdentity};

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn argus_sub() -> StreamDesc {
        StreamDesc {
            video: VideoIdentity {
                codec: VideoCodecSer::H264,
                params: ParamSets {
                    vps: None,
                    sps: hex("67640029ac1514a0e01064"),
                    pps: hex("68ee3cb0"),
                },
                width: 896,
                height: 512,
            },
            audio: Some(AudioDesc {
                asc: vec![0x14, 0x08],
                sample_rate: 16000,
                channels: 1,
            }),
            framerate: Some(15),
            gen: 0,
        }
    }

    #[test]
    fn test_h264_sdp_matches_golden_fmtp() {
        let s = sdp(&argus_sub(), 42, "127.0.0.1");
        // The GStreamer golden's fmtp (spec §9), byte for byte
        assert!(s.contains("a=fmtp:96 packetization-mode=1;sprop-parameter-sets=Z2QAKawVFKDgEGQ=,aO48sA==;profile-level-id=640029;level-asymmetry-allowed=1\r\n"), "{s}");
        assert!(s.contains("a=rtpmap:96 MPEG4-GENERIC/16000/1\r\n"));
        assert!(s.contains("config=1408\r\n"));
        assert!(s.contains("a=control:*\r\n"));
        assert!(s.contains("a=control:stream=0\r\n") && s.contains("a=control:stream=1\r\n"));
        assert!(s.contains("a=framerate:15\r\n"));
        assert!(s.starts_with("v=0\r\no=- 42 1 IN IP4 127.0.0.1\r\n"));
    }

    #[test]
    fn test_h265_sdp() {
        let mut d = argus_sub();
        d.video.codec = VideoCodecSer::H265;
        d.video.params = ParamSets {
            vps: Some(hex("40010c01ffff016000000300000300000300000300 96ac09"
                .replace(' ', "")
                .as_str())),
            sps: hex("42010101600000030000030000030000030096a001e020021c7f8aad3ba24bb2"),
            pps: hex("4401c072f0941e3648"),
        };
        d.audio = None;
        let s = sdp(&d, 1, "::1");
        assert!(s.contains("IN IP6 ::1"));
        assert!(s.contains("a=rtpmap:96 H265/90000\r\n"));
        assert!(
            s.contains(
                "sprop-sps=QgEBAWAAAAMAAAMAAAMAAAMAlqAB4CACHH+KrTuiS7I=;sprop-pps=RAHAcvCUHjZI\r\n"
            ),
            "{s}"
        );
        assert!(!s.contains("m=audio"));
    }
}
