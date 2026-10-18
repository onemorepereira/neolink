//! Stream-based snapshot for cameras that do not respond to MSG_ID_SNAP.
//!
//! `GET /<camera>/snapshot.jpg` normally comes from a SNAP command (MSG_ID 109).
//! The Argus PT Ultra and similar battery cameras ignore that command entirely.
//! This module provides a fallback: start the sub-stream, capture the first
//! I-frame, decode it to JPEG using pure-Rust crates.
//!
//! No GStreamer, no FFI, no shell — stays FROM-scratch compatible.
//! Compiled only with the `native-snapshot` feature.

use anyhow::{Context, Result};
use neolink_core::{
    bc_protocol::StreamKind,
    bcmedia::model::{BcMedia, VideoType},
};

use crate::common::NeoInstance;

/// Pull one I-frame from the sub-stream and return it as a JPEG byte vector.
///
/// Uses `run_task_for` with a permit so the connection stays alive.
/// Should be called while the camera is already awake (inside `wake_reads`).
pub(crate) async fn snapshot_from_stream(instance: &NeoInstance) -> Result<Vec<u8>> {
    let (video_type, data) = instance
        .run_task_for("snap_stream:snapshot", |cam| {
            Box::pin(async move {
                // Sub-stream: lower resolution → smaller keyframe, faster I-frame
                let mut stream = cam
                    .start_video(StreamKind::Sub, 0, false)
                    .await
                    .context("start_video (sub-stream)")?;
                loop {
                    let pkt = stream.get_data().await??;
                    if let BcMedia::Iframe(iframe) = pkt {
                        cam.stop_video(StreamKind::Sub)
                            .await
                            .context("stop_video after iframe")?;
                        return Ok((iframe.video_type, iframe.data));
                    }
                }
            })
        })
        .await?;

    decode_iframe_to_jpeg(video_type, &data)
}

/// Decode one Annex-B I-frame to JPEG.
fn decode_iframe_to_jpeg(video_type: VideoType, data: &[u8]) -> Result<Vec<u8>> {
    match video_type {
        VideoType::H264 => decode_h264(data),
        VideoType::H265 => decode_h265(data),
    }
}

fn decode_h264(data: &[u8]) -> Result<Vec<u8>> {
    use rusty_h264_decoder::Decoder;

    let mut dec = Decoder::default();
    let frames = dec
        .decode_stream(data)
        .map_err(|e| anyhow::anyhow!("H.264 decode: {e:?}"))?;

    let frame = frames
        .into_iter()
        .next()
        .context("H.264 stream contained no decodable frame")?;

    // YuvFrame has tight strides: y.len() == width * height
    let chroma_w = (frame.width + 1) / 2;
    yuv420_to_jpeg(&frame.y, &frame.u, &frame.v, frame.width, frame.height, frame.width, chroma_w)
}

fn decode_h265(data: &[u8]) -> Result<Vec<u8>> {
    use rusty_h265::Decoder;

    let mut dec = Decoder::new();
    dec.push_annexb(data, None)
        .map_err(|e| anyhow::anyhow!("H.265 push_annexb: {e:?}"))?;
    dec.flush();

    let frame = dec
        .next_frame()
        .map_err(|e| anyhow::anyhow!("H.265 no frame: {e:?}"))?;

    let width = frame.width;
    let height = frame.height;
    let chroma_w = (width + 1) / 2;
    let chroma_h = (height + 1) / 2;

    // write_yuv writes packed planar: Y then Cb then Cr, u8 for 8-bit
    let mut yuv = Vec::new();
    frame.write_yuv(&mut yuv);

    let y_len = width * height;
    let c_len = chroma_w * chroma_h;

    if yuv.len() < y_len + 2 * c_len {
        return Err(anyhow::anyhow!(
            "H.265 write_yuv output too short: {} bytes for {}x{}",
            yuv.len(),
            width,
            height
        ));
    }

    let y = &yuv[..y_len];
    let cb = &yuv[y_len..y_len + c_len];
    let cr = &yuv[y_len + c_len..y_len + 2 * c_len];

    yuv420_to_jpeg(y, cb, cr, width, height, width, chroma_w)
}

/// Convert a planar YUV 4:2:0 frame to JPEG.
///
/// `y_stride` is the row width of the Y plane (may differ from `width` for
/// H.264 frames with alignment padding, though `rusty_h264-decoder` uses
/// tight strides today). `chroma_w` is the row width of the Cb/Cr planes.
fn yuv420_to_jpeg(
    y: &[u8],
    cb: &[u8],
    cr: &[u8],
    width: usize,
    height: usize,
    y_stride: usize,
    chroma_w: usize,
) -> Result<Vec<u8>> {
    let mut rgb = vec![0u8; width * height * 3];

    for row in 0..height {
        for col in 0..width {
            let yv = y[row * y_stride + col] as f32;
            let cbi = cb[(row / 2) * chroma_w + (col / 2)] as f32 - 128.0;
            let cri = cr[(row / 2) * chroma_w + (col / 2)] as f32 - 128.0;

            let r = (yv + 1.402 * cri).clamp(0.0, 255.0) as u8;
            let g = (yv - 0.344_136 * cbi - 0.714_136 * cri).clamp(0.0, 255.0) as u8;
            let b = (yv + 1.772 * cbi).clamp(0.0, 255.0) as u8;

            let i = (row * width + col) * 3;
            rgb[i] = r;
            rgb[i + 1] = g;
            rgb[i + 2] = b;
        }
    }

    let mut out = Vec::new();
    let mut enc = jpeg_encoder::Encoder::new(&mut out, 85);
    enc.encode(&rgb, width as u16, height as u16, jpeg_encoder::ColorType::Rgb)
        .context("JPEG encode")?;

    Ok(out)
}
