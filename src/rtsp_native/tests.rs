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
    std::env::set_var(
        "NEOLINK_STATE_DIR",
        std::env::temp_dir().join(format!("neolink-native-tests-{}", std::process::id())),
    );
    let text = format!(
        "bind = \"127.0.0.1\"\nbind_port = {port}\nsnapshot_port = 0\nrtsp_backend = \"native\"\n{top}\n\
         [[cameras]]\nname = \"{name}\"\nusername = \"admin\"\npassword = \"fakepass\"\n\
         address = \"{}\"\nidle_grace_s = 1\n{extra}\n{tail}\n",
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

async fn run(config: Config) -> Running {
    let _ = env_logger::builder().is_test(true).try_init();
    let port = config.bind_port;
    let reactor = NeoReactor::new(config).await;
    let task = tokio::spawn(main(reactor.clone()));
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
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
    assert_eq!(cam.stats().video_starts, 1, "one wake for DESCRIBE + PLAY");
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
    for _ in 0..12 {
        let mut c = Client::connect(r.port).await;
        let d = c
            .request(
                "DESCRIBE",
                &format!("rtsp://127.0.0.1:{port}/native-a21/main"),
                &[],
            )
            .await;
        assert_eq!(d.status, 200, "{}", d.head);
        tokio::time::sleep(Duration::from_millis(500)).await;
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
        // bridge_max_s at its minimum: the 12 s idle below outlasts it, so the motion pause
        // must not be capped by it (A12's 10 min variant, scaled)
        "idle_disconnect = false\nbridge_max_s = 10\n[cameras.pause]\non_motion = true",
        "",
    ))
    .await;
    let url = format!("rtsp://127.0.0.1:{port}/native-a12/main");
    let mut c = Client::connect(r.port).await;
    c.play(&url, &[]).await;
    // The DESCRIBE learning start ends; no motion: no further camera stream
    assert!(
        c.collect(Duration::from_secs(12)).await,
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

    // certificate: rejected, still serving
    let mut with_cert = with_users.clone();
    with_cert.certificate = Some("/nonexistent.pem".into());
    let _ = r.reactor.update_config(with_cert).await;
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
