//! Capture BcMedia frames from a camera, print per-frame stats and write a fixture file
//!
//! Used to characterise camera streams (NAL types, timestamps, audio headers) and to
//! record fixtures that can be replayed in tests.
//!
//! ```bash
//! NEO_ADDR=192.168.1.10 NEO_USER=admin NEO_PASS=secret \
//!   cargo run -p neolink_core --example bcmedia_dump -- main 20 /tmp/main.bcm [sub]
//! ```
//!
//! Arguments: stream (main|sub|extern), seconds, output path, and optionally a second
//! stream to run concurrently (to check whether the camera allows it).
//!
//! The output is a `.bcm` fixture, see [`neolink_core::fixture`].

use neolink_core::bc_protocol::{
    BcCamera, BcCameraOpt, ConnectionProtocol, Credentials, DiscoveryMethods, StreamKind,
};
use neolink_core::bcmedia::model::*;
use neolink_core::fixture::{Kind, Record};
use std::time::{Duration, Instant};

fn stream_kind(s: &str) -> StreamKind {
    match s {
        "main" => StreamKind::Main,
        "sub" => StreamKind::Sub,
        "extern" => StreamKind::Extern,
        _ => panic!("unknown stream {}", s),
    }
}

/// NAL unit headers (type, layer id) of an Annex-B buffer
fn nal_types(data: &[u8], h265: bool) -> Vec<(u8, u8)> {
    let mut out = vec![];
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let b = data[i + 3];
            if h265 {
                let layer = ((b & 1) << 5) | data.get(i + 4).map(|n| n >> 3).unwrap_or(0);
                out.push(((b >> 1) & 0x3f, layer));
            } else {
                out.push((b & 0x1f, 0));
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

async fn capture(
    camera: &BcCamera,
    kind: StreamKind,
    secs: u64,
    out: Option<std::path::PathBuf>,
) -> Result<(), neolink_core::Error> {
    let name = format!("{:?}", kind);
    let mut file = out.map(|p| std::io::BufWriter::new(std::fs::File::create(p).unwrap()));
    let mut stream = camera.start_video(kind, 0, false).await?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        let media = match tokio::time::timeout(Duration::from_secs(10), stream.get_data()).await {
            Ok(r) => r??,
            Err(_) => {
                println!("{} timeout waiting for media", name);
                break;
            }
        };
        let host_us = start.elapsed().as_micros() as u64;
        let (kind, h265, cam_us, cam_time, data, desc) = match &media {
            BcMedia::InfoV1(i) => (
                Kind::InfoV1,
                false,
                0,
                0,
                [
                    &i.video_width.to_le_bytes()[..],
                    &i.video_height.to_le_bytes(),
                    &[i.fps],
                ]
                .concat(),
                format!("{}x{} fps={}", i.video_width, i.video_height, i.fps),
            ),
            BcMedia::InfoV2(i) => (
                Kind::InfoV2,
                false,
                0,
                0,
                [
                    &i.video_width.to_le_bytes()[..],
                    &i.video_height.to_le_bytes(),
                    &[i.fps],
                ]
                .concat(),
                format!("{}x{} fps={}", i.video_width, i.video_height, i.fps),
            ),
            BcMedia::Iframe(f) => {
                let h265 = matches!(f.video_type, VideoType::H265);
                (
                    Kind::Iframe,
                    h265,
                    f.microseconds,
                    f.time.unwrap_or(0),
                    f.data.clone(),
                    format!("{:?} nals={:?}", f.video_type, nal_types(&f.data, h265)),
                )
            }
            BcMedia::Pframe(f) => {
                let h265 = matches!(f.video_type, VideoType::H265);
                (
                    Kind::Pframe,
                    h265,
                    f.microseconds,
                    0,
                    f.data.clone(),
                    format!("{:?} nals={:?}", f.video_type, nal_types(&f.data, h265)),
                )
            }
            BcMedia::Aac(a) => {
                let d = &a.data;
                let desc = if d.len() >= 7 && d[0] == 0xff && d[1] & 0xf0 == 0xf0 {
                    format!(
                        "adts profile={} sr_idx={} ch={} frames={} dur_us={:?}",
                        (d[2] >> 6) + 1,
                        (d[2] >> 2) & 0xf,
                        ((d[2] & 1) << 2) | (d[3] >> 6),
                        (d[6] & 3) + 1,
                        a.duration()
                    )
                } else {
                    "not adts".to_string()
                };
                (Kind::Aac, false, 0, 0, d.clone(), desc)
            }
            BcMedia::Adpcm(a) => (
                Kind::Adpcm,
                false,
                0,
                0,
                a.data.clone(),
                format!("dur_us={:?}", a.duration()),
            ),
        };
        println!(
            "{} host_us={} kind={:?} cam_us={} cam_time={} len={} {}",
            name,
            host_us,
            kind,
            cam_us,
            cam_time,
            data.len(),
            desc
        );
        if let Some(f) = file.as_mut() {
            Record {
                host_us,
                kind,
                h265,
                cam_us,
                cam_time,
                data,
            }
            .write_to(f)
            .unwrap();
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), neolink_core::Error> {
    let _ = env_logger::builder().try_init();
    let args: Vec<String> = std::env::args().collect();
    let kind = stream_kind(args.get(1).map(|s| s.as_str()).unwrap_or("main"));
    let secs: u64 = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(20);
    let out = args.get(3).map(std::path::PathBuf::from);
    let second = args.get(4).map(|s| stream_kind(s));

    let options = BcCameraOpt {
        name: "dump".to_string(),
        channel_id: 0,
        addrs: vec![std::env::var("NEO_ADDR")
            .expect("NEO_ADDR")
            .parse()
            .unwrap()],
        port: None,
        uid: std::env::var("NEO_UID").ok(),
        protocol: ConnectionProtocol::TcpUdp,
        discovery: DiscoveryMethods::Local,
        credentials: Credentials {
            username: std::env::var("NEO_USER").unwrap_or_else(|_| "admin".to_string()),
            password: std::env::var("NEO_PASS").ok(),
        },
        debug: false,
        max_discovery_retries: 10,
    };
    let camera = BcCamera::new(&options).await?;
    camera.login().await?;

    match second {
        None => capture(&camera, kind, secs, out).await,
        Some(second) => {
            let (a, b) = tokio::join!(
                capture(&camera, kind, secs, out),
                capture(&camera, second, secs, None)
            );
            a.and(b)
        }
    }
}
