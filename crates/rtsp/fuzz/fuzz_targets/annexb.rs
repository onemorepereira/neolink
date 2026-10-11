#![no_main]
//! Annex-B splitting and packetizing never panic, and packets respect the MTU
use libfuzzer_sys::fuzz_target;
use neolink_rtsp::codec::annexb::{split_nals, VideoCodec};
use neolink_rtsp::rtp::packetize;

fuzz_target!(|data: &[u8]| {
    let buf = bytes::Bytes::copy_from_slice(data);
    let nals = split_nals(&buf);
    for codec in [VideoCodec::H264, VideoCodec::H265] {
        for c in packetize::video(codec, &nals, 200) {
            assert!(c.len() + packetize::RTP_HEADER <= 200);
        }
    }
});
