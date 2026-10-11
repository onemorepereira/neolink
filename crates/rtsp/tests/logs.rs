//! A20 / R25: credentials never reach the logs, at any level. Its own test binary, since it
//! installs a capturing logger.

mod common;

use common::*;
use neolink_rtsp::rtsp::auth::Schemes;
use std::sync::Mutex;
use std::time::Duration;

static LINES: Mutex<Vec<String>> = Mutex::new(vec![]);

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        LINES
            .lock()
            .unwrap()
            .push(format!("{} {}: {}", r.level(), r.target(), r.args()));
    }
    fn flush(&self) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_no_credentials_in_logs() {
    log::set_logger(&Capture).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let h = start_with(Setup {
        users: alice_bob(),
        schemes: Schemes::Both,
        permitted: Some(vec!["alice".into()]),
        ..Default::default()
    })
    .await;
    let base = format!("rtsp://{}/argus/main", h.addr);
    let with_userinfo = format!("rtsp://alice:alicepw@{}/argus/main", h.addr);
    let alice = basic("alice", "alicepw");
    let wrong = basic("alice", "wrongpw");
    let bob = basic("bob", "bobpw");
    let digest = format!(
        "Digest username=\"alice\", realm=\"neolink\", nonce=\"x\", uri=\"{base}\", response=\"0123456789abcdef0123456789abcdef\""
    );
    let mut c = Client::connect(h.addr).await;
    for auth in [&wrong, &bob, &digest, &alice] {
        c.request("DESCRIBE", &base, &[("Authorization", auth)])
            .await;
    }
    c.request("DESCRIBE", &with_userinfo, &[("Authorization", &alice)])
        .await;
    let (session, _) = c.play(&base, Some(&alice)).await;
    c.collect(Duration::from_secs(1)).await;
    c.request(
        "GET_PARAMETER",
        &base,
        &[("Session", &session), ("Authorization", &wrong)],
    )
    .await;
    c.request(
        "TEARDOWN",
        &base,
        &[("Session", &session), ("Authorization", &alice)],
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let lines = LINES.lock().unwrap().clone();
    assert!(lines.len() > 5, "the logger captured {} lines", lines.len());
    let secrets = [
        "alicepw",
        "bobpw",
        "wrongpw",
        alice.trim_start_matches("Basic "),
        wrong.trim_start_matches("Basic "),
        bob.trim_start_matches("Basic "),
        "0123456789abcdef0123456789abcdef",
        "Authorization",
        "authorization",
    ];
    for l in &lines {
        for s in secrets {
            assert!(
                !l.contains(s),
                "credential material {s:?} in a log line: {l}"
            );
        }
    }
}
