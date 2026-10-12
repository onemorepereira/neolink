//! Shared harness for the server integration tests: the server on fixture sources and a
//! minimal RTSP client
#![allow(dead_code)]

use neolink_rtsp::hub::task::{spawn, HubConfig, HubHandle};
use neolink_rtsp::rtsp::auth::{Allowed, Auth, Schemes, User};
use neolink_rtsp::rtsp::route::{aliases, MountTarget, Routes, StreamKind};
use neolink_rtsp::rtsp::server::{Provider, Server, ServerConfig};
use neolink_rtsp::source::{parse_bcm, FixtureSource};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

pub struct TestProvider {
    pub hubs: HashMap<MountTarget, HubHandle>,
    pub allowed: Allowed,
}

impl Provider for TestProvider {
    fn hub(&self, target: &MountTarget) -> Option<HubHandle> {
        self.hubs.get(target).cloned()
    }
    fn allowed(&self, _camera: &str) -> Allowed {
        self.allowed.clone()
    }
}

pub fn fixture(name: &str) -> Vec<neolink_rtsp::source::FixtureUnit> {
    let path = format!("{}/../../fixtures/synth/{name}", env!("CARGO_MANIFEST_DIR"));
    let data = std::fs::read(&path)
        .unwrap_or_else(|_| panic!("{path}: run `cargo xtask mkfixture --standard` first"));
    parse_bcm(&data)
}

pub struct Harness {
    pub addr: SocketAddr,
    pub server: Arc<Server>,
    pub cancel: CancellationToken,
    pub sources: Vec<Arc<FixtureSource>>,
    pub hubs: Vec<HubHandle>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// What the server under test serves
pub struct Setup {
    pub main: &'static str,
    pub sub: &'static str,
    pub users: Vec<User>,
    pub schemes: Schemes,
    pub permitted: Option<Vec<String>>,
    pub cfg: ServerConfig,
    pub hub: fn(&mut HubConfig),
    /// Persist descs here (`DescCache`)
    pub state_dir: Option<std::path::PathBuf>,
    /// Simulated camera wake time
    pub wake: Duration,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            main: "plain_h265.bcm",
            sub: "plain_h264.bcm",
            users: vec![],
            schemes: Schemes::Both,
            permitted: None,
            cfg: ServerConfig::default(),
            hub: |_| {},
            state_dir: None,
            wake: Duration::ZERO,
        }
    }
}

/// A fresh directory under the system temp dir
pub fn temp_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "neolink-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn alice_bob() -> Vec<User> {
    ["alice", "bob"]
        .iter()
        .map(|n| User {
            name: n.to_string(),
            pass: Some(format!("{n}pw")),
        })
        .collect()
}

pub async fn start(users: Vec<User>, schemes: Schemes, cfg: ServerConfig) -> Harness {
    start_with(Setup {
        users,
        schemes,
        cfg,
        ..Default::default()
    })
    .await
}

/// sources[0] is main, sources[1] sub; hubs likewise
pub async fn start_with(setup: Setup) -> Harness {
    let _ = env_logger::builder().is_test(true).try_init();
    let mut hubs = HashMap::new();
    let mut sources = vec![];
    let mut handles = vec![];
    for (kind, file) in [(StreamKind::Main, setup.main), (StreamKind::Sub, setup.sub)] {
        let source = Arc::new(FixtureSource::new(fixture(file), 1.0, Some(false)));
        source.start_delay_ms.store(
            setup.wake.as_millis() as u64,
            std::sync::atomic::Ordering::SeqCst,
        );
        let mut hc = HubConfig::new(kind.name());
        hc.battery = false;
        hc.cache = setup
            .state_dir
            .as_ref()
            .map(|d| neolink_rtsp::hub::desc::DescCache::new(d, "argus", kind.name()));
        (setup.hub)(&mut hc);
        let h = spawn(hc, source.clone());
        hubs.insert(
            MountTarget {
                camera: "argus".into(),
                kind,
            },
            h.clone(),
        );
        sources.push(source);
        handles.push(h);
    }
    let have_users = !setup.users.is_empty();
    let routes = Routes::default();
    routes.set(aliases("argus", &[StreamKind::Main, StreamKind::Sub]));
    let provider = Arc::new(TestProvider {
        hubs,
        allowed: Allowed::from_config(setup.permitted.as_deref(), have_users),
    });
    let server = Server::new(
        setup.cfg,
        routes,
        Auth::new(setup.users, setup.schemes),
        provider,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    tokio::spawn(server.clone().serve(listener, cancel.clone()));
    Harness {
        addr,
        server,
        cancel,
        sources,
        hubs: handles,
    }
}

/// A minimal RTSP client that also collects interleaved packets
pub struct Client {
    pub s: TcpStream,
    pub buf: Vec<u8>,
    pub cseq: u32,
    /// channel → RTP/RTCP packets in arrival order
    pub packets: Vec<(u8, Vec<u8>)>,
    /// When each packet was taken from the socket
    pub arrivals: Vec<std::time::Instant>,
}

#[derive(Debug)]
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Resp {
    pub fn header(&self, k: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    }
}

impl Client {
    pub async fn connect(addr: SocketAddr) -> Self {
        Self {
            s: TcpStream::connect(addr).await.unwrap(),
            buf: vec![],
            cseq: 0,
            packets: vec![],
            arrivals: vec![],
        }
    }

    /// Connect from a chosen source address (any of 127.0.0.0/8 on Linux): another "host"
    pub async fn connect_from(addr: SocketAddr, src: &str) -> Self {
        Self::from_stream(connect_from(addr, src).await)
    }

    pub fn from_stream(s: TcpStream) -> Self {
        Self {
            s,
            buf: vec![],
            cseq: 0,
            packets: vec![],
            arrivals: vec![],
        }
    }

    /// DESCRIBE, SETUP both tracks over TCP, PLAY; returns the session id and the SDP
    pub async fn play(&mut self, base: &str, auth: Option<&str>) -> (String, String) {
        let mut extra: Vec<(&str, &str)> = vec![];
        if let Some(a) = auth {
            extra.push(("Authorization", a));
        }
        let r = self.request("DESCRIBE", base, &extra).await;
        assert_eq!(r.status, 200, "DESCRIBE {r:?}");
        let sdp = r.body.clone();
        let mut session = String::new();
        for (track, ch) in [(0, "0-1"), (1, "2-3")] {
            let t = format!("RTP/AVP/TCP;unicast;interleaved={ch}");
            let mut e = extra.clone();
            e.push(("Transport", &t));
            if !session.is_empty() {
                e.push(("Session", &session));
            }
            let r = self
                .request("SETUP", &format!("{base}/stream={track}"), &e)
                .await;
            assert_eq!(r.status, 200, "SETUP {r:?}");
            session = r
                .header("Session")
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_string();
        }
        let mut e = extra.clone();
        e.push(("Session", &session));
        let r = self.request("PLAY", base, &e).await;
        assert_eq!(r.status, 200, "PLAY {r:?}");
        (session, sdp)
    }

    /// Read until EOF or `d`; true on EOF
    pub async fn until_eof(&mut self, d: Duration) -> bool {
        let end = tokio::time::Instant::now() + d;
        let mut chunk = vec![0u8; 65536];
        loop {
            while self.take_packet() {}
            match tokio::time::timeout_at(end, self.s.read(&mut chunk)).await {
                Ok(Ok(0)) | Ok(Err(_)) => return true,
                Ok(Ok(n)) => self.buf.extend_from_slice(&chunk[..n]),
                Err(_) => return false,
            }
        }
    }

    pub async fn fill(&mut self) -> bool {
        let mut chunk = vec![0u8; 65536];
        match tokio::time::timeout(Duration::from_secs(10), self.s.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => false,
            Ok(Ok(n)) => {
                self.buf.extend_from_slice(&chunk[..n]);
                true
            }
        }
    }

    /// Take one interleaved packet from the front of the buffer, if complete
    pub fn take_packet(&mut self) -> bool {
        if self.buf.first() != Some(&b'$') || self.buf.len() < 4 {
            return false;
        }
        let len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
        if self.buf.len() < 4 + len {
            return false;
        }
        let ch = self.buf[1];
        let p = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        self.packets.push((ch, p));
        self.arrivals.push(std::time::Instant::now());
        true
    }

    /// The next response; interleaved packets before it are collected
    pub async fn response(&mut self) -> Option<Resp> {
        loop {
            while self.take_packet() {}
            if self.buf.first() == Some(&b'$') {
                if !self.fill().await {
                    return None;
                }
                continue;
            }
            if let Some(end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&self.buf[..end]).to_string();
                let mut lines = head.split("\r\n");
                let status = lines.next()?.split(' ').nth(1)?.parse().ok()?;
                let headers: Vec<(String, String)> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .collect();
                let cl: usize = headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .map(|(_, v)| v.parse().unwrap())
                    .unwrap_or(0);
                while self.buf.len() < end + 4 + cl {
                    if !self.fill().await {
                        return None;
                    }
                }
                let body = String::from_utf8_lossy(&self.buf[end + 4..end + 4 + cl]).to_string();
                self.buf.drain(..end + 4 + cl);
                return Some(Resp {
                    status,
                    headers,
                    body,
                });
            }
            if !self.fill().await {
                return None;
            }
        }
    }

    pub async fn request(&mut self, method: &str, uri: &str, extra: &[(&str, &str)]) -> Resp {
        self.cseq += 1;
        let mut r = format!("{method} {uri} RTSP/1.0\r\nCSeq: {}\r\n", self.cseq);
        for (k, v) in extra {
            r.push_str(&format!("{k}: {v}\r\n"));
        }
        r.push_str("\r\n");
        self.s.write_all(r.as_bytes()).await.unwrap();
        let resp = self.response().await.expect("a response");
        assert_eq!(
            resp.header("CSeq"),
            Some(self.cseq.to_string().as_str()),
            "{resp:?}"
        );
        resp
    }

    /// Collect interleaved packets for `d`
    pub async fn collect(&mut self, d: Duration) {
        let end = tokio::time::Instant::now() + d;
        let mut chunk = vec![0u8; 65536];
        loop {
            while self.take_packet() {}
            match tokio::time::timeout_at(end, self.s.read(&mut chunk)).await {
                Ok(Ok(n)) if n > 0 => self.buf.extend_from_slice(&chunk[..n]),
                _ => break,
            }
        }
    }
}

/// A TCP connection from a chosen source address (any of 127.0.0.0/8 on Linux)
pub async fn connect_from(addr: SocketAddr, src: &str) -> TcpStream {
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.bind(SocketAddr::new(src.parse().unwrap(), 0)).unwrap();
    sock.connect(addr).await.unwrap()
}

/// True if the peer closed the connection within `d` (anything it sent is discarded)
pub async fn closed_within(s: &mut TcpStream, d: Duration) -> bool {
    let end = tokio::time::Instant::now() + d;
    let mut chunk = [0u8; 4096];
    loop {
        match tokio::time::timeout_at(end, s.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return true,
            Ok(Ok(_)) => {}
            Err(_) => return false,
        }
    }
}

/// The H.264/H.265 NAL type an RTP video payload starts (FU: the fragmented type)
pub fn video_nal_type(h265: bool, p: &[u8]) -> u8 {
    let pl = &p[12..];
    if h265 {
        let t = (pl[0] >> 1) & 0x3f;
        if t == 49 {
            pl[2] & 0x3f
        } else {
            t
        }
    } else {
        let t = pl[0] & 0x1f;
        if t == 28 {
            pl[1] & 0x1f
        } else {
            t
        }
    }
}

pub fn is_irap_type(h265: bool, t: u8) -> bool {
    if h265 {
        (16..=21).contains(&t) || (32..=34).contains(&t)
    } else {
        t == 5 || t == 7 || t == 8
    }
}

pub fn basic(user: &str, pass: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
    )
}

pub fn rtp_seq(p: &[u8]) -> u16 {
    u16::from_be_bytes([p[2], p[3]])
}

pub fn rtp_ts(p: &[u8]) -> u32 {
    u32::from_be_bytes([p[4], p[5], p[6], p[7]])
}

/// Per-channel checks: RTP v2, PT 96, consecutive seq, non-decreasing timestamps
pub fn check_rtp(packets: &[(u8, Vec<u8>)], channel: u8) -> (usize, Option<u16>) {
    let mut last: Option<(u16, u32)> = None;
    let mut n = 0;
    for (ch, p) in packets.iter().filter(|(c, _)| *c == channel) {
        assert_eq!(*ch, channel);
        assert!(p.len() >= 12);
        assert_eq!(p[0] >> 6, 2, "RTP version");
        assert_eq!(p[1] & 0x7f, 96, "PT");
        let (seq, ts) = (rtp_seq(p), rtp_ts(p));
        if let Some((ls, lt)) = last {
            assert_eq!(
                seq,
                ls.wrapping_add(1),
                "seq continuity on channel {channel}"
            );
            assert!(
                (ts.wrapping_sub(lt) as i32) >= 0,
                "timestamp went back on channel {channel}: {lt} -> {ts}"
            );
        }
        last = Some((seq, ts));
        n += 1;
    }
    (n, last.map(|l| l.0))
}

pub fn rtp_info_seq(info: &str, stream: u32) -> u16 {
    info.split(',')
        .find(|e| e.contains(&format!("stream={stream};")))
        .and_then(|e| e.split(';').find_map(|kv| kv.strip_prefix("seq=")))
        .unwrap_or_else(|| panic!("no seq for stream {stream} in {info}"))
        .parse()
        .unwrap()
}
