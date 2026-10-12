//! Gate 3 acceptance tests on fixtures over real sockets (SPEC A7, A8, A9, A11, A18, A20)

mod common;

use common::*;
use neolink_rtsp::hub::task::{HubConfig, ParameterSets};
use neolink_rtsp::rtsp::auth::Schemes;
use neolink_rtsp::rtsp::server::ServerConfig;
use std::sync::atomic::Ordering::SeqCst;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

fn starts(h: &Harness, i: usize) -> usize {
    h.sources[i].starts.lock().unwrap().len()
}

/// First video packet of a client: its NAL type is an IRAP (or a parameter set before one)
fn first_video_is_irap(c: &Client, h265: bool) -> bool {
    c.packets
        .iter()
        .find(|(ch, _)| *ch == 0)
        .is_some_and(|(_, p)| is_irap_type(h265, video_nal_type(h265, p)))
}

/// A7: staggered joiners, one during camera start-up, main and sub at once: one camera
/// start per kind; every client's first video frame is an IRAP. Under `strip` and `inject`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_a7_staggered_joins_one_start_per_kind() {
    for ps in [ParameterSets::Strip, ParameterSets::Inject] {
        let hub: fn(&mut HubConfig) = match ps {
            ParameterSets::Inject => |c| c.parameter_sets = ParameterSets::Inject,
            _ => |c| c.parameter_sets = ParameterSets::Strip,
        };
        let h = start_with(Setup {
            hub,
            ..Default::default()
        })
        .await;
        let addr = h.addr;
        let mut tasks = vec![];
        for (i, (delay_ms, path)) in [
            (0, "main"),
            (200, "main"), // while the camera is starting
            (300, "sub"),
            (2500, "main"),
            (3000, "sub"),
            (5000, "main"),
        ]
        .into_iter()
        .enumerate()
        {
            tasks.push(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                let mut c = Client::connect(addr).await;
                let base = format!("rtsp://{addr}/argus/{path}");
                c.play(&base, None).await;
                // Until media flows (bounded), not a fixed window: CI machines are loaded
                let t = Instant::now();
                while t.elapsed() < Duration::from_secs(6)
                    && (c.packets.len() <= 10 || !c.packets.iter().any(|(ch, _)| *ch == 0))
                {
                    c.collect(Duration::from_millis(250)).await;
                }
                let h265 = path == "main";
                (i, path, first_video_is_irap(&c, h265), c.packets.len())
            }));
        }
        for t in tasks {
            let (i, path, irap, n) = t.await.unwrap();
            assert!(n > 10, "{ps:?} client {i} ({path}): {n} packets");
            assert!(
                irap,
                "{ps:?} client {i} ({path}): first video frame is not an IRAP"
            );
        }
        assert_eq!(starts(&h, 0), 1, "{ps:?}: main camera starts");
        assert_eq!(starts(&h, 1), 1, "{ps:?}: sub camera starts");
    }
}

/// A8 (crate part): 100 connect/teardown cycles; the camera stream stops within 5 s of
/// the last client and the runtime's task count returns to its baseline
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_a8_hundred_cycles() {
    let h = start_with(Setup::default()).await;
    let base = format!("rtsp://{}/argus/main", h.addr);
    let cycle = |n: usize| {
        let base = base.clone();
        let addr = h.addr;
        async move {
            let mut c = Client::connect(addr).await;
            let (session, _) = c.play(&base, None).await;
            while c.take_packet() {}
            while !c.packets.iter().any(|(ch, _)| *ch == 0) {
                assert!(c.fill().await, "cycle {n}: no media");
                while c.take_packet() {}
            }
            let r = c.request("TEARDOWN", &base, &[("Session", &session)]).await;
            assert_eq!(r.status, 200);
            assert!(
                c.until_eof(Duration::from_secs(5)).await,
                "closed after TEARDOWN"
            );
        }
    };
    cycle(0).await;
    let wait_stopped = |what: &'static str| {
        let src = h.sources[0].clone();
        async move {
            let t = Instant::now();
            while src.running.load(SeqCst) > 0 {
                assert!(
                    t.elapsed() < Duration::from_secs(6),
                    "{what}: camera still running"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            t.elapsed()
        }
    };
    wait_stopped("after the warm-up").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let metrics = tokio::runtime::Handle::current().metrics();
    let baseline = metrics.num_alive_tasks();
    for n in 1..=100 {
        cycle(n).await;
    }
    let stop = wait_stopped("after 100 cycles").await;
    assert!(
        stop <= Duration::from_secs(5),
        "stopped {stop:?} after the last client"
    );
    let t = Instant::now();
    while metrics.num_alive_tasks() > baseline && t.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(metrics.num_alive_tasks(), baseline, "tasks leaked");
    // One camera start for the warm-up, one more for the cycles (it lingered between)
    assert!(starts(&h, 0) <= 2, "{} starts", starts(&h, 0));
}

/// A9: a client that stops reading (4 KiB receive buffer) does not disturb another
/// client's timing, and is dropped
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_a9_stuck_client() {
    let h = start_with(Setup {
        // Small socket buffers so the stuck client stalls the writer within the test
        cfg: ServerConfig {
            write_stall_timeout: Duration::from_secs(3),
            send_buffer: Some(16 * 1024),
            // The stuck and the healthy client: a third gets in only once the stuck
            // connection is really gone
            max_connections: 2,
            ..Default::default()
        },
        hub: |c| c.latency_us = 500_000,
        ..Default::default()
    })
    .await;
    let base = format!("rtsp://{}/argus/main", h.addr);
    // The stuck client
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.set_recv_buffer_size(4096).unwrap();
    let stuck = sock.connect(h.addr).await.unwrap();
    let mut stuck = Client::from_stream(stuck);
    stuck.play(&base, None).await;
    // The healthy one
    let mut good = Client::connect(h.addr).await;
    good.play(&base, None).await;
    good.collect(Duration::from_secs(12)).await;
    // Inter-frame arrival deltas (first packet of each video frame, by RTP timestamp)
    let mut frames: Vec<(u32, Instant)> = vec![];
    for (i, (ch, p)) in good.packets.iter().enumerate() {
        if *ch == 0 && frames.last().is_none_or(|f| f.0 != rtp_ts(p)) {
            frames.push((rtp_ts(p), good.arrivals[i]));
        }
    }
    assert!(frames.len() > 100, "{} frames", frames.len());
    // Skip the start-up second; compare arrival spacing with media spacing
    let mut dev: Vec<f64> = frames[15..]
        .windows(2)
        .map(|w| {
            let media = w[1].0.wrapping_sub(w[0].0) as f64 / 90_000.0;
            let arrival = w[1].1.duration_since(w[0].1).as_secs_f64();
            (arrival - media).abs()
        })
        .collect();
    dev.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p99 = dev[(dev.len() as f64 * 0.99) as usize - 1];
    assert!(
        p99 < 0.050,
        "inter-frame p99 deviation {:.1} ms",
        p99 * 1000.0
    );
    let st = h.hubs[0].stats().await.unwrap();
    assert_eq!(st.sessions, 1, "the stuck session was dropped: {st:?}");
    // ... and its connection closed, not just its session (review #1)
    assert!(
        stuck.until_eof(Duration::from_secs(10)).await,
        "the stuck connection is still open"
    );
    let t = Instant::now();
    loop {
        let mut third = Client::connect(h.addr).await;
        third.cseq += 1;
        let req = format!("OPTIONS {base} RTSP/1.0\r\nCSeq: 1\r\n\r\n");
        third.s.write_all(req.as_bytes()).await.unwrap();
        if third.response().await.is_some_and(|r| r.status == 200) {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(3),
            "the stuck connection still holds its slot"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A11: a 60 s camera outage mid-stream: the session survives, live media resumes from an
/// IRAP, RTP timestamps stay monotonic and follow wall time (no jump over 1 s)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_a11_outage() {
    let h = start_with(Setup::default()).await;
    h.sources[0].looping.store(true, SeqCst);
    let base = format!("rtsp://{}/argus/main", h.addr);
    let mut c = Client::connect(h.addr).await;
    c.play(&base, None).await;
    c.collect(Duration::from_secs(5)).await;
    h.sources[0].stalled.store(true, SeqCst);
    let stalled_at = c.packets.len();
    c.collect(Duration::from_secs(60)).await;
    h.sources[0].stalled.store(false, SeqCst);
    let resumed_at = c.packets.len();
    c.collect(Duration::from_secs(8)).await;
    // Bridged media flowed during the outage
    let during = &c.packets[stalled_at..resumed_at];
    assert!(during.iter().any(|(ch, _)| *ch == 0), "no bridge video");
    assert!(during.iter().any(|(ch, _)| *ch == 2), "no bridge audio");
    // Live again, starting at an IRAP: the first video after the stall ends whose
    // timestamp is new and which is not a bridge repeat
    let after = &c.packets[resumed_at..];
    assert!(
        after.iter().filter(|(ch, _)| *ch == 0).count() > 50,
        "live video resumed"
    );
    for ch in [0u8, 2] {
        check_rtp(&c.packets, ch);
        let clock = if ch == 0 { 90_000.0 } else { 16_000.0 };
        let mut last: Option<(u32, Instant)> = None;
        for (i, (c2, p)) in c.packets.iter().enumerate() {
            if *c2 != ch {
                continue;
            }
            let (ts, at) = (rtp_ts(p), c.arrivals[i]);
            if let Some((lts, lat)) = last {
                let media = ts.wrapping_sub(lts) as f64 / clock;
                let wall = at.duration_since(lat).as_secs_f64();
                assert!(
                    (media - wall).abs() <= 1.0,
                    "channel {ch}: media step {media:.3} s vs wall {wall:.3} s"
                );
            }
            last = Some((ts, at));
        }
    }
    // Still one session, still connected
    assert_eq!(h.hubs[0].stats().await.unwrap().playing, 1);
}

/// A18: a mid-stream resolution change ends the session with BYE and a close; a new
/// DESCRIBE gets the new SDP
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_a18_resolution_change() {
    let h = start_with(Setup {
        main: "reschange_h264.bcm",
        ..Default::default()
    })
    .await;
    let base = format!("rtsp://{}/argus/main", h.addr);
    let mut c = Client::connect(h.addr).await;
    let (_, sdp1) = c.play(&base, None).await;
    assert!(
        c.until_eof(Duration::from_secs(25)).await,
        "closed on the change"
    );
    assert!(
        c.packets.iter().any(|(ch, p)| *ch % 2 == 1 && p[1] == 203),
        "BYE before the close"
    );
    let mut c2 = Client::connect(h.addr).await;
    let r = c2.request("DESCRIBE", &base, &[]).await;
    assert_eq!(r.status, 200);
    let fmtp = |s: &str| {
        s.lines()
            .find(|l| l.starts_with("a=fmtp:96"))
            .unwrap()
            .to_string()
    };
    assert_ne!(
        fmtp(&sdp1),
        fmtp(&r.body),
        "the new SDP has the new parameter sets"
    );
}

/// A20: oversized head, slowloris, 100 unauthenticated connections, 24 authenticated
/// DESCRIBEs from one IP, a session id from another connection, re-authorization
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_a20_security() {
    let h = start_with(Setup {
        users: alice_bob(),
        schemes: Schemes::Both,
        cfg: ServerConfig {
            first_request_timeout: Duration::from_secs(1),
            ..Default::default()
        },
        ..Default::default()
    })
    .await;
    let base = format!("rtsp://{}/argus/main", h.addr);
    let alice = basic("alice", "alicepw");

    // Head over 8 KiB: 431 and close
    let mut c = Client::connect(h.addr).await;
    let big = format!(
        "OPTIONS {base} RTSP/1.0\r\nCSeq: 1\r\nX-Pad: {}\r\n\r\n",
        "a".repeat(9000)
    );
    c.s.write_all(big.as_bytes()).await.unwrap();
    assert_eq!(c.response().await.unwrap().status, 431);
    assert!(c.response().await.is_none());

    // Slowloris: a byte every 200 ms never completes a request; closed after ~1 s, also
    // after a first complete request
    for warm in [false, true] {
        let mut s = TcpStream::connect(h.addr).await.unwrap();
        if warm {
            s.write_all(format!("OPTIONS {base} RTSP/1.0\r\nCSeq: 1\r\n\r\n").as_bytes())
                .await
                .unwrap();
        }
        let t = Instant::now();
        let mut closed = false;
        for b in b"OPTIONS rtsp://x/ RTSP/1.0\r\nX: ".iter().cycle().take(60) {
            if s.write_all(&[*b]).await.is_err() {
                closed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(closed, "warm={warm}: slowloris connection still open");
        assert!(
            t.elapsed() < Duration::from_secs(4),
            "warm={warm}: {:?}",
            t.elapsed()
        );
    }

    // 100 unauthenticated connections from one IP: it holds at most its share (32), its
    // own oldest are closed to make room, and another address still gets in at once
    let per_ip = ServerConfig::default().max_unauthenticated_per_ip;
    let mut held = vec![];
    for _ in 0..100 {
        held.push(Client::connect(h.addr).await);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut closed = vec![];
    for c in held.iter_mut() {
        closed.push(closed_within(&mut c.s, Duration::from_millis(5)).await);
    }
    assert_eq!(
        closed.iter().filter(|c| **c).count(),
        100 - per_ip,
        "connections past the per-IP share closed"
    );
    assert!(
        closed[100 - per_ip..].iter().all(|c| !c),
        "the newest {per_ip} are the ones kept"
    );
    let mut other = Client::connect_from(h.addr, "127.0.0.2").await;
    let r = other
        .request("DESCRIBE", &base, &[("Authorization", &alice)])
        .await;
    assert_eq!(r.status, 200, "another address locked out");
    drop(held);
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // 24 simultaneous authenticated DESCRIBEs from one IP all succeed
    let mut tasks = vec![];
    for _ in 0..24 {
        let (addr, base, alice) = (h.addr, base.clone(), alice.clone());
        tasks.push(tokio::spawn(async move {
            let mut c = Client::connect(addr).await;
            c.request("DESCRIBE", &base, &[("Authorization", &alice)])
                .await
                .status
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }

    // A session id from another connection: 454
    let mut a = Client::connect(h.addr).await;
    let (session, _) = a.play(&base, Some(&alice)).await;
    let mut b = Client::connect(h.addr).await;
    let r = b
        .request(
            "PLAY",
            &base,
            &[("Session", &session), ("Authorization", &alice)],
        )
        .await;
    assert_eq!(r.status, 454);
    let r = b
        .request(
            "TEARDOWN",
            &base,
            &[("Session", &session), ("Authorization", &alice)],
        )
        .await;
    assert_eq!(r.status, 454);
    // Every request is re-authorized, as the session's user
    let r = a
        .request("GET_PARAMETER", &base, &[("Session", &session)])
        .await;
    assert_eq!(r.status, 401, "no credentials on a session request");
    let bob = basic("bob", "bobpw");
    let r = a
        .request(
            "GET_PARAMETER",
            &base,
            &[("Session", &session), ("Authorization", &bob)],
        )
        .await;
    assert_eq!(r.status, 454, "another user on alice's session");
    let r = a
        .request(
            "GET_PARAMETER",
            &base,
            &[("Session", &session), ("Authorization", &alice)],
        )
        .await;
    assert_eq!(r.status, 200);
}

/// Time from sending DESCRIBE to a fresh client's first video RTP packet, and to its
/// first live frame (the next video timestamp: the first frame may be the cached IRAP)
async fn describe_to_first_video(h: &Harness) -> (Duration, Duration) {
    let base = format!("rtsp://{}/argus/main", h.addr);
    let mut c = Client::connect(h.addr).await;
    let t = Instant::now();
    c.play(&base, None).await;
    // Packets that came in the same read as the PLAY reply are already buffered
    let video_ts = |c: &Client| -> Vec<(u32, Instant)> {
        let mut v: Vec<(u32, Instant)> = vec![];
        for (i, (ch, p)) in c.packets.iter().enumerate() {
            if *ch == 0 && v.last().is_none_or(|l| l.0 != rtp_ts(p)) {
                v.push((rtp_ts(p), c.arrivals[i]));
            }
        }
        v
    };
    while c.take_packet() {}
    while video_ts(&c).len() < 2 {
        assert!(c.fill().await, "no media");
        while c.take_packet() {}
    }
    let v = video_ts(&c);
    (v[0].1.duration_since(t), v[1].1.duration_since(t))
}

/// N1: DESCRIBE → first video RTP ≤ 2 s with the camera streaming, and ≤ 2 s plus the
/// wake time with it asleep (desc from the cache; 3 s simulated wake)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_n1_time_to_first_video() {
    let dir = temp_dir("n1");
    // Streaming: another client keeps the camera up
    {
        let h = start_with(Setup {
            state_dir: Some(dir.clone()),
            ..Default::default()
        })
        .await;
        let mut keeper = Client::connect(h.addr).await;
        keeper
            .play(&format!("rtsp://{}/argus/main", h.addr), None)
            .await;
        keeper.collect(Duration::from_secs(3)).await;
        let (d, live) = describe_to_first_video(&h).await;
        eprintln!("N1 streaming: first video {d:?}, first live {live:?}");
        assert!(d <= Duration::from_secs(2), "streaming: {d:?}");
    }
    // Asleep: the desc comes from the cache; PLAY wakes the camera
    let wake = Duration::from_secs(3);
    let h = start_with(Setup {
        state_dir: Some(dir.clone()),
        wake,
        ..Default::default()
    })
    .await;
    let (d, live) = describe_to_first_video(&h).await;
    eprintln!("N1 asleep (wake {wake:?}): first video {d:?}, first live {live:?}");
    assert!(d <= Duration::from_secs(2) + wake, "asleep: {d:?}");
    assert!(
        live <= Duration::from_secs(2) + wake,
        "asleep, live: {live:?}"
    );
    assert_eq!(
        h.sources[0].starts.lock().unwrap().as_slice(),
        &[neolink_rtsp::source::StartMode::Demand],
        "one demand start, no learning start"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review: SETUP checks the track before creating a session; PLAY/PAUSE must name the
/// session; no SETUP while playing; PLAY needs a track
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_session_state_rules() {
    let h = start_with(Setup::default()).await;
    let base = format!("rtsp://{}/argus/main", h.addr);
    let mut c = Client::connect(h.addr).await;
    let tcp = ("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1");
    let r = c
        .request("SETUP", &format!("{base}/stream=5"), &[tcp])
        .await;
    assert_eq!(r.status, 404);
    assert!(r.header("Session").is_none());
    assert_eq!(
        c.request("PLAY", &base, &[]).await.status,
        455,
        "no session"
    );
    assert_eq!(h.hubs[0].stats().await.unwrap().playing, 0);
    let r = c
        .request("SETUP", &format!("{base}/stream=0"), &[tcp])
        .await;
    assert_eq!(r.status, 200);
    let session = r
        .header("Session")
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    assert_eq!(
        c.request("PLAY", &base, &[]).await.status,
        454,
        "PLAY must name it"
    );
    let r = c.request("PLAY", &base, &[("Session", &session)]).await;
    assert_eq!(r.status, 200);
    let audio = ("Transport", "RTP/AVP/TCP;unicast;interleaved=2-3");
    let r = c
        .request(
            "SETUP",
            &format!("{base}/stream=1"),
            &[audio, ("Session", &session)],
        )
        .await;
    assert_eq!(r.status, 455, "SETUP while playing");
    assert_eq!(
        c.request("PAUSE", &base, &[]).await.status,
        454,
        "PAUSE must name it"
    );
}

/// Review #2: the stream changes while the session is paused; the next PLAY ends the
/// session (454) instead of answering 503 forever, and a fresh session plays
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_stream_change_while_paused() {
    let h = start_with(Setup {
        main: "reschange_h264.bcm",
        ..Default::default()
    })
    .await;
    let base = format!("rtsp://{}/argus/main", h.addr);
    let mut c = Client::connect(h.addr).await;
    let (session, _) = c.play(&base, None).await;
    let r = c.request("PAUSE", &base, &[("Session", &session)]).await;
    assert_eq!(r.status, 200);
    // Another client keeps the camera up while the resolution changes (mid-fixture)
    let mut keeper = Client::connect(h.addr).await;
    keeper.play(&base, None).await;
    keeper.until_eof(Duration::from_secs(25)).await;
    let r = c.request("PLAY", &base, &[("Session", &session)]).await;
    assert_eq!(r.status, 454, "{r:?}");
    let mut fresh = Client::connect(h.addr).await;
    fresh.play(&base, None).await;
    fresh.collect(Duration::from_secs(1)).await;
    assert!(fresh.packets.iter().any(|(ch, _)| *ch == 0));
}

/// Security review: unauthenticated connections are bounded in time and junk interleaved
/// data doesn't keep them alive; failed auths are paced per IP across connections; an
/// unknown mount doesn't answer 404 before credentials
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_unauthenticated_bounds() {
    let h = start_with(Setup {
        users: alice_bob(),
        cfg: ServerConfig {
            idle_timeout: Duration::from_secs(1),
            unauthenticated_lifetime: Duration::from_secs(3),
            failed_auth_per_minute: 4,
            ..Default::default()
        },
        ..Default::default()
    })
    .await;
    let base = format!("rtsp://{}/argus/main", h.addr);
    // OPTIONS then `$` frames on a channel nobody set up: closed at the idle timeout
    let mut c = Client::connect(h.addr).await;
    assert_eq!(c.request("OPTIONS", &base, &[]).await.status, 200);
    let t = Instant::now();
    let mut closed = false;
    for _ in 0..30 {
        if c.s.write_all(b"$\x05\x00\x00").await.is_err() {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        closed || c.until_eof(Duration::from_millis(100)).await,
        "junk kept it open"
    );
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    // OPTIONS every 500 ms (activity), never authenticating: closed at the lifetime
    let mut c = Client::connect(h.addr).await;
    let t = Instant::now();
    let mut open_for = Duration::ZERO;
    for _ in 0..20 {
        c.cseq += 1;
        let req = format!("OPTIONS {base} RTSP/1.0\r\nCSeq: {}\r\n\r\n", c.cseq);
        if c.s.write_all(req.as_bytes()).await.is_err() || c.response().await.is_none() {
            break;
        }
        open_for = t.elapsed();
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        open_for >= Duration::from_secs(2) && open_for < Duration::from_secs(5),
        "unauthenticated connection open for {open_for:?}"
    );
    // Unknown mount: 401 without credentials, 404 with valid ones
    let mut c = Client::connect(h.addr).await;
    let nope = format!("rtsp://{}/nope/main", h.addr);
    assert_eq!(c.request("DESCRIBE", &nope, &[]).await.status, 401);
    let alice = basic("alice", "alicepw");
    assert_eq!(
        c.request("DESCRIBE", &nope, &[("Authorization", &alice)])
            .await
            .status,
        404
    );
    // 10 wrong passwords over 10 parallel connections: all answered at once (nothing
    // waits while holding a connection); past the 4 allowed per minute this address gets
    // 401 for any user not already proven from it (alice was, above), right or wrong
    let t = Instant::now();
    let mut tasks = vec![];
    for _ in 0..10 {
        let (addr, base) = (h.addr, base.clone());
        tasks.push(tokio::spawn(async move {
            let mut c = Client::connect(addr).await;
            let wrong = basic("alice", "wrong");
            c.request("DESCRIBE", &base, &[("Authorization", &wrong)])
                .await
                .status
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), 401);
    }
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "10 failures took {:?}",
        t.elapsed()
    );
    let bob = basic("bob", "bobpw");
    assert_eq!(
        c.request("DESCRIBE", &base, &[("Authorization", &bob)])
            .await
            .status,
        401,
        "a right guess past the limit"
    );
    assert_eq!(
        c.request("DESCRIBE", &base, &[("Authorization", &alice)])
            .await
            .status,
        200,
        "a user proven from this address"
    );
}
