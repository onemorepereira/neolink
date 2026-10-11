#![no_main]
//! ADTS parsing and splitting never panic
use libfuzzer_sys::fuzz_target;
use neolink_rtsp::codec::aac::adts_access_units;

fuzz_target!(|data: &[u8]| {
    let buf = bytes::Bytes::copy_from_slice(data);
    if let Ok((h, aus)) = adts_access_units(&buf) {
        assert!(aus.iter().map(|a| a.len()).sum::<usize>() <= h.frame_length);
    }
});
