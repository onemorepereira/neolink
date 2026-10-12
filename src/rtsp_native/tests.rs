//! The native backend end to end against `FakeCamera` (PLAN 4.7)

use super::*;
use crate::common::tests::wait_stats;
use neolink_core::test_util::{FakeCamera, FakeCameraConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn fixture(name: &str) -> Arc<Vec<neolink_core::fixture::Record>> {
    let path = format!("{}/fixtures/synth/{name}", env!("CARGO_MANIFEST_DIR"));
    let file = std::fs::File::open(&path).expect("run cargo xtask mkfixture --standard first");
    Arc::new(neolink_core::fixture::read_all(file).unwrap())
}

async fn fake() -> FakeCamera {
    FakeCamera::start(FakeCameraConfig {
        main: fixture("plain_h265.bcm"),
        sub: fixture("plain_h264.bcm"),
        loop_media: true,
        ..Default::default()
    })
    .await
    .unwrap()
}

/// Camera name → a delay at the start of its `camera_main`: a camera slow to set up
static START_DELAYS: std::sync::Mutex<Vec<(String, Duration)>> = std::sync::Mutex::new(Vec::new());

pub(super) async fn start_delay(name: &str) {
    let d = START_DELAYS
        .lock()
        .unwrap()
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, d)| *d);
    if let Some(d) = d {
        tokio::time::sleep(d).await;
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A config serving `cam` as `name`; `top` goes before the camera, `extra` into it, `tail`
/// after it (e.g. `[[users]]`)
fn config(cam: &FakeCamera, port: u16, name: &str, top: &str, extra: &str, tail: &str) -> Config {
    // idle_grace_s = 1 unless `extra` sets it
    let grace = if extra.contains("idle_grace_s") {
        ""
    } else {
        "idle_grace_s = 1\n"
    };
    let text = format!(
        "bind = \"127.0.0.1\"\nbind_port = {port}\nsnapshot_port = 0\nrtsp_backend = \"native\"\n{top}\n\
         [[cameras]]\nname = \"{name}\"\nusername = \"admin\"\npassword = \"fakepass\"\n\
         address = \"{}\"\n{grace}{extra}\n{tail}\n",
        cam.addr()
    );
    toml::from_str(&text).unwrap()
}

struct Running {
    reactor: NeoReactor,
    task: tokio::task::JoinHandle<Result<()>>,
    port: u16,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Running {
    /// What `main` does on a signal: drop the server's future (the abort lands once the
    /// task has been dropped)
    async fn drop_server(&mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

async fn run(config: Config) -> Running {
    let _ = env_logger::builder().is_test(true).try_init();
    let port = config.bind_port;
    let reactor = NeoReactor::new(config).await;
    let mut task = tokio::spawn(main(reactor.clone()));
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // free_port() gave the port up: another process may have taken it since, and the
    // connect above then reached that process's server
    if task.is_finished() {
        panic!("the server ended at start: {:?}", (&mut task).await);
    }
    Running {
        reactor,
        task,
        port,
    }
}

/// A minimal RTSP client that counts interleaved packets per channel
struct Client {
    s: TcpStream,
    buf: Vec<u8>,
    cseq: u32,
    packets: HashMap<u8, usize>,
    session: Option<String>,
}

struct Resp {
    status: u16,
    head: String,
    body: String,
}

impl Resp {
    fn header(&self, k: &str) -> Option<&str> {
        self.head.lines().find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.trim().eq_ignore_ascii_case(k).then(|| v.trim())
        })
    }
}

impl Client {
    async fn connect(port: u16) -> Self {
        Self {
            s: TcpStream::connect(("127.0.0.1", port)).await.unwrap(),
            buf: vec![],
            cseq: 0,
            packets: HashMap::new(),
            session: None,
        }
    }

    /// Read more; false on EOF, error or 10 s of silence
    async fn fill(&mut self) -> bool {
        let mut chunk = vec![0u8; 65536];
        match tokio::time::timeout(Duration::from_secs(10), self.s.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => {
                self.buf.extend_from_slice(&chunk[..n]);
                true
            }
            _ => false,
        }
    }

    fn take_packets(&mut self) {
        while self.buf.first() == Some(&b'$') && self.buf.len() >= 4 {
            let len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
            if self.buf.len() < 4 + len {
                break;
            }
            *self.packets.entry(self.buf[1]).or_default() += 1;
            self.buf.drain(..4 + len);
        }
    }

    async fn response(&mut self) -> Option<Resp> {
        loop {
            self.take_packets();
            if self.buf.first() != Some(&b'$') {
                if let Some(end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&self.buf[..end]).to_string();
                    let status = head.split(' ').nth(1)?.parse().ok()?;
                    let cl: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .map(|v| v.parse().unwrap())
                        .unwrap_or(0);
                    while self.buf.len() < end + 4 + cl {
                        if !self.fill().await {
                            return None;
                        }
                    }
                    let body =
                        String::from_utf8_lossy(&self.buf[end + 4..end + 4 + cl]).to_string();
                    self.buf.drain(..end + 4 + cl);
                    return Some(Resp { status, head, body });
                }
            }
            if !self.fill().await {
                return None;
            }
        }
    }

    async fn request(&mut self, method: &str, url: &str, extra: &[(&str, &str)]) -> Resp {
        self.cseq += 1;
        let mut r = format!("{method} {url} RTSP/1.0\r\nCSeq: {}\r\n", self.cseq);
        if let Some(s) = &self.session {
            r.push_str(&format!("Session: {s}\r\n"));
        }
        for (k, v) in extra {
            r.push_str(&format!("{k}: {v}\r\n"));
        }
        r.push_str("\r\n");
        self.s.write_all(r.as_bytes()).await.unwrap();
        self.response().await.expect("a response")
    }

    /// DESCRIBE, SETUP video (and audio if offered), PLAY
    async fn play(&mut self, url: &str, extra: &[(&str, &str)]) -> Resp {
        let d = self.request("DESCRIBE", url, extra).await;
        assert_eq!(d.status, 200, "DESCRIBE: {}", d.head);
        for (track, ch) in [(0, "0-1"), (1, "2-3")] {
            if track == 1 && !d.body.contains("m=audio") {
                continue;
            }
            let t = format!("RTP/AVP/TCP;unicast;interleaved={ch}");
            let mut e = extra.to_vec();
            e.push(("Transport", &t));
            let r = self
                .request("SETUP", &format!("{url}/stream={track}"), &e)
                .await;
            assert_eq!(r.status, 200, "SETUP: {}", r.head);
            self.session = r
                .header("Session")
                .map(|s| s.split(';').next().unwrap().to_string());
        }
        let r = self.request("PLAY", url, extra).await;
        assert_eq!(r.status, 200, "PLAY: {}", r.head);
        d
    }

    /// Count packets for `d`; false if the connection closed
    async fn collect(&mut self, d: Duration) -> bool {
        let end = tokio::time::Instant::now() + d;
        let mut chunk = vec![0u8; 65536];
        loop {
            self.take_packets();
            match tokio::time::timeout_at(end, self.s.read(&mut chunk)).await {
                Ok(Ok(n)) if n > 0 => self.buf.extend_from_slice(&chunk[..n]),
                Ok(_) => return false,
                Err(_) => return true,
            }
        }
    }

    fn video(&self) -> usize {
        self.packets.get(&0).copied().unwrap_or(0)
    }
}

/// DESCRIBE on a sleeping camera is one learning wake that PLAY continues; the camera is
/// released after the session (linger + idle grace)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_native_describe_play_release() {
    let cam = fake().await;
    let port = free_port();
    let r = run(config(&cam, port, "native1", "", "", "")).await;
    assert_eq!(cam.stats().connections, 0, "P1: no contact at boot");
    let url = format!("rtsp://127.0.0.1:{}/native1/main", r.port);
    let mut c = Client::connect(r.port).await;
    let d = c.play(&url, &[]).await;
    assert!(d.body.contains("H265/90000"), "{}", d.body);
    assert!(d.body.contains("MPEG4-GENERIC/16000/1"), "{}", d.body);
    assert!(c.collect(Duration::from_secs(3)).await);
    assert!(c.video() > 30, "{} video packets", c.video());
    assert!(c.packets.get(&2).copied().unwrap_or(0) > 10, "audio");
    // One wake (login) for DESCRIBE + PLAY; while awake the sub stream learns its desc
    // too (P18: one wake teaches every stream kind)
    let st = cam.stats();
    assert_eq!(st.logins, 1, "one wake for DESCRIBE + PLAY");
    assert_eq!(
        st.video_starts, 2,
        "main, and sub learning in the same wake"
    );
    let t = c.request("TEARDOWN", &url, &[]).await;
    assert_eq!(t.status, 200);
    // Hub linger (4 s), then the camera's idle grace (1 s): stop, logout, closed
    wait_stats(&cam, 15, |s| s.video_stops >= 1 && s.open_connections == 0).await;
    // The sub stream and the aliases are mounted too
    let mut c = Client::connect(r.port).await;
    let d = c
        .request(
            "DESCRIBE",
            &format!("rtsp://127.0.0.1:{}/native1/subStream", r.port),
            &[],
        )
        .await;
    assert_eq!(d.status, 200);
    assert!(d.body.contains("H264/90000"));
}

/// A21: after a restart with the desc cached, DESCRIBE probes never wake the camera
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_native_a21_probes_on_cached_mount() {
    let cam = fake().await;
    let port = free_port();
    {
        let r = run(config(&cam, port, "native-a21", "", "", "")).await;
        let mut c = Client::connect(r.port).await;
        let d = c
            .request(
                "DESCRIBE",
                &format!("rtsp://127.0.0.1:{port}/native-a21/main"),
                &[],
            )
            .await;
        assert_eq!(d.status, 200);
        wait_stats(&cam, 20, |s| s.open_connections == 0).await;
        r.reactor.shutdown().await;
    }
    let before = cam.stats();
    let port = free_port();
    let r = run(config(&cam, port, "native-a21", "", "", "")).await;
    // Longer than the 15 s stream-info poll, which must not wake it either
    for _ in 0..22 {
        let mut c = Client::connect(r.port).await;
        let d = c
            .request(
                "DESCRIBE",
                &format!("rtsp://127.0.0.1:{port}/native-a21/main"),
                &[],
            )
            .await;
        assert_eq!(d.status, 200, "{}", d.head);
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
    let after = cam.stats();
    assert_eq!(
        after.connections, before.connections,
        "probes contacted the camera"
    );
    assert_eq!(after.video_starts, before.video_starts);
}

/// A12: `pause.on_motion`. Idle: the session stays up without camera media; motion starts
/// the camera stream; its end stops it again; the session survives both. The camera stays
/// connected (`idle_disconnect = false`): a camera reports motion only over an open
/// connection, so a sleeping one needs push notifications for this (owner-in-the-loop
/// hardware test)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_native_a12_pause_on_motion() {
    let cam = fake().await;
    let port = free_port();
    let r = run(config(
        &cam,
        port,
        "native-a12",
        "",
        // bridge_max_s at its minimum: the 60 s idle below outlasts it, so the motion pause
        // must not be capped by it (A12's 10 min variant, scaled)
        "stream = \"main\"\nidle_disconnect = false\nbridge_max_s = 10\n[cameras.pause]\non_motion = true",
        "",
    ))
    .await;
    let url = format!("rtsp://127.0.0.1:{port}/native-a12/main");
    let mut c = Client::connect(r.port).await;
    c.play(&url, &[]).await;
    // The DESCRIBE learning start ends; no motion: no further camera stream for 60 s
    assert!(
        c.collect(Duration::from_secs(60)).await,
        "session kept while idle"
    );
    let starts = cam.stats().video_starts;
    assert_eq!(starts, 1, "only the learning start");
    wait_stats(&cam, 10, |s| s.video_stops >= 1).await;
    let idle_video = c.video();
    cam.motion(true);
    wait_stats(&cam, 10, |s| s.video_starts == starts + 1).await;
    assert!(c.collect(Duration::from_secs(3)).await);
    assert!(c.video() > idle_video + 30, "live media on motion");
    cam.motion(false);
    wait_stats(&cam, 10, |s| s.video_stops >= 2).await;
    assert!(
        c.collect(Duration::from_secs(2)).await,
        "session kept after motion"
    );
}

/// Hot reload: users require auth from the next request; a removed camera's sessions end
/// and its mounts go; a reload adding `certificate` is rejected and serving goes on
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_native_hot_reload() {
    let cam = fake().await;
    let port = free_port();
    let base = config(&cam, port, "native-hr", "", "", "");
    let r = run(base.clone()).await;
    let url = format!("rtsp://127.0.0.1:{port}/native-hr/main");
    let mut playing = Client::connect(r.port).await;
    playing.play(&url, &[]).await;
    assert!(playing.collect(Duration::from_secs(1)).await);

    // Users
    let with_users = config(
        &cam,
        port,
        "native-hr",
        "",
        "",
        "[[users]]\nname = \"alice\"\npass = \"alicepw\"",
    );
    r.reactor.update_config(with_users.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut c = Client::connect(r.port).await;
    assert_eq!(c.request("DESCRIBE", &url, &[]).await.status, 401);
    let basic = format!(
        "Basic {}",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "alice:alicepw")
    );
    assert_eq!(
        c.request("DESCRIBE", &url, &[("Authorization", &basic)])
            .await
            .status,
        200
    );

    // certificate: the whole reload is refused, so nothing in it applies, here a
    // permitted_users that would lock alice out
    let mut with_cert = with_users.clone();
    with_cert.certificate = Some("/nonexistent.pem".into());
    with_cert.cameras[0].permitted_users = Some(vec!["bob".into()]);
    assert!(r.reactor.update_config(with_cert).await.is_err());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut c = Client::connect(r.port).await;
    assert_eq!(
        c.request("DESCRIBE", &url, &[("Authorization", &basic)])
            .await
            .status,
        200
    );

    // Remove the camera: the playing session ends, the mount is gone
    let mut gone = with_users.clone();
    gone.cameras.clear();
    r.reactor.update_config(gone).await.unwrap();
    assert!(
        !playing.collect(Duration::from_secs(5)).await,
        "the removed camera's session ended"
    );
    let mut c = Client::connect(r.port).await;
    assert_eq!(
        c.request("DESCRIBE", &url, &[("Authorization", &basic)])
            .await
            .status,
        404
    );
    // ... and re-added under the same name: served again, with its permitted users
    let mut back = with_users.clone();
    back.cameras[0].permitted_users = Some(vec!["alice".into()]);
    r.reactor.update_config(back).await.unwrap();
    let t = tokio::time::Instant::now();
    loop {
        let mut c = Client::connect(r.port).await;
        let st = c
            .request("DESCRIBE", &url, &[("Authorization", &basic)])
            .await
            .status;
        if st == 200 {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "re-added camera: {}",
            st
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A8 (neolink part): connect/teardown cycles return the camera use count and the task
/// count to their baselines
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_native_a8_cycles() {
    let cam = fake().await;
    let port = free_port();
    let r = run(config(&cam, port, "native-a8", "", "", "")).await;
    let url = format!("rtsp://127.0.0.1:{port}/native-a8/main");
    let cycle = || async {
        let mut c = Client::connect(r.port).await;
        c.play(&url, &[]).await;
        while c.video() == 0 {
            assert!(c.fill().await);
            c.take_packets();
        }
        assert_eq!(c.request("TEARDOWN", &url, &[]).await.status, 200);
    };
    cycle().await;
    wait_stats(&cam, 20, |s| s.open_connections == 0).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let metrics = tokio::runtime::Handle::current().metrics();
    let baseline = metrics.num_alive_tasks();
    for _ in 0..30 {
        cycle().await;
    }
    wait_stats(&cam, 20, |s| s.open_connections == 0).await;
    let t = tokio::time::Instant::now();
    while metrics.num_alive_tasks() > baseline && t.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        metrics.num_alive_tasks() <= baseline,
        "{} tasks vs {baseline}",
        metrics.num_alive_tasks()
    );
    let instance = r.reactor.get("native-a8").await.unwrap();
    assert_eq!(
        instance.get_state().await.unwrap(),
        crate::common::NeoCamThreadState::Disconnected
    );
}

/// `battery` unknown is treated as battery: learning starts are rate-limited
#[test]
fn test_hub_config_battery_unknown_is_battery() {
    let c: CameraConfig =
        toml::from_str("name = \"x\"\nusername = \"a\"\naddress = \"127.0.0.1:9000\"").unwrap();
    assert_eq!(c.battery, None);
    assert!(hub_config(&c, StreamKind::Main, 1400).battery);
    let c: CameraConfig = toml::from_str(
        "name = \"x\"\nusername = \"a\"\naddress = \"127.0.0.1:9000\"\nbattery = false",
    )
    .unwrap();
    assert!(!hub_config(&c, StreamKind::Main, 1400).battery);
}

/// A7 (neolink part): main and sub at once share the camera connection; ending the main
/// session doesn't stop the sub stream
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_native_main_and_sub_together() {
    let cam = fake().await;
    let port = free_port();
    let r = run(config(&cam, port, "native-a7", "", "", "")).await;
    let main_url = format!("rtsp://127.0.0.1:{port}/native-a7/main");
    let sub_url = format!("rtsp://127.0.0.1:{port}/native-a7/sub");
    let mut m = Client::connect(r.port).await;
    m.play(&main_url, &[]).await;
    let mut s = Client::connect(r.port).await;
    s.play(&sub_url, &[]).await;
    // A late joiner's live video starts at the next IRAP (up to one 2 s GOP)
    assert!(m.collect(Duration::from_secs(4)).await);
    assert!(s.collect(Duration::from_secs(1)).await);
    assert!(
        m.video() > 20 && s.video() > 10,
        "main {} sub {}",
        m.video(),
        s.video()
    );
    assert_eq!(m.request("TEARDOWN", &main_url, &[]).await.status, 200);
    // Past the main Hub's linger: its camera stream stops; sub goes on
    wait_stats(&cam, 15, |st| st.video_stops >= 1).await;
    let before = s.video();
    assert!(s.collect(Duration::from_secs(3)).await);
    assert!(
        s.video() > before + 20,
        "sub stalled after main stopped: {before} -> {}",
        s.video()
    );
    assert_eq!(cam.stats().video_starts, 2, "one camera stream per kind");
    assert_eq!(cam.stats().logins, 1, "one wake");
}

/// A taken snapshot port (8555 is go2rtc's WebRTC default) is not fatal
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_native_snapshot_port_taken() {
    let cam = fake().await;
    let port = free_port();
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let snap = taken.local_addr().unwrap().port();
    let mut cfg = config(&cam, port, "native-snap", "", "", "");
    cfg.snapshot_port = Some(snap);
    let r = run(cfg).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut c = Client::connect(r.port).await;
    let url = format!("rtsp://127.0.0.1:{}/native-snap/main", port);
    assert_eq!(c.request("OPTIONS", &url, &[]).await.status, 200);
    assert!(!r.task.is_finished(), "the RTSP server must keep running");
    drop(taken);
}

/// R27: on shutdown a playing client gets BYE and the camera stream stops, before the
/// reactor logs out
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_native_shutdown_sends_bye() {
    let cam = fake().await;
    let port = free_port();
    let mut r = run(config(&cam, port, "native-bye", "", "", "")).await;
    let url = format!("rtsp://127.0.0.1:{}/native-bye/main", port);
    let mut c = Client::connect(r.port).await;
    c.play(&url, &[]).await;
    assert!(c.collect(Duration::from_secs(2)).await);
    // What main.rs does on SIGTERM: drop the server future, then wait for it to end
    r.drop_server().await;
    shutdown().await;
    let mut all = c.buf.clone();
    let mut chunk = vec![0u8; 65536];
    loop {
        match tokio::time::timeout(Duration::from_secs(3), c.s.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => all.extend_from_slice(&chunk[..n]),
            _ => break,
        }
    }
    assert!(
        all.windows(2).any(|w| w == [0x81, 203]),
        "no RTCP BYE before the close"
    );
    wait_stats(&cam, 5, |s| s.video_stops >= 1).await;
    r.reactor.shutdown().await;
    wait_stats(&cam, 5, |s| s.logouts >= 1).await;
}

/// Clients open main and sub at once on a sleeping camera: both DESCRIBEs succeed in the
/// same wake (the learning gate's shared-wake window), with one login
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_native_cold_main_and_sub_describe_together() {
    let cam = fake().await;
    let port = free_port();
    let r = run(config(&cam, port, "native-cold2", "", "", "")).await;
    let mut tasks = vec![];
    for kind in ["main", "sub"] {
        let url = format!("rtsp://127.0.0.1:{}/native-cold2/{}", port, kind);
        let p = r.port;
        tasks.push(tokio::spawn(async move {
            let mut c = Client::connect(p).await;
            c.request("DESCRIBE", &url, &[]).await.status
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }
    assert_eq!(cam.stats().logins, 1, "one wake");
}

/// A client that connects as soon as the port opens gets the configured camera, even when
/// that camera is slow to set up: the server accepts nobody before the cameras configured
/// at start have their mounts (a 404 there flaked test_native_cold_main_and_sub_describe_together)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_native_no_404_before_mounts() {
    START_DELAYS
        .lock()
        .unwrap()
        .push(("native-slowstart".to_string(), Duration::from_millis(500)));
    let cam = fake().await;
    let port = free_port();
    let r = run(config(&cam, port, "native-slowstart", "", "", "")).await;
    let mut c = Client::connect(r.port).await;
    let d = c
        .request(
            "DESCRIBE",
            &format!("rtsp://127.0.0.1:{}/native-slowstart/main", port),
            &[],
        )
        .await;
    assert_eq!(d.status, 200, "{}", d.head);
}

/// An always-connected camera serves its mounts from the start, not only once it has
/// connected and reported its streams (hardware session 4 got 404 there)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_native_always_on_mounts_at_start() {
    let cam = fake().await;
    let port = free_port();
    let r = run(config(
        &cam,
        port,
        "native-on",
        "",
        "idle_disconnect = false",
        "",
    ))
    .await;
    let mut c = Client::connect(r.port).await;
    let d = c
        .request(
            "DESCRIBE",
            &format!("rtsp://127.0.0.1:{}/native-on/main", port),
            &[],
        )
        .await;
    assert_ne!(d.status, 404, "{}", d.head);
}

/// A12 from a cold start: the desc is cached, the camera stays connected with
/// pause.on_motion and nothing moves. A client gets the cached IRAP and then bridged
/// frames and silent audio while it waits (hardware session 4 got nothing until motion,
/// which Frigate's 5 s -timeout would not survive)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_native_cold_motion_paused_bridges() {
    let cam = fake().await;
    // Learn once, so the next run starts from the cache
    {
        let port = free_port();
        let mut r = run(config(&cam, port, "native-coldpause", "", "", "")).await;
        let mut c = Client::connect(r.port).await;
        c.play(
            &format!("rtsp://127.0.0.1:{}/native-coldpause/main", port),
            &[],
        )
        .await;
        assert!(c.collect(Duration::from_secs(3)).await);
        drop(c);
        r.drop_server().await;
        shutdown().await;
        r.reactor.shutdown().await;
    }
    let port = free_port();
    let r = run(config(
        &cam,
        port,
        "native-coldpause",
        "",
        "stream = \"main\"\nidle_disconnect = false\n[cameras.pause]\non_motion = true",
        "",
    ))
    .await;
    let mut c = Client::connect(r.port).await;
    c.play(
        &format!("rtsp://127.0.0.1:{}/native-coldpause/main", port),
        &[],
    )
    .await;
    assert!(c.collect(Duration::from_secs(12)).await);
    let video = c.video();
    let audio = c.packets.get(&2).copied().unwrap_or(0);
    assert!(video >= 2, "{} video packets while paused", video);
    assert!(audio > 100, "{} audio packets while paused", audio);
}

// ---------------------------------------------------------------------------------------
// Shutdown logs every camera out, whatever its streams are doing (container red-team:
// the native backend left a camera that was still learning its streams logged in)

/// What `main` does on SIGTERM/SIGINT: drop the server future, wait for the native server
/// to end, then shut the cameras down. Returns how long that took
async fn stop_like_main(r: &mut Running) -> Duration {
    let t = tokio::time::Instant::now();
    r.drop_server().await;
    shutdown().await;
    r.reactor.shutdown().await;
    t.elapsed()
}

/// Every login was matched by a logout by the time shutdown returned (the fake counts a
/// message when it reads it: allow it a moment), and shutdown stayed well inside
/// docker's 10 s stop timeout
async fn assert_logged_out(cam: &FakeCamera, took: Duration, what: &str) {
    tokio::time::sleep(Duration::from_millis(200)).await;
    let s = cam.stats();
    eprintln!("{what}: shutdown took {took:?}; {s:?}");
    assert!(
        s.logins >= 1,
        "{}: the camera never logged in: {:?}",
        what,
        s
    );
    assert_eq!(
        s.logouts, s.logins,
        "{what}: camera not logged out on shutdown: {s:?}"
    );
    assert!(
        took < Duration::from_secs(5),
        "{}: shutdown took {:?}",
        what,
        took
    );
}

/// A camera with no media, so a learning start never learns the desc, that takes
/// `msg_delay` over each request (a busy camera handles them in order)
async fn fake_silent(login_delay: Duration, msg_delay: Duration) -> FakeCamera {
    FakeCamera::start(FakeCameraConfig {
        login_delay,
        msg_delay,
        ..Default::default()
    })
    .await
    .unwrap()
}

/// Sends a DESCRIBE without waiting for its answer (it blocks while learning); the
/// connection stays open until the task is aborted
fn describe_in_background(port: u16, path: &str) -> tokio::task::JoinHandle<()> {
    let url = format!("rtsp://127.0.0.1:{port}/{path}");
    tokio::spawn(async move {
        let mut c = Client::connect(port).await;
        let req = format!("DESCRIBE {url} RTSP/1.0\r\nCSeq: 1\r\n\r\n");
        c.s.write_all(req.as_bytes()).await.unwrap();
        let _ = c.response().await;
        std::future::pending::<()>().await;
    })
}

/// A mains camera connects at boot and every stream starts learning (P18); a signal
/// before any desc is learned must still log it out. The camera is busy, so the stream
/// stops that shutdown sends are still being handled when the logout follows them (what
/// the container matrix hit, intermittently on the host)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_native_shutdown_while_learning_logs_out() {
    let cam = fake_silent(Duration::ZERO, Duration::from_millis(30)).await;
    let mut r = run(config(
        &cam,
        free_port(),
        "native-sd-learn",
        "",
        "battery = false",
        "",
    ))
    .await;
    wait_stats(&cam, 10, |s| s.logins == 1 && s.video_starts >= 1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let took = stop_like_main(&mut r).await;
    assert_logged_out(&cam, took, "learning (mains)").await;
}

/// A sleeping (busy) camera woken by a DESCRIBE that is still learning the stream
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_native_shutdown_while_describe_learns_logs_out() {
    let cam = fake_silent(Duration::ZERO, Duration::from_millis(30)).await;
    let port = free_port();
    let mut r = run(config(&cam, port, "native-sd-describe", "", "", "")).await;
    let client = describe_in_background(port, "native-sd-describe/main");
    wait_stats(&cam, 10, |s| s.logins == 1 && s.video_starts >= 1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let took = stop_like_main(&mut r).await;
    assert_logged_out(&cam, took, "learning (DESCRIBE)").await;
    client.abort();
}

/// A signal while the camera is still answering the login of a wake: the session it
/// opened is closed with a logout once the login completes, within the bound (2 s)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_native_shutdown_while_waking_logs_out() {
    let cam = fake_silent(Duration::from_millis(1500), Duration::ZERO).await;
    let port = free_port();
    let mut r = run(config(&cam, port, "native-sd-wake", "", "", "")).await;
    let client = describe_in_background(port, "native-sd-wake/main");
    // The login request has arrived; its answer is 1.5 s away
    wait_stats(&cam, 10, |s| s.logins == 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let took = stop_like_main(&mut r).await;
    assert_logged_out(&cam, took, "waking").await;
    client.abort();
}

/// A signal while a client is playing
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_native_shutdown_while_playing_logs_out() {
    let cam = fake().await;
    let port = free_port();
    let mut r = run(config(&cam, port, "native-sd-play", "", "", "")).await;
    let mut c = Client::connect(port).await;
    c.play(&format!("rtsp://127.0.0.1:{port}/native-sd-play/main"), &[])
        .await;
    assert!(c.collect(Duration::from_secs(1)).await);
    let took = stop_like_main(&mut r).await;
    assert_logged_out(&cam, took, "playing").await;
}

/// A signal while the camera is idle but still connected, inside idle_grace_s
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_native_shutdown_in_idle_grace_logs_out() {
    let cam = fake().await;
    let port = free_port();
    let mut r = run(config(
        &cam,
        port,
        "native-sd-idle",
        "",
        "idle_grace_s = 30",
        "",
    ))
    .await;
    let url = format!("rtsp://127.0.0.1:{port}/native-sd-idle/main");
    let mut c = Client::connect(port).await;
    c.play(&url, &[]).await;
    assert!(c.collect(Duration::from_secs(1)).await);
    assert_eq!(c.request("TEARDOWN", &url, &[]).await.status, 200);
    drop(c);
    // The streams stop (Hub linger, learning windows); the connection stays for the grace
    wait_stats(&cam, 20, |s| s.video_stops >= s.video_starts).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(cam.stats().open_connections, 1, "{:?}", cam.stats());
    let took = stop_like_main(&mut r).await;
    assert_logged_out(&cam, took, "idle grace").await;
}
