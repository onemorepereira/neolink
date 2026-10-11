#![no_main]
//! The request parser never panics, whatever the input and however it is chunked, and
//! never yields more bytes than it was given
use libfuzzer_sys::fuzz_target;
use neolink_rtsp::rtsp::msg::{parse, Parsed};

fuzz_target!(|data: &[u8]| {
    // The first byte picks a chunk size, so splits land anywhere
    let Some((&step, data)) = data.split_first() else {
        return;
    };
    let step = (step as usize % 64) + 1;
    let mut buf = bytes::BytesMut::new();
    let mut out = 0usize;
    for chunk in data.chunks(step) {
        buf.extend_from_slice(chunk);
        loop {
            match parse(&mut buf) {
                Ok(Some(Parsed::Request(r))) => out += r.body.len(),
                Ok(Some(Parsed::Interleaved { data, .. })) => out += data.len(),
                Ok(None) => break,
                Err(_) => return,
            }
        }
    }
    assert!(out <= data.len());
});
