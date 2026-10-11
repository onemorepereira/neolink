//! # neolink_rtsp
//!
//! A pure-Rust RTSP 1.0 server for live camera streams. It knows nothing about Baichuan or
//! neolink; see `docs/native-rtsp/DESIGN.md`.

pub mod codec;
pub mod hub;
pub mod rtp;
pub mod source;
