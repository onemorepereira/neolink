//! Connection limits under attack (SPEC R24, DESIGN §6.6): one address (or a few) must
//! not be able to lock everyone else out, and legitimate traffic that looks like a flood
//! (Frigate/go2rtc: many connections from one address) must still work.
//!
//! Other "hosts" are other source addresses in 127.0.0.0/8, which Linux lets a socket
//! bind to.

mod common;

use common::*;
use neolink_rtsp::rtsp::auth::Schemes;
use neolink_rtsp::rtsp::server::ServerConfig;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// A request that reports failure instead of panicking: the status, or None if the
/// connection closed or answered something else
async fn try_request(
    c: &mut Client,
    method: &str,
    uri: &str,
    extra: &[(&str, &str)],
) -> Option<u16> {
    c.cseq += 1;
    let mut r = format!("{method} {uri} RTSP/1.0\r\nCSeq: {}\r\n", c.cseq);
    for (k, v) in extra {
        r.push_str(&format!("{k}: {v}\r\n"));
    }
    r.push_str("\r\n");
    c.s.write_all(r.as_bytes()).await.ok()?;
    let resp = tokio::time::timeout(Duration::from_secs(10), c.response())
        .await
        .ok()??;
    if resp.header("CSeq") != Some(c.cseq.to_string().as_str()) {
        return None;
    }
    Some(resp.status)
}

/// What a Frigate ffmpeg does: OPTIONS, DESCRIBE without credentials (401 when users are
/// configured), DESCRIBE with them, SETUP both tracks, PLAY; then it must get video.
/// Ok((client, session id)) when playing with media, Err(the step that failed)
async fn player(
    addr: SocketAddr,
    src: &str,
    auth: Option<&str>,
) -> Result<(Client, String), String> {
    let base = format!("rtsp://{addr}/argus/main");
    let mut c = Client::connect_from(addr, src).await;
    let s = try_request(&mut c, "OPTIONS", &base, &[]).await;
    if s != Some(200) {
        return Err(format!("OPTIONS {s:?}"));
    }
    let mut extra: Vec<(&str, &str)> = vec![];
    if let Some(a) = auth {
        let s = try_request(&mut c, "DESCRIBE", &base, &[]).await;
        if s != Some(401) {
            return Err(format!("DESCRIBE without credentials {s:?}"));
        }
        extra.push(("Authorization", a));
    }
    let s = try_request(&mut c, "DESCRIBE", &base, &extra).await;
    if s != Some(200) {
        return Err(format!("DESCRIBE {s:?}"));
    }
    let mut session = String::new();
    for (track, ch) in [(0, "0-1"), (1, "2-3")] {
        let t = format!("RTP/AVP/TCP;unicast;interleaved={ch}");
        let mut e = extra.clone();
        e.push(("Transport", &t));
        if !session.is_empty() {
            e.push(("Session", &session));
        }
        c.cseq += 1;
        let mut r = format!(
            "SETUP {base}/stream={track} RTSP/1.0\r\nCSeq: {}\r\n",
            c.cseq
        );
        for (k, v) in &e {
            r.push_str(&format!("{k}: {v}\r\n"));
        }
        r.push_str("\r\n");
        c.s.write_all(r.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let resp = c.response().await.ok_or("SETUP: closed")?;
        if resp.status != 200 {
            return Err(format!("SETUP {}", resp.status));
        }
        session = resp
            .header("Session")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
    }
    let mut e = extra.clone();
    e.push(("Session", &session));
    let s = try_request(&mut c, "PLAY", &base, &e).await;
    if s != Some(200) {
        return Err(format!("PLAY {s:?}"));
    }
    if !has_new_video(&mut c, Duration::from_secs(3)).await {
        return Err("no video".into());
    }
    Ok((c, session))
}

/// Fresh video arrives within `d`
async fn has_new_video(c: &mut Client, d: Duration) -> bool {
    let end = tokio::time::Instant::now() + d;
    c.packets.clear();
    while tokio::time::Instant::now() < end {
        c.collect(Duration::from_millis(200)).await;
        if c.packets.iter().any(|(ch, _)| *ch == 0) {
            return true;
        }
    }
    false
}

/// Silent connections from `src` (they never send a byte)
async fn hold(addr: SocketAddr, src: &str, n: usize) -> Vec<TcpStream> {
    let mut v = vec![];
    for _ in 0..n {
        v.push(connect_from(addr, src).await);
    }
    v
}

/// How many of these the server has closed (within a short look each)
async fn count_closed(v: &mut [TcpStream]) -> usize {
    let mut n = 0;
    for s in v.iter_mut() {
        if closed_within(s, Duration::from_millis(30)).await {
            n += 1;
        }
    }
    n
}

/// Finding 1: one address opening 100 connections must not lock out another address,
/// and a Frigate-like client (20 concurrent players, one address) must still play
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_one_address_cannot_lock_out_another() {
    let h = start_with(Setup {
        users: alice_bob(),
        ..Default::default()
    })
    .await;
    let _flood = hold(h.addr, "127.0.0.2", 100).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let alice = basic("alice", "alicepw");
    let mut tasks = vec![];
    for _ in 0..20 {
        let (addr, alice) = (h.addr, alice.clone());
        tasks.push(tokio::spawn(async move {
            player(addr, "127.0.0.1", Some(&alice)).await.map(|_| ())
        }));
    }
    let mut failures = vec![];
    for t in tasks {
        if let Err(e) = t.await.unwrap() {
            failures.push(e);
        }
    }
    assert!(failures.is_empty(), "players failed: {failures:?}");
}

/// Finding 1 with churn (finding 2, users configured): an attacker that reconnects as
/// soon as it is closed, sending OPTIONS to look alive, must not lock others out
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_reconnect_churn_does_not_lock_out() {
    let h = start_with(Setup {
        users: alice_bob(),
        cfg: ServerConfig {
            first_request_timeout: Duration::from_secs(1),
            unauthenticated_lifetime: Duration::from_secs(1),
            ..Default::default()
        },
        ..Default::default()
    })
    .await;
    let stop = tokio_util::sync::CancellationToken::new();
    let mut attackers = vec![];
    for _ in 0..100 {
        let (addr, stop) = (h.addr, stop.clone());
        attackers.push(tokio::spawn(async move {
            let req = format!("OPTIONS rtsp://{addr}/ RTSP/1.0\r\nCSeq: 1\r\n\r\n");
            while !stop.is_cancelled() {
                let mut s = connect_from(addr, "127.0.0.2").await;
                loop {
                    if s.write_all(req.as_bytes()).await.is_err() {
                        break;
                    }
                    if closed_within(&mut s, Duration::from_millis(300)).await {
                        break;
                    }
                    if stop.is_cancelled() {
                        return;
                    }
                }
            }
        }));
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let alice = basic("alice", "alicepw");
    for i in 0..3 {
        let r = player(h.addr, "127.0.0.1", Some(&alice)).await;
        assert!(r.is_ok(), "attempt {i}: {:?}", r.err());
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
    stop.cancel();
    for a in attackers {
        let _ = a.await;
    }
}

/// Finding 2 (anonymous): without users, a connection that never sets up a session is
/// bounded by unauthenticated_lifetime too, whatever it sends to look alive
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_anonymous_sessionless_connection_is_bounded() {
    let h = start_with(Setup {
        cfg: ServerConfig {
            unauthenticated_lifetime: Duration::from_secs(2),
            ..Default::default()
        },
        ..Default::default()
    })
    .await;
    let base = format!("rtsp://{}/argus/main", h.addr);
    // OPTIONS, GET_PARAMETER without a session, and `$` junk: none keeps it past ~2 s
    for keepalive in ["OPTIONS", "GET_PARAMETER", "$"] {
        let mut c = Client::connect(h.addr).await;
        let t = Instant::now();
        let mut open_for = Duration::ZERO;
        while t.elapsed() < Duration::from_secs(7) {
            let ok = if keepalive == "$" {
                c.s.write_all(b"$\x05\x00\x00").await.is_ok()
                    && !closed_within(&mut c.s, Duration::from_millis(1)).await
            } else {
                try_request(&mut c, keepalive, &base, &[]).await == Some(200)
            };
            if !ok {
                break;
            }
            open_for = t.elapsed();
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        assert!(
            open_for < Duration::from_secs(4),
            "{keepalive}: session-less anonymous connection open for {open_for:?}"
        );
    }
}

/// Finding 2 (anonymous): playing anonymous clients don't hold unauthenticated slots, so
/// they don't cap the server at max_unauthenticated_total, and aren't evicted
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_anonymous_players_hold_no_unauthenticated_slot() {
    let h = start_with(Setup {
        cfg: ServerConfig {
            max_unauthenticated_per_ip: 4,
            max_unauthenticated_total: 4,
            unauthenticated_lifetime: Duration::from_secs(1),
            ..Default::default()
        },
        ..Default::default()
    })
    .await;
    let mut players = vec![];
    for i in 0..10 {
        match player(h.addr, "127.0.0.1", None).await {
            Ok((c, _)) => players.push(c),
            Err(e) => panic!("player {i}: {e}"),
        }
    }
    assert_eq!(h.server.unauthenticated_count(), 0);
    // Past the lifetime: all still get media
    tokio::time::sleep(Duration::from_millis(1500)).await;
    for (i, c) in players.iter_mut().enumerate() {
        assert!(
            has_new_video(c, Duration::from_secs(3)).await,
            "player {i} lost"
        );
    }
}

/// Finding 6: with users configured, an anonymous-mount client that PAUSEs (after the
/// unauthenticated lifetime) keeps its session, and can PLAY again
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_pause_on_anonymous_mount_with_users() {
    let h = start_with(Setup {
        users: alice_bob(),
        permitted: Some(vec!["anonymous".into(), "alice".into()]),
        cfg: ServerConfig {
            unauthenticated_lifetime: Duration::from_secs(1),
            ..Default::default()
        },
        ..Default::default()
    })
    .await;
    let base = format!("rtsp://{}/argus/main", h.addr);
    let mut c = Client::connect(h.addr).await;
    let (session, _) = c.play(&base, None).await;
    c.collect(Duration::from_millis(1500)).await;
    assert_eq!(
        try_request(&mut c, "PAUSE", &base, &[("Session", &session)]).await,
        Some(200)
    );
    assert!(
        !closed_within(&mut c.s, Duration::from_millis(1500)).await,
        "paused anonymous session closed"
    );
    assert_eq!(
        try_request(&mut c, "GET_PARAMETER", &base, &[("Session", &session)]).await,
        Some(200)
    );
    assert_eq!(
        try_request(&mut c, "PLAY", &base, &[("Session", &session)]).await,
        Some(200)
    );
    assert!(has_new_video(&mut c, Duration::from_secs(3)).await);
}

async fn describe_from(addr: SocketAddr, src: &str, user: &str, pass: &str) -> Option<u16> {
    let base = format!("rtsp://{addr}/argus/main");
    let mut c = Client::connect_from(addr, src).await;
    try_request(
        &mut c,
        "DESCRIBE",
        &base,
        &[("Authorization", &basic(user, pass))],
    )
    .await
}

fn throttled(n: usize) -> Setup {
    Setup {
        users: alice_bob(),
        cfg: ServerConfig {
            failed_auth_per_minute: n,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Finding 3: failed auths past the limit are answered at once (no sleeping while
/// holding slots)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_failed_auth_limit_answers_at_once() {
    let h = start_with(throttled(4)).await;
    // 10 wrong passwords over 10 parallel connections: all answered promptly
    let t = Instant::now();
    let mut tasks = vec![];
    for _ in 0..10 {
        let addr = h.addr;
        tasks.push(tokio::spawn(async move {
            describe_from(addr, "127.0.0.2", "alice", "wrong").await
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), Some(401));
    }
    assert!(
        t.elapsed() < Duration::from_millis(1500),
        "failures answered after {:?}",
        t.elapsed()
    );
}

/// Finding 3: past the limit for a user from an address, that user's credentials from
/// there get the answer wrong ones would, also the right password, so a parallel guesser
/// can't tell when it hit. Another user on the same address (a stale password in one
/// client must not lock out Frigate's user) and the same user from another address are
/// not affected
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_failed_auth_limit_is_per_user() {
    let h = start_with(throttled(4)).await;
    let a = h.addr;
    let mut tasks = vec![];
    for _ in 0..6 {
        tasks.push(tokio::spawn(async move {
            describe_from(a, "127.0.0.2", "alice", "wrong").await
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), Some(401));
    }
    assert_eq!(
        describe_from(a, "127.0.0.2", "alice", "alicepw").await,
        Some(401),
        "a right guess past the limit"
    );
    assert_eq!(
        describe_from(a, "127.0.0.2", "bob", "bobpw").await,
        Some(200)
    );
    assert_eq!(
        describe_from(a, "127.0.0.3", "alice", "alicepw").await,
        Some(200)
    );
}

/// Red team: brute force of alice from the address Frigate plays alice from. Frigate's
/// session (keepalives carrying credentials, PAUSE/PLAY, media) is unaffected; new
/// guesses at alice from that address, right or wrong, stay limited
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_failed_auth_limit_keeps_sessions() {
    let h = start_with(throttled(4)).await;
    let a = h.addr;
    let base = format!("rtsp://{a}/argus/main");
    let alice = basic("alice", "alicepw");
    let (mut frigate, session) = player(a, "127.0.0.2", Some(&alice)).await.unwrap();
    let mut tasks = vec![];
    for i in 0..30 {
        tasks.push(tokio::spawn(async move {
            describe_from(a, "127.0.0.2", "alice", &format!("guess{i}")).await
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), Some(401));
    }
    assert_eq!(
        describe_from(a, "127.0.0.2", "alice", "alicepw").await,
        Some(401),
        "a new right guess at alice"
    );
    let auth = [
        ("Session", session.as_str()),
        ("Authorization", alice.as_str()),
    ];
    assert_eq!(
        try_request(&mut frigate, "GET_PARAMETER", &base, &auth).await,
        Some(200),
        "Frigate's keepalive"
    );
    assert_eq!(
        try_request(&mut frigate, "PAUSE", &base, &auth).await,
        Some(200)
    );
    assert_eq!(
        try_request(&mut frigate, "PLAY", &base, &auth).await,
        Some(200)
    );
    assert!(has_new_video(&mut frigate, Duration::from_secs(3)).await);
}

/// Red team: username spraying (many usernames, known or not, one wrong password each)
/// from one address hits the address ceiling (5 × the per-user limit): every user's
/// credentials from there are then refused; below the ceiling they are not
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_failed_auth_username_spraying() {
    let h = start_with(throttled(4)).await;
    let a = h.addr;
    // 19 usernames once each (under 20): bob still gets in
    for i in 0..19 {
        assert_eq!(
            describe_from(a, "127.0.0.4", &format!("user{i}"), "x").await,
            Some(401)
        );
    }
    assert_eq!(
        describe_from(a, "127.0.0.4", "bob", "bobpw").await,
        Some(200)
    );
    // The 20th and on: the address is limited for every username
    for i in 19..25 {
        assert_eq!(
            describe_from(a, "127.0.0.4", &format!("user{i}"), "x").await,
            Some(401)
        );
    }
    assert_eq!(
        describe_from(a, "127.0.0.4", "bob", "bobpw").await,
        Some(401)
    );
    // Unparseable credentials count too (as one empty username)
    let mut c = Client::connect_from(a, "127.0.0.5").await;
    for _ in 0..4 {
        assert_eq!(
            try_request(
                &mut c,
                "DESCRIBE",
                &format!("rtsp://{a}/argus/main"),
                &[("Authorization", "Basic !!!")]
            )
            .await,
            Some(401)
        );
    }
    assert_eq!(
        describe_from(a, "127.0.0.5", "bob", "bobpw").await,
        Some(200)
    );
    assert_eq!(
        describe_from(a, "127.0.0.6", "bob", "bobpw").await,
        Some(200)
    );
}

/// Finding 3: connections with throttled failed auths are answered and don't delay
/// shutdown (the server's tasks end, not only the sockets' write halves)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_failed_auth_does_not_stall_shutdown() {
    let h = start_with(throttled(1)).await;
    let req = format!(
        "DESCRIBE rtsp://{}/argus/main RTSP/1.0\r\nCSeq: 1\r\nAuthorization: {}\r\n\r\n",
        h.addr,
        basic("alice", "wrong")
    );
    let mut conns = vec![];
    for _ in 0..12 {
        let mut c = Client::connect_from(h.addr, "127.0.0.5").await;
        c.s.write_all(req.as_bytes()).await.unwrap();
        conns.push(c);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.cancel.cancel();
    let t = Instant::now();
    while h.server.connection_count() > 0 && t.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        h.server.connection_count(),
        0,
        "connections left {:?} after shutdown",
        t.elapsed()
    );
}

/// Red team: many attacking addresses (8 × 40 connections) and an attacker sharing the
/// victim's address (NAT): the victim still gets in, from a fresh address and from the
/// shared one
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_many_addresses_and_shared_address() {
    let h = start_with(Setup {
        users: alice_bob(),
        ..Default::default()
    })
    .await;
    let mut flood = vec![];
    for i in 2..10 {
        flood.extend(hold(h.addr, &format!("127.0.0.{i}"), 40).await);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let alice = basic("alice", "alicepw");
    let r = player(h.addr, "127.0.0.1", Some(&alice)).await;
    assert!(r.is_ok(), "fresh address: {:?}", r.err());
    let r = player(h.addr, "127.0.0.2", Some(&alice)).await;
    assert!(r.is_ok(), "attacker's address: {:?}", r.err());
    let held = flood.len() - count_closed(&mut flood).await;
    assert!(
        held <= ServerConfig::default().max_unauthenticated_total,
        "{held} unauthenticated connections held"
    );
}

/// Red team: rapid connect/close, half-open requests, pipelined junk and oversized heads
/// leave no slot behind
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_churn_leaves_no_slot_behind() {
    let h = start_with(Setup {
        users: alice_bob(),
        ..Default::default()
    })
    .await;
    let base = format!("rtsp://{}/argus/main", h.addr);
    let mut tasks = vec![];
    for i in 0..300 {
        let base = base.clone();
        let addr = h.addr;
        tasks.push(tokio::spawn(async move {
            let src = format!("127.0.0.{}", 2 + i % 5);
            let mut s = connect_from(addr, &src).await;
            match i % 6 {
                0 => {}
                1 => {
                    let _ = s.write_all(b"OPTIONS rtsp://x/ RTSP/1.0\r\nCSe").await;
                }
                2 => {
                    let r = format!(
                        "OPTIONS {base} RTSP/1.0\r\nCSeq: 1\r\n\r\nDESCRIBE {base} RTSP/1.0\r\nCSeq: 2\r\n\r\n$\x00\x00\x01x"
                    );
                    let _ = s.write_all(r.as_bytes()).await;
                }
                3 => {
                    let _ = s.write_all(&[b'A'; 9000]).await;
                }
                4 => {
                    let r = format!(
                        "DESCRIBE {base} RTSP/1.0\r\nCSeq: 1\r\nAuthorization: {}\r\n\r\n",
                        basic("alice", "x")
                    );
                    let _ = s.write_all(r.as_bytes()).await;
                }
                _ => {
                    let _ = s.write_all(b"\x00\xff garbage\r\n\r\n").await;
                }
            }
            if i % 2 == 0 {
                let _ = s.shutdown().await; // half-close
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let t = Instant::now();
    while h.server.connection_count() > 0 && t.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(h.server.connection_count(), 0);
    assert_eq!(h.server.unauthenticated_count(), 0);
    // And a client still plays. Not from the churning addresses: their wrong passwords
    // may have put them past the failed-auth limit, where bob (never proven there) gets
    // 401 like a wrong password
    let r = player(h.addr, "127.0.0.7", Some(&basic("bob", "bobpw"))).await;
    assert!(r.is_ok(), "{:?}", r.err());
}

/// The per-address and total unauthenticated caps hold for silent connections
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_unauthenticated_caps() {
    let h = start(alice_bob(), Schemes::Both, ServerConfig::default()).await;
    let mut a = hold(h.addr, "127.0.0.2", 100).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let cfg = ServerConfig::default();
    assert_eq!(
        h.server.unauthenticated_count(),
        cfg.max_unauthenticated_per_ip
    );
    assert_eq!(
        count_closed(&mut a).await,
        100 - cfg.max_unauthenticated_per_ip
    );
    let _b = hold(h.addr, "127.0.0.3", 100).await;
    let _c = hold(h.addr, "127.0.0.4", 100).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        h.server.unauthenticated_count(),
        cfg.max_unauthenticated_total
    );
}

/// Red team: a connection flood as fast as the attacker can connect (each connection
/// displaces an older one) doesn't fill max_connections with closing connections, so
/// another address still plays
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_connect_flood() {
    let h = start_with(Setup {
        users: alice_bob(),
        ..Default::default()
    })
    .await;
    let stop = tokio_util::sync::CancellationToken::new();
    let mut flooders = vec![];
    for _ in 0..8 {
        let (addr, stop) = (h.addr, stop.clone());
        flooders.push(tokio::spawn(async move {
            let mut ring = std::collections::VecDeque::new();
            let mut n = 0usize;
            while !stop.is_cancelled() {
                ring.push_back(connect_from(addr, "127.0.0.2").await);
                if ring.len() > 40 {
                    ring.pop_front();
                }
                n += 1;
            }
            n
        }));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let alice = basic("alice", "alicepw");
    for i in 0..3 {
        let r = player(h.addr, "127.0.0.1", Some(&alice)).await;
        assert!(r.is_ok(), "attempt {i}: {:?}", r.err());
    }
    stop.cancel();
    let mut total = 0;
    for f in flooders {
        total += f.await.unwrap();
    }
    assert!(total > 1000, "only {total} flood connections");
}
