#![no_main]
//! SPS parsing never panics and gives sane sizes
use libfuzzer_sys::fuzz_target;
use neolink_rtsp::codec::annexb::VideoCodec;
use neolink_rtsp::codec::sps::picture_size;

fuzz_target!(|data: &[u8]| {
    for codec in [VideoCodec::H264, VideoCodec::H265] {
        if let Ok(s) = picture_size(codec, data) {
            assert!(s.width > 0 && s.width <= 16384 && s.height > 0 && s.height <= 16384);
        }
    }
});
