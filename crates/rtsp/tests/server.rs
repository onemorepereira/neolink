//! RTSP server integration tests over real sockets (PLAN 3.2, 3.3, 3.6, 3.7, 3.8)
//!
//! The ffmpeg tests run when `NEOLINK_FFMPEG` is set (CI sets it) and are skipped otherwise.

mod common;

use common::*;
use neolink_rtsp::rtsp::auth::{Schemes, User};
use neolink_rtsp::rtsp::server::ServerConfig;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_raw_session_play_pause_play_teardown() {
    let h = start(vec![], Schemes::Both, ServerConfig::default()).await;
    let base = format!("rtsp://{}/argus/main", h.addr);
    let mut c = Client::connect(h.addr).await;

    let r = c.request("OPTIONS", &base, &[]).await;
    assert_eq!(r.status, 200);
    assert!(r.header("Public").unwrap().contains("DESCRIBE"));

    let r = c
        .request("DESCRIBE", &base, &[("Accept", "application/sdp")])
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.header("Content-Base"), Some(format!("{base}/").as_str()));
    assert!(r.body.contains("a=rtpmap:96 H265/90000"), "{}", r.body);
    assert!(
        r.body.contains("a=control:stream=1"),
        "audio track: {}",
        r.body
    );

    // UDP only: 461, which makes ffmpeg and live555 retry over TCP
    let r = c
        .request(
            "SETUP",
            &format!("{base}/stream=0"),
            &[("Transport", "RTP/AVP;unicast;client_port=5000-5001")],
        )
        .await;
    assert_eq!(r.status, 461);
    let r = c
        .request(
            "SETUP",
            &format!("{base}/stream=0"),
            &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1")],
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(r
        .header("Transport")
        .unwrap()
        .starts_with("RTP/AVP/TCP;unicast;interleaved=0-1;ssrc="));
    let session = r
        .header("Session")
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    // A second SETUP with the session; same channels requested again get moved
    let r = c
        .request(
            "SETUP",
            &format!("{base}/stream=1"),
            &[
                ("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1"),
                ("Session", &session),
            ],
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(r.header("Transport").unwrap().contains("interleaved=2-3"));
    // Wrong session id: 454
    let r = c.request("PLAY", &base, &[("Session", "nope")]).await;
    assert_eq!(r.status, 454);

    let r = c.request("PLAY", &base, &[("Session", &session)]).await;
    assert_eq!(r.status, 200, "{r:?}");
    let info = r.header("RTP-Info").unwrap().to_string();
    assert!(
        info.contains("rtptime="),
        "RTP-Info after the first frame: {info}"
    );
    let first_seq = rtp_info_seq(&info, 0);
    // SRs come every 2 s, for tracks that have sent media; audio starts one GOP in
    c.collect(Duration::from_millis(4500)).await;
    let video = c.packets.iter().find(|(ch, _)| *ch == 0).expect("video");
    assert_eq!(
        rtp_seq(&video.1),
        first_seq,
        "first packet matches RTP-Info"
    );
    let (nv, last_v) = check_rtp(&c.packets, 0);
    let (na, last_a) = check_rtp(&c.packets, 2);
    assert!(nv > 60, "{nv} video packets in 4.5 s");
    // A late joiner's audio starts with the next live IRAP (one GOP in), then at the
    // full 15.6 packets/s
    assert!(na > 8, "{na} audio packets in 4.5 s");
    // An SR on each RTCP channel
    for ch in [1, 3] {
        assert!(
            c.packets.iter().any(|(c, p)| *c == ch && p[1] == 200),
            "SR on channel {ch}"
        );
    }

    // PAUSE: nothing after the reply
    let r = c.request("PAUSE", &base, &[("Session", &session)]).await;
    assert_eq!(r.status, 200);
    let before = c.packets.len();
    c.collect(Duration::from_millis(800)).await;
    assert_eq!(c.packets.len(), before, "media after the PAUSE reply");
    // Sequence numbers continue after a re-PLAY
    let (_, last_v) = (nv, last_v.unwrap());
    let r = c.request("PLAY", &base, &[("Session", &session)]).await;
    assert_eq!(r.status, 200, "{r:?}");
    let info = r.header("RTP-Info").unwrap().to_string();
    assert_eq!(rtp_info_seq(&info, 0), last_v.wrapping_add(1), "{info}");
    assert_eq!(
        rtp_info_seq(&info, 1),
        last_a.unwrap().wrapping_add(1),
        "{info}"
    );
    let skip = c.packets.len();
    c.collect(Duration::from_secs(2)).await;
    let rest = c.packets[skip..].to_vec();
    let (nv2, _) = check_rtp(&rest, 0);
    assert!(nv2 > 20);
    assert_eq!(
        rtp_seq(&rest.iter().find(|(ch, _)| *ch == 0).unwrap().1),
        last_v.wrapping_add(1)
    );

    let r = c.request("TEARDOWN", &base, &[("Session", &session)]).await;
    assert_eq!(r.status, 200);
    drop(c);
    // The camera stops once the linger ends (default linger is short in tests? only check
    // that the hub's session left: a new client can still play)
    let mut c2 = Client::connect(h.addr).await;
    let r = c2.request("DESCRIBE", &base, &[]).await;
    assert_eq!(r.status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_rejections_and_limits() {
    let cfg = ServerConfig {
        max_unauthenticated_per_ip: 3,
        first_request_timeout: Duration::from_millis(500),
        ..Default::default()
    };
    let h = start(vec![], Schemes::Both, cfg).await;
    let base = format!("rtsp://{}/argus/main", h.addr);

    // Bad version: 505, then the connection closes (no reply loop)
    let mut c = Client::connect(h.addr).await;
    c.s.write_all(b"OPTIONS * RTSP/2.0\r\nCSeq: 1\r\n\r\n")
        .await
        .unwrap();
    let r = c.response().await.unwrap();
    assert_eq!(r.status, 505);
    assert!(c.response().await.is_none(), "closed after a parse error");
    // Body too large: 413 and close
    let mut c = Client::connect(h.addr).await;
    c.s.write_all(b"SET_PARAMETER * RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 999999\r\n\r\n")
        .await
        .unwrap();
    assert_eq!(c.response().await.unwrap().status, 413);
    assert!(c.response().await.is_none());

    // Unknown mount, methods
    let mut c = Client::connect(h.addr).await;
    assert_eq!(
        c.request("DESCRIBE", &format!("rtsp://{}/nope", h.addr), &[])
            .await
            .status,
        404
    );
    assert_eq!(c.request("RECORD", &base, &[]).await.status, 405);
    assert_eq!(c.request("FOO", &base, &[]).await.status, 501);
    assert_eq!(c.request("PLAY", &base, &[]).await.status, 455);
    drop(c);

    // Silent connections are closed after first_request_timeout
    let mut idle = Client::connect(h.addr).await;
    let t = std::time::Instant::now();
    assert!(idle.response().await.is_none());
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());

    // Unauthenticated connections per IP: the 4th gets 503
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut held = vec![];
    for _ in 0..3 {
        held.push(TcpStream::connect(h.addr).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut c = Client::connect(h.addr).await;
    assert_eq!(c.response().await.unwrap().status, 503);
    drop(held);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut c = Client::connect(h.addr).await;
    assert_eq!(c.request("OPTIONS", &base, &[]).await.status, 200);

    // Request rate: a burst over request_burst gets 503s but the connection survives
    let mut burst = String::new();
    for i in 0..60 {
        burst.push_str(&format!(
            "OPTIONS {base} RTSP/1.0\r\nCSeq: {}\r\n\r\n",
            100 + i
        ));
    }
    c.s.write_all(burst.as_bytes()).await.unwrap();
    let mut codes = vec![];
    for _ in 0..60 {
        codes.push(c.response().await.unwrap().status);
    }
    assert!(
        codes.iter().filter(|s| **s == 503).count() >= 15,
        "{codes:?}"
    );
    assert!(
        codes.iter().filter(|s| **s == 200).count() >= 38,
        "{codes:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_auth_over_the_wire() {
    let users = vec![User {
        name: "alice".into(),
        pass: Some("alicepw".into()),
    }];
    let h = start(users, Schemes::Both, ServerConfig::default()).await;
    let base = format!("rtsp://{}/argus/main", h.addr);
    let mut c = Client::connect(h.addr).await;
    // OPTIONS needs no credentials
    assert_eq!(c.request("OPTIONS", &base, &[]).await.status, 200);
    let r = c.request("DESCRIBE", &base, &[]).await;
    assert_eq!(r.status, 401);
    let challenges: Vec<&str> = r
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("WWW-Authenticate"))
        .map(|(_, v)| v.as_str())
        .collect();
    assert!(
        challenges.iter().any(|v| v.starts_with("Digest ")),
        "{challenges:?}"
    );
    assert!(
        challenges.iter().any(|v| v.starts_with("Basic ")),
        "{challenges:?}"
    );
    // Basic
    let r = c
        .request(
            "DESCRIBE",
            &base,
            &[("Authorization", "Basic YWxpY2U6YWxpY2Vwdw==")],
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = c
        .request(
            "DESCRIBE",
            &base,
            &[("Authorization", "Basic YWxpY2U6d3Jvbmc=")],
        )
        .await;
    assert_eq!(r.status, 401);
    // Digest, computed as a client would
    let digest = challenges
        .iter()
        .find(|v| v.starts_with("Digest "))
        .unwrap();
    let nonce = digest
        .split("nonce=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let md5 = |s: String| format!("{:x}", md5::compute(s.as_bytes()));
    let ha1 = md5("alice:neolink:alicepw".to_string());
    let ha2 = md5(format!("DESCRIBE:{base}"));
    let resp = md5(format!("{ha1}:{nonce}:{ha2}"));
    let auth = format!(
        "Digest username=\"alice\", realm=\"neolink\", nonce=\"{nonce}\", uri=\"{base}\", response=\"{resp}\""
    );
    let r = c
        .request("DESCRIBE", &base, &[("Authorization", &auth)])
        .await;
    assert_eq!(r.status, 200, "{r:?}");
}

// ---------------------------------------------------------------------------------------
// ffmpeg

fn ffmpeg() -> Option<String> {
    std::env::var("NEOLINK_FFMPEG").ok()
}

fn ffprobe(ffmpeg: &str) -> String {
    let p = std::path::Path::new(ffmpeg);
    p.with_file_name("ffprobe").to_string_lossy().into_owned()
}

/// Record `secs` with ffmpeg (stream copy) and return (video frames, audio frames, stderr)
async fn ffmpeg_record(url: &str, extra: &[&str], secs: u32) -> Option<(usize, usize, String)> {
    let ff = ffmpeg()?;
    let dir = std::env::temp_dir().join(format!("neolink-rtsp-{}", rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("out.mkv");
    let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "info".into()];
    args.extend(extra.iter().map(|s| s.to_string()));
    args.extend(
        [
            "-i",
            url,
            "-t",
            &secs.to_string(),
            "-c",
            "copy",
            "-y",
            out.to_str().unwrap(),
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    let o = tokio::time::timeout(
        Duration::from_secs(secs as u64 + 30),
        tokio::process::Command::new(&ff).args(&args).output(),
    )
    .await
    .expect("ffmpeg finished")
    .unwrap();
    let stderr = String::from_utf8_lossy(&o.stderr).to_string();
    if !o.status.success() {
        return Some((0, 0, stderr));
    }
    let count = |sel: &str| -> usize {
        let o = std::process::Command::new(ffprobe(&ff))
            .args([
                "-v",
                "error",
                "-select_streams",
                sel,
                "-count_packets",
                "-show_entries",
                "stream=nb_read_packets",
                "-of",
                "csv=p=0",
            ])
            .arg(&out)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse()
            .unwrap_or(0)
    };
    Some((count("v:0"), count("a:0"), stderr))
}

fn rand_suffix() -> String {
    format!(
        "{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ffmpeg_tcp_and_udp_fallback() {
    if ffmpeg().is_none() {
        eprintln!("NEOLINK_FFMPEG unset: skipping");
        return;
    }
    let h = start(vec![], Schemes::Both, ServerConfig::default()).await;
    for (path, extra) in [
        ("argus/main", vec!["-rtsp_transport", "tcp"]),
        // ffmpeg's default tries UDP first; the 461 makes it retry over TCP
        ("argus/sub", vec![]),
    ] {
        let url = format!("rtsp://{}/{path}", h.addr);
        let (v, a, err) = ffmpeg_record(&url, &extra, 4).await.unwrap();
        // 4 s at the fixtures' ~15 fps and 16 kHz AAC (15.6 frames/s)
        assert!(v >= 45, "{path}: {v} video frames\n{err}");
        assert!(a >= 45, "{path}: {a} audio frames\n{err}");
        assert!(
            !err.contains("non monotonically increasing dts"),
            "{path}: {err}"
        );
    }
    let _ = &h.sources;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ffmpeg_digest() {
    if ffmpeg().is_none() {
        eprintln!("NEOLINK_FFMPEG unset: skipping");
        return;
    }
    let users = vec![User {
        name: "alice".into(),
        pass: Some("alicepw".into()),
    }];
    let h = start(users, Schemes::Digest, ServerConfig::default()).await;
    let ok = format!("rtsp://alice:alicepw@{}/argus/main", h.addr);
    let (v, _, err) = ffmpeg_record(&ok, &["-rtsp_transport", "tcp"], 2)
        .await
        .unwrap();
    assert!(v >= 20, "{v} frames\n{err}");
    let bad = format!("rtsp://alice:nope@{}/argus/main", h.addr);
    let (v, _, err) = ffmpeg_record(&bad, &["-rtsp_transport", "tcp"], 2)
        .await
        .unwrap();
    assert_eq!(v, 0);
    assert!(err.contains("401"), "{err}");
}
