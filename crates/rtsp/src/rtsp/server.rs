//! The RTSP server: connections, sessions, media writer, RTCP and limits
//! (DESIGN §5.2, §5.3, §6.2, §6.3, §6.6; SPEC R4–R6, R12, R13, R23–R25)

use super::auth::{Allowed, Auth, Decision};
use super::headers::{choose_transport, session_id, transport_reply, TransportError};
use super::msg::{parse, Parsed, Request, Response};
use super::route::{MountTarget, Routes};
use super::sdp::sdp;
use crate::hub::desc::StreamDesc;
use crate::hub::task::{DescribeError, Frame, HubHandle, Subscription};
use crate::hub::wire::Track;
use bytes::{BufMut, Bytes, BytesMut};
use std::collections::{HashMap, VecDeque};
use std::io::IoSlice;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// What the server needs from its embedder (neolink)
pub trait Provider: Send + Sync + 'static {
    /// The Hub serving this mount, if the stream exists
    fn hub(&self, target: &MountTarget) -> Option<HubHandle>;
    /// Who may open this camera's mounts
    fn allowed(&self, camera: &str) -> Allowed;
}

pub struct ServerConfig {
    pub max_connections: usize,
    pub max_unauthenticated_per_ip: usize,
    /// Below max_connections, so unauthenticated clients can't take every slot
    pub max_unauthenticated_total: usize,
    /// With users configured, a connection must authenticate within this, unless playing
    /// an anonymous mount
    pub unauthenticated_lifetime: Duration,
    pub first_request_timeout: Duration,
    pub idle_timeout: Duration,
    pub write_stall_timeout: Duration,
    pub requests_per_second: f64,
    pub request_burst: f64,
    pub failed_auth_per_minute: usize,
    pub session_timeout_s: u32,
    pub play_wait: Duration,
    pub rtcp_sr: bool,
    /// SO_SNDBUF for client sockets; None keeps the kernel's autotuning
    pub send_buffer: Option<usize>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_connections: 128,
            max_unauthenticated_per_ip: 64,
            max_unauthenticated_total: 64,
            unauthenticated_lifetime: Duration::from_secs(30),
            first_request_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(60),
            write_stall_timeout: Duration::from_secs(10),
            requests_per_second: 20.0,
            request_burst: 40.0,
            failed_auth_per_minute: 10,
            session_timeout_s: 60,
            play_wait: Duration::from_secs(2),
            rtcp_sr: true,
            send_buffer: None,
        }
    }
}

pub struct Server {
    pub config: ServerConfig,
    pub routes: Routes,
    /// Swapped whole when users change (`set_auth`)
    auth: std::sync::RwLock<Arc<Auth>>,
    pub provider: Arc<dyn Provider>,
    connections: AtomicUsize,
    /// Keyed by `ip_key`, as is `failed_auth`
    unauth_per_ip: Mutex<HashMap<IpAddr, usize>>,
    unauth_total: AtomicUsize,
    failed_auth: Mutex<HashMap<IpAddr, FailedAuth>>,
    next_session: AtomicU64,
    /// Fixed at creation: NTP = ntp_epoch + wire time (DESIGN §5.3)
    ntp_epoch_us: u64,
}

const PUBLIC: &str =
    "OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN, GET_PARAMETER, SET_PARAMETER";
/// Seconds between 1900 (NTP) and 1970
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

impl Server {
    pub fn new(
        config: ServerConfig,
        routes: Routes,
        auth: Auth,
        provider: Arc<dyn Provider>,
    ) -> Arc<Self> {
        let unix_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        Arc::new(Self {
            config,
            routes,
            auth: std::sync::RwLock::new(Arc::new(auth)),
            provider,
            connections: AtomicUsize::new(0),
            unauth_per_ip: Mutex::new(HashMap::new()),
            unauth_total: AtomicUsize::new(0),
            failed_auth: Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(rand::random::<u32>() as u64),
            ntp_epoch_us: unix_us + NTP_UNIX_OFFSET * 1_000_000,
        })
    }

    /// The current users and schemes
    pub fn auth(&self) -> Arc<Auth> {
        self.auth.read().unwrap().clone()
    }

    /// Replace the users and schemes (hot reload). Outstanding Digest nonces become
    /// invalid; clients answer the next 401 with new credentials.
    pub fn set_auth(&self, auth: Auth) {
        *self.auth.write().unwrap() = Arc::new(auth);
    }

    /// Accept connections until `cancel`
    pub async fn serve(self: Arc<Self>, listener: TcpListener, cancel: CancellationToken) {
        loop {
            let (stream, peer) = tokio::select! {
                _ = cancel.cancelled() => return,
                v = listener.accept() => match v {
                    Ok(v) => v,
                    Err(e) => {
                        log::debug!("accept: {e}");
                        continue;
                    }
                },
            };
            let server = self.clone();
            let cancel = cancel.child_token();
            tokio::spawn(async move { server.connection(stream, peer, cancel).await });
        }
    }

    fn unauth_inc(&self, ip: IpAddr) -> bool {
        let mut m = self.unauth_per_ip.lock().unwrap();
        if self.unauth_total.load(Ordering::SeqCst) >= self.config.max_unauthenticated_total {
            return false;
        }
        let n = m.entry(ip_key(ip)).or_default();
        if *n >= self.config.max_unauthenticated_per_ip {
            return false;
        }
        *n += 1;
        self.unauth_total.fetch_add(1, Ordering::SeqCst);
        true
    }

    fn unauth_dec(&self, ip: IpAddr) {
        let mut m = self.unauth_per_ip.lock().unwrap();
        let key = ip_key(ip);
        if let Some(n) = m.get_mut(&key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&key);
            }
            self.unauth_total.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Failed-auth limiting per IP (per /64 for IPv6), across all its connections: past
    /// `failed_auth_per_minute`, each failure waits for its own 1 s slot, so parallel
    /// connections don't multiply the rate. Returns when this failure may be answered.
    fn note_failed_auth(&self, ip: IpAddr) -> Option<Instant> {
        let mut m = self.failed_auth.lock().unwrap();
        let now = Instant::now();
        let minute = Duration::from_secs(60);
        // Bounded: forget addresses with nothing recent
        if m.len() > 1024 {
            m.retain(|_, f| {
                f.next_slot > now
                    || f.times
                        .back()
                        .is_some_and(|t| now.duration_since(*t) < minute)
            });
        }
        let f = m.entry(ip_key(ip)).or_insert_with(|| FailedAuth {
            times: VecDeque::new(),
            next_slot: now,
        });
        while f
            .times
            .front()
            .is_some_and(|t| now.duration_since(*t) >= minute)
        {
            f.times.pop_front();
        }
        f.times.push_back(now);
        if f.times.len() > self.config.failed_auth_per_minute {
            f.times.pop_front(); // the list never needs more than the threshold
            let slot = f.next_slot.max(now) + Duration::from_secs(1);
            f.next_slot = slot;
            return Some(slot);
        }
        None
    }

    async fn connection(
        self: Arc<Self>,
        mut stream: TcpStream,
        peer: SocketAddr,
        cancel: CancellationToken,
    ) {
        let ip = peer.ip();
        if self.connections.fetch_add(1, Ordering::SeqCst) >= self.config.max_connections
            || !self.unauth_inc(ip)
        {
            let _ = stream.write_all(&Response::new(503).to_bytes(None)).await;
            self.connections.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        set_socket_options(&stream, self.config.send_buffer);
        let local_ip = stream
            .local_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|_| "0.0.0.0".into());
        log::info!("rtsp: connection from {peer}");
        let (mut read, write) = stream.into_split();
        let (ctl_tx, ctl_rx) = mpsc::channel::<WriterCmd>(64);
        let writer_cancel = cancel.child_token();
        // A writer that ends (stall, error) ends the connection
        let writer_done = CancellationToken::new();
        let writer = tokio::spawn({
            let (done, cancel) = (writer_done.clone(), writer_cancel.clone());
            let (stall, sr, epoch) = (
                self.config.write_stall_timeout,
                self.config.rtcp_sr,
                self.ntp_epoch_us,
            );
            async move {
                writer(write, ctl_rx, cancel, stall, sr, epoch).await;
                done.cancel();
            }
        });
        let mut conn = Conn {
            server: self.clone(),
            ip,
            local_ip,
            ctl: ctl_tx,
            session: None,
            authenticated: false,
            tokens: self.config.request_burst,
            last_refill: Instant::now(),
            described: HashMap::new(),
        };
        let mut buf = BytesMut::with_capacity(4096);
        let mut chunk = vec![0u8; 16 * 1024];
        // Only complete requests (or client RTCP) count as activity, and a started request
        // must complete within first_request_timeout: trickled bytes extend nothing
        let connected = Instant::now();
        let mut seen_request = false;
        let mut last_activity = connected;
        let mut partial_since: Option<Instant> = None;
        loop {
            let mut idle_deadline = if !seen_request {
                connected + self.config.first_request_timeout
            } else if conn.playing() {
                Instant::now() + Duration::from_secs(3600)
            } else {
                last_activity + self.config.idle_timeout
            };
            if let Some(t) = partial_since {
                idle_deadline = idle_deadline.min(t + self.config.first_request_timeout);
            }
            // With users configured, authenticate within a bound (playing an anonymous
            // mount is exempt): unauthenticated clients can't hold slots forever
            if !conn.authenticated && !conn.playing() && self.auth().have_users() {
                idle_deadline = idle_deadline.min(connected + self.config.unauthenticated_lifetime);
            }
            let n = tokio::select! {
                _ = cancel.cancelled() => break,
                _ = writer_done.cancelled() => break,
                _ = tokio::time::sleep_until(idle_deadline) => break,
                _ = conn.session_closed() => break,
                r = read.read(&mut chunk) => match r {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                },
            };
            buf.extend_from_slice(&chunk[..n]);
            let mut close = false;
            loop {
                match parse(&mut buf) {
                    Ok(None) => break,
                    Ok(Some(Parsed::Interleaved { channel, .. })) => {
                        // Client RTCP on one of the session's RTCP channels is liveness;
                        // anything else is dropped (R23)
                        if conn.is_rtcp_channel(channel) {
                            last_activity = Instant::now();
                        }
                    }
                    Ok(Some(Parsed::Request(req))) => {
                        seen_request = true;
                        last_activity = Instant::now();
                        if !conn.rate_ok() {
                            let _ = conn.send(Response::new(503), req.cseq()).await;
                            continue;
                        }
                        if !conn.handle(req).await {
                            close = true;
                            break;
                        }
                    }
                    Err(e) => {
                        // The parser does not consume a bad head, and the stream can't be
                        // resynchronised reliably: reply and close
                        let _ = conn.send(Response::new(e.status()), None).await;
                        close = true;
                        break;
                    }
                }
            }
            if close {
                break;
            }
            partial_since = match (buf.is_empty(), partial_since) {
                (true, _) => None,
                (false, None) => Some(Instant::now()),
                (false, p) => p,
            };
        }
        // Close: BYE, shutdown(Write) after a short drain, then drop (DESIGN §6.2)
        conn.teardown().await;
        let _ = conn.ctl.send(WriterCmd::Close).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
        if !conn.authenticated {
            self.unauth_dec(ip);
        }
        self.connections.fetch_sub(1, Ordering::SeqCst);
        log::info!("rtsp: connection from {peer} closed");
    }
}

type Routed = (
    MountTarget,
    super::route::RoutedPath,
    HubHandle,
    Option<String>,
);

struct FailedAuth {
    times: VecDeque<Instant>,
    next_slot: Instant,
}

/// The key for per-address limits: IPv4 (also when IPv4-mapped), or the IPv6 /64, which is
/// what one host or home network usually gets
fn ip_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let s = v6.segments();
                IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
    }
}

struct SessionState {
    id: String,
    num: u64,
    user: Option<String>,
    target: MountTarget,
    hub: HubHandle,
    desc: StreamDesc,
    tracks: HashMap<u32, TrackState>,
    playing: bool,
    closed: CancellationToken,
}

#[derive(Clone, Copy)]
struct TrackState {
    channels: (u8, u8),
    ssrc: u32,
    seq: u16,
    ts_offset: u32,
    clock: u32,
    /// SR counts: cumulative per SSRC, also across PAUSE/PLAY (RFC 3550 §6.4.1)
    packets: u32,
    octets: u32,
}

struct Conn {
    server: Arc<Server>,
    ip: IpAddr,
    local_ip: String,
    ctl: mpsc::Sender<WriterCmd>,
    session: Option<SessionState>,
    authenticated: bool,
    tokens: f64,
    last_refill: Instant,
    /// mount → desc gen at DESCRIBE time
    described: HashMap<String, u32>,
}

enum WriterCmd {
    Bytes(Bytes),
    /// Start media for these tracks (track number → state), with the first frame
    Play {
        sub: Subscription,
        tracks: Vec<(Track, TrackState)>,
        first: Option<Arc<Frame>>,
        done: tokio::sync::oneshot::Sender<()>,
        reply: Bytes,
    },
    /// Stop media, then send the reply (PAUSE ordering, DESIGN §6.3)
    /// `done` returns the tracks' state so a later PLAY continues their sequence numbers
    Pause {
        reply: Bytes,
        done: tokio::sync::oneshot::Sender<Vec<(Track, TrackState)>>,
    },
    /// Stop media without a BYE (the client asked: TEARDOWN)
    Forget,
    Close,
}

impl Conn {
    fn playing(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.playing)
    }

    fn is_rtcp_channel(&self, channel: u8) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.tracks.values().any(|t| t.channels.1 == channel))
    }

    async fn session_closed(&self) {
        match &self.session {
            Some(s) => s.closed.cancelled().await,
            None => std::future::pending().await,
        }
    }

    fn rate_ok(&mut self) -> bool {
        let now = Instant::now();
        let dt = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        let cfg = &self.server.config;
        self.tokens = (self.tokens + dt * cfg.requests_per_second).min(cfg.request_burst);
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }

    async fn send(&self, r: Response, cseq: Option<&str>) -> bool {
        self.ctl
            .send(WriterCmd::Bytes(r.to_bytes(cseq)))
            .await
            .is_ok()
    }

    async fn teardown(&mut self) {
        if let Some(s) = self.session.take() {
            log::info!(
                "rtsp: {} stopped {}/{}",
                self.ip,
                s.target.camera,
                s.target.kind.name()
            );
            s.hub.unsubscribe(s.num).await;
            s.closed.cancel();
        }
    }

    /// Authorize a request for a mount; Err is the response to send
    async fn authorize(&mut self, req: &Request, camera: &str) -> Result<Option<String>, Response> {
        let allowed = self.server.provider.allowed(camera);
        self.authorize_with(req, allowed).await
    }

    async fn authorize_with(
        &mut self,
        req: &Request,
        allowed: Allowed,
    ) -> Result<Option<String>, Response> {
        match self.server.auth().check(
            &allowed,
            &req.method,
            &req.uri,
            req.header("Authorization"),
            self.ip,
        ) {
            Decision::Ok(user) => {
                // Only real credentials lift the per-IP limit; anonymous access doesn't
                if user.is_some() && !self.authenticated {
                    self.authenticated = true;
                    self.server.unauth_dec(self.ip);
                }
                Ok(user)
            }
            Decision::NotFound => Err(Response::new(404)),
            Decision::Challenge(c) => {
                let mut r = Response::new(401);
                for v in c {
                    r = r.header("WWW-Authenticate", v);
                }
                Err(r)
            }
            Decision::Failed(c) => {
                log::info!("rtsp: auth failure from {}", self.ip);
                if let Some(slot) = self.server.note_failed_auth(self.ip) {
                    tokio::time::sleep_until(slot).await;
                }
                let mut r = Response::new(401);
                for v in c {
                    r = r.header("WWW-Authenticate", v);
                }
                Err(r)
            }
        }
    }

    /// Handle one request; false closes the connection
    async fn handle(&mut self, req: Request) -> bool {
        let cseq = req.cseq().map(|s| s.to_string());
        let cseq = cseq.as_deref();
        // R23: every request on a session is re-authorized, as the session's user
        if matches!(
            req.method.as_str(),
            "PLAY" | "PAUSE" | "TEARDOWN" | "GET_PARAMETER" | "SET_PARAMETER"
        ) && self.session.is_some()
        {
            if let Err(r) = self.reauthorize(&req).await {
                return self.send(r, cseq).await;
            }
        }
        let resp = match req.method.as_str() {
            "OPTIONS" => Response::new(200).header("Public", PUBLIC),
            "GET_PARAMETER" | "SET_PARAMETER" => self.keepalive(&req),
            "DESCRIBE" => match self.describe(&req).await {
                Ok(r) | Err(r) => r,
            },
            "SETUP" => match self.setup(&req).await {
                Ok(r) | Err(r) => r,
            },
            "PLAY" => match self.play(&req, cseq).await {
                Ok(None) => return true, // the writer sent the reply
                Ok(Some(r)) | Err(r) => r,
            },
            "PAUSE" => match self.pause(&req, cseq).await {
                Ok(None) => return true,
                Ok(Some(r)) | Err(r) => r,
            },
            "TEARDOWN" => {
                if let Err(r) = self.session_header_ok(&req) {
                    return self.send(r, cseq).await;
                }
                // Like GStreamer: echo the session, then close the connection
                let mut r = Response::new(200);
                if let Some(s) = &self.session {
                    let t = self.server.config.session_timeout_s;
                    r = r.header("Session", format!("{};timeout={t}", s.id));
                }
                self.teardown().await;
                let _ = self.ctl.send(WriterCmd::Forget).await;
                let r = r.header("Connection", "close");
                self.send(r, cseq).await;
                return false;
            }
            "RECORD" | "ANNOUNCE" => Response::new(405).header("Allow", PUBLIC),
            _ => Response::new(501),
        };
        self.send(resp, cseq).await
    }

    fn keepalive(&self, req: &Request) -> Response {
        match self.session_header_ok(req) {
            Ok(()) => Response::new(200),
            Err(r) => r,
        }
    }

    /// PLAY and PAUSE on an existing session must name it
    fn session_required(&self, req: &Request) -> Result<(), Response> {
        if self.session.is_some() && req.header("Session").is_none() {
            return Err(Response::new(454));
        }
        self.session_header_ok(req)
    }

    /// A Session header, if present, must name this connection's session (R23)
    fn session_header_ok(&self, req: &Request) -> Result<(), Response> {
        match (req.header("Session").map(session_id), &self.session) {
            (None, _) => Ok(()),
            (Some(id), Some(s)) if id == s.id => Ok(()),
            _ => Err(Response::new(454)),
        }
    }

    /// The session's camera and user must still authorize this request
    async fn reauthorize(&mut self, req: &Request) -> Result<(), Response> {
        self.session_header_ok(req)?;
        let Some(s) = &self.session else {
            return Ok(());
        };
        let (camera, user) = (s.target.camera.clone(), s.user.clone());
        if self.authorize(req, &camera).await? != user {
            return Err(Response::new(454));
        }
        Ok(())
    }

    /// The mount, its Hub and the authorized user (None: anonymous)
    async fn route(&mut self, req: &Request) -> Result<Routed, Response> {
        let Some((target, path)) = self.server.routes.lookup(&req.uri) else {
            // With users configured, an unauthenticated client must not learn which mounts
            // exist: check the credentials (any user) before answering 404
            if self.server.auth().have_users() {
                self.authorize_with(req, Allowed::Everyone).await?;
            }
            return Err(Response::new(404));
        };
        let user = self.authorize(req, &target.camera).await?;
        let Some(hub) = self.server.provider.hub(&target) else {
            return Err(Response::new(404));
        };
        Ok((target, path, hub, user))
    }

    async fn describe(&mut self, req: &Request) -> Result<Response, Response> {
        let (_, path, hub, _) = self.route(req).await?;
        let desc = match hub.describe().await {
            Ok(d) => d,
            Err(DescribeError::NotReady) => {
                return Err(Response::new(503).header("Retry-After", "10"))
            }
            Err(DescribeError::Gone) => return Err(Response::new(503)),
        };
        self.described.insert(path.mount.clone(), desc.gen);
        let base = content_base(&req.uri);
        let body = sdp(&desc, rand::random::<u32>() as u64, &self.local_ip);
        Ok(Response::new(200)
            .header("Content-Base", base)
            .body("application/sdp", body))
    }

    async fn setup(&mut self, req: &Request) -> Result<Response, Response> {
        self.session_header_ok(req)?;
        let (target, path, hub, user) = self.route(req).await?;
        // The session belongs to the user who set it up (R23)
        if self.session.as_ref().is_some_and(|s| s.user != user) {
            return Err(Response::new(454));
        }
        let track = path.track.unwrap_or(0);
        let transport = match choose_transport(req.header("Transport").unwrap_or("")) {
            Ok(t) => t,
            Err(TransportError::Unsupported) => return Err(Response::new(461)),
            Err(TransportError::Malformed) => return Err(Response::new(400)),
        };
        if self.session.as_ref().is_some_and(|s| s.target != target) {
            return Err(Response::new(459));
        }
        // The writer's tracks are fixed at PLAY
        if self.playing() {
            return Err(Response::new(455));
        }
        let desc = match &self.session {
            Some(s) => s.desc.clone(),
            None => match hub.describe().await {
                Ok(d) => d,
                Err(_) => return Err(Response::new(503).header("Retry-After", "10")),
            },
        };
        // Check the track before creating anything
        let clock = match track {
            0 => 90_000,
            1 => match &desc.audio {
                Some(a) => a.sample_rate,
                None => return Err(Response::new(404)),
            },
            _ => return Err(Response::new(404)),
        };
        if self.session.is_none() {
            let num = self.server.next_session.fetch_add(1, Ordering::Relaxed);
            let id = format!("{:016x}", rand::random::<u64>());
            self.session = Some(SessionState {
                id,
                num,
                user: user.clone(),
                target: target.clone(),
                hub,
                desc,
                tracks: HashMap::new(),
                playing: false,
                closed: CancellationToken::new(),
            });
        }
        let s = self.session.as_mut().unwrap();
        // Unique channel pairs per connection
        let used: Vec<u8> = s
            .tracks
            .values()
            .flat_map(|t| [t.channels.0, t.channels.1])
            .collect();
        let mut channels = transport
            .interleaved
            .unwrap_or((track as u8 * 2, track as u8 * 2 + 1));
        while used.contains(&channels.0) || used.contains(&channels.1) {
            channels = (channels.0.wrapping_add(2), channels.1.wrapping_add(2));
        }
        let ts = TrackState {
            channels,
            ssrc: rand::random(),
            seq: rand::random(),
            ts_offset: rand::random(),
            clock,
            packets: 0,
            octets: 0,
        };
        s.tracks.insert(track, ts);
        Ok(Response::new(200)
            .header("Transport", transport_reply(channels, ts.ssrc))
            .header(
                "Session",
                format!("{};timeout={}", s.id, self.server.config.session_timeout_s),
            ))
    }

    async fn play(
        &mut self,
        req: &Request,
        cseq: Option<&str>,
    ) -> Result<Option<Response>, Response> {
        self.session_required(req)?;
        let wait = self.server.config.play_wait;
        let Some(s) = self.session.as_mut() else {
            return Err(Response::new(455));
        };
        if s.playing {
            return Ok(Some(Response::new(200).header("Session", s.id.clone())));
        }
        if s.tracks.is_empty() {
            return Err(Response::new(455));
        }
        // The stream changed since this session's SETUP (F-cc 29): the session is over; the
        // client starts again from DESCRIBE
        let mount = super::route::normalise(&req.uri).mount;
        let stale = self.described.get(&mount).is_some_and(|g| *g != s.desc.gen);
        let sub = if stale {
            None
        } else {
            match s.hub.subscribe(s.num).await {
                Ok(sub) => Some(sub),
                Err(_) => return Err(Response::new(503).header("Retry-After", "10")),
            }
        };
        let Some(mut sub) = sub.filter(|sub| sub.desc.gen == s.desc.gen) else {
            self.teardown().await;
            return Err(Response::new(454));
        };
        let s = self.session.as_mut().unwrap();
        s.closed = sub.closed.clone();
        s.hub.play(s.num).await;
        // Hold the reply until the first frame (for RTP-Info) or play_wait
        let first = tokio::time::timeout(wait, sub.recv()).await.ok().flatten();
        let base = content_base(&req.uri);
        let mut infos = vec![];
        let mut tracks = vec![];
        let mut ids: Vec<&u32> = s.tracks.keys().collect();
        ids.sort();
        for id in ids {
            let t = s.tracks[id];
            let track = if *id == 0 { Track::Video } else { Track::Audio };
            match &first {
                Some(f) => infos.push(format!(
                    "url={base}stream={id};seq={};rtptime={}",
                    t.seq,
                    rtp_ts(f.wire_us, t)
                )),
                None => infos.push(format!("url={base}stream={id};seq={}", t.seq)),
            }
            tracks.push((track, t));
        }
        let reply = Response::new(200)
            .header("Range", "npt=0-")
            .header(
                "Session",
                format!("{};timeout={}", s.id, self.server.config.session_timeout_s),
            )
            .header("RTP-Info", infos.join(","))
            .to_bytes(cseq);
        let (done, done_rx) = tokio::sync::oneshot::channel();
        if self
            .ctl
            .send(WriterCmd::Play {
                sub,
                tracks,
                first,
                done,
                reply,
            })
            .await
            .is_err()
        {
            return Err(Response::new(500));
        }
        let _ = done_rx.await;
        s.playing = true;
        // SPEC R28: who plays what
        log::info!(
            "rtsp: {} playing {}/{} as {}",
            self.ip,
            s.target.camera,
            s.target.kind.name(),
            s.user.as_deref().unwrap_or("anonymous")
        );
        Ok(None)
    }

    async fn pause(
        &mut self,
        req: &Request,
        cseq: Option<&str>,
    ) -> Result<Option<Response>, Response> {
        self.session_required(req)?;
        let Some(s) = self.session.as_mut() else {
            return Err(Response::new(455));
        };
        s.hub.pause(s.num).await;
        s.playing = false;
        let reply = Response::new(200)
            .header("Session", s.id.clone())
            .to_bytes(cseq);
        let (done, done_rx) = tokio::sync::oneshot::channel();
        if self
            .ctl
            .send(WriterCmd::Pause { reply, done })
            .await
            .is_err()
        {
            return Err(Response::new(500));
        }
        if let Ok(states) = done_rx.await {
            for (track, state) in states {
                let id = if track == Track::Video { 0 } else { 1 };
                s.tracks.insert(id, state);
            }
        }
        Ok(None)
    }
}

/// DESIGN §6.2: no Nagle; dead peers found by keepalive (idle 30 s, every 10 s, 3 probes)
/// and, on Linux, by TCP_USER_TIMEOUT 20 s for unacknowledged data
fn set_socket_options(stream: &TcpStream, send_buffer: Option<usize>) {
    let _ = stream.set_nodelay(true);
    let sock = socket2::SockRef::from(stream);
    let ka = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(30))
        .with_interval(Duration::from_secs(10))
        .with_retries(3);
    if let Err(e) = sock.set_tcp_keepalive(&ka) {
        log::debug!("rtsp: keepalive: {e}");
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Err(e) = sock.set_tcp_user_timeout(Some(Duration::from_secs(20))) {
        log::debug!("rtsp: TCP_USER_TIMEOUT: {e}");
    }
    if let Some(n) = send_buffer {
        let _ = sock.set_send_buffer_size(n);
    }
}

/// `Content-Base`: the request URI without a track suffix, with a trailing slash
fn content_base(uri: &str) -> String {
    let base = uri.split('?').next().unwrap_or(uri);
    let mut base = base.trim_end_matches('/').to_string();
    if let Some(i) = base.rfind('/') {
        let last = &base[i + 1..];
        if last.starts_with("stream=") || last.starts_with("trackID=") {
            base.truncate(i);
        }
    }
    base.push('/');
    base
}

/// `rtp_ts = (wire_us × clock / 1e6 + offset) mod 2^32`, rounded
fn rtp_ts(wire_us: u64, t: TrackState) -> u32 {
    let ticks = (wire_us as u128 * t.clock as u128 + 500_000) / 1_000_000;
    (ticks as u64).wrapping_add(t.ts_offset as u64) as u32
}

struct WriterTrack {
    state: TrackState,
    last_wire: u64,
}

/// The writer: control bytes first, then media in ~64 KiB vectored chunks (DESIGN §6.2)
async fn writer(
    mut w: tokio::net::tcp::OwnedWriteHalf,
    mut ctl: mpsc::Receiver<WriterCmd>,
    cancel: CancellationToken,
    stall: Duration,
    rtcp_sr: bool,
    ntp_epoch_us: u64,
) {
    let mut sub: Option<Subscription> = None;
    let mut tracks: HashMap<Track, WriterTrack> = HashMap::new();
    let mut sr = tokio::time::interval(Duration::from_secs(2));
    let mut last_wire_seen = 0u64;
    loop {
        let item = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            c = ctl.recv() => match c {
                None | Some(WriterCmd::Close) => break,
                Some(c) => Either::Ctl(c),
            },
            f = recv_sub(&mut sub) => match f {
                Some(f) => Either::Frame(f),
                None => {
                    sub = None;
                    continue;
                }
            },
            _ = sr.tick(), if sub.is_some() => Either::Sr,
        };
        let res = match item {
            Either::Ctl(WriterCmd::Bytes(b)) => timed(stall, w.write_all(&b)).await,
            Either::Ctl(WriterCmd::Play {
                sub: s,
                tracks: t,
                first,
                done,
                reply,
            }) => {
                tracks = t
                    .into_iter()
                    .map(|(k, state)| {
                        (
                            k,
                            WriterTrack {
                                state,
                                last_wire: 0,
                            },
                        )
                    })
                    .collect();
                let mut r = timed(stall, w.write_all(&reply)).await;
                if let (Ok(()), Some(f)) = (&r, first) {
                    last_wire_seen = last_wire_seen.max(f.wire_us);
                    r = send_frame(&mut w, &mut tracks, &f, stall).await;
                    if r.is_ok() && rtcp_sr {
                        r = send_sr(&mut w, &tracks, last_wire_seen, ntp_epoch_us, stall).await;
                    }
                }
                sr.reset();
                sub = Some(s);
                let _ = done.send(());
                r
            }
            Either::Ctl(WriterCmd::Pause { reply, done }) => {
                // Nothing is written after the PAUSE reply: drop the media first
                sub = None;
                let r = timed(stall, w.write_all(&reply)).await;
                let _ = done.send(tracks.iter().map(|(k, t)| (*k, t.state)).collect());
                r
            }
            Either::Ctl(WriterCmd::Forget) => {
                sub = None;
                tracks.clear();
                Ok(())
            }
            Either::Ctl(WriterCmd::Close) => break,
            Either::Frame(f) => {
                last_wire_seen = last_wire_seen.max(f.wire_us);
                let first = tracks.get(&f.track).is_some_and(|t| t.state.packets == 0);
                let mut r = send_frame(&mut w, &mut tracks, &f, stall).await;
                // A track's first packets get an SR at once: until one arrives, live555
                // (VLC) can't place them on the shared clock, and a track that starts
                // later (audio after a late joiner's IRAP) would wait for the next tick
                if r.is_ok() && first && rtcp_sr {
                    r = send_sr(&mut w, &tracks, last_wire_seen, ntp_epoch_us, stall).await;
                }
                r
            }
            Either::Sr => {
                if rtcp_sr {
                    send_sr(&mut w, &tracks, last_wire_seen, ntp_epoch_us, stall).await
                } else {
                    send_rr(&mut w, &tracks, stall).await
                }
            }
        };
        if res.is_err() {
            log::info!("rtsp: write failed or stalled; closing");
            break;
        }
    }
    // BYE for each track still set up (the server is ending the session), then
    // shutdown(Write)
    if !tracks.is_empty() {
        let mut bye = BytesMut::new();
        for t in tracks.values() {
            let mut p = BytesMut::new();
            p.put_u8(0x81);
            p.put_u8(203);
            p.put_u16(1);
            p.put_u32(t.state.ssrc);
            interleave(&mut bye, t.state.channels.1, &p);
        }
        let _ = timed(Duration::from_secs(1), w.write_all(&bye)).await;
    }
    let _ = w.shutdown().await;
}

// One short-lived value per writer loop iteration: its size doesn't matter
#[allow(clippy::large_enum_variant)]
enum Either {
    Ctl(WriterCmd),
    Frame(Arc<Frame>),
    Sr,
}

async fn recv_sub(sub: &mut Option<Subscription>) -> Option<Arc<Frame>> {
    match sub {
        Some(s) => s.recv().await,
        None => std::future::pending().await,
    }
}

async fn timed<F: std::future::Future<Output = std::io::Result<()>>>(
    d: Duration,
    f: F,
) -> Result<(), ()> {
    match tokio::time::timeout(d, f).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

fn interleave(out: &mut BytesMut, channel: u8, packet: &[u8]) {
    out.put_u8(b'$');
    out.put_u8(channel);
    out.put_u16(packet.len() as u16);
    out.extend_from_slice(packet);
}

/// One frame's RTP packets, written with vectored IO in chunks of about 64 KiB
async fn send_frame(
    w: &mut tokio::net::tcp::OwnedWriteHalf,
    tracks: &mut HashMap<Track, WriterTrack>,
    f: &Frame,
    stall: Duration,
) -> Result<(), ()> {
    let Some(t) = tracks.get_mut(&f.track) else {
        return Ok(()); // track not set up
    };
    let ts = rtp_ts(f.wire_us, t.state);
    t.last_wire = f.wire_us;
    let mut heads: Vec<[u8; 16]> = Vec::with_capacity(f.rtp.len());
    for c in f.rtp.iter() {
        let len = (12 + c.len()) as u16;
        let mut h = [0u8; 16];
        h[0] = b'$';
        h[1] = t.state.channels.0;
        h[2..4].copy_from_slice(&len.to_be_bytes());
        h[4] = 0x80;
        h[5] = 96 | if c.marker { 0x80 } else { 0 };
        h[6..8].copy_from_slice(&t.state.seq.to_be_bytes());
        h[8..12].copy_from_slice(&ts.to_be_bytes());
        h[12..16].copy_from_slice(&t.state.ssrc.to_be_bytes());
        t.state.seq = t.state.seq.wrapping_add(1);
        t.state.packets = t.state.packets.wrapping_add(1);
        t.state.octets = t.state.octets.wrapping_add(c.len() as u32);
        heads.push(h);
    }
    // Chunks of packets up to ~64 KiB
    let mut i = 0;
    while i < f.rtp.len() {
        let mut slices: Vec<IoSlice> = vec![];
        let mut size = 0;
        let start = i;
        while i < f.rtp.len() && (size < 64 * 1024 || i == start) {
            let c = &f.rtp[i];
            slices.push(IoSlice::new(&heads[i]));
            if c.prefix_len > 0 {
                slices.push(IoSlice::new(c.prefix()));
            }
            slices.push(IoSlice::new(&c.body));
            size += 16 + c.len();
            i += 1;
        }
        timed(stall, write_all_vectored(w, &mut slices)).await?;
    }
    Ok(())
}

async fn write_all_vectored(
    w: &mut tokio::net::tcp::OwnedWriteHalf,
    slices: &mut [IoSlice<'_>],
) -> std::io::Result<()> {
    let mut slices: &mut [IoSlice<'_>] = slices;
    while !slices.is_empty() {
        let n = w.write_vectored(slices).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        IoSlice::advance_slices(&mut slices, n);
    }
    Ok(())
}

/// RTCP SR per track: NTP and RTP from the same wire time (DESIGN §5.3)
async fn send_sr(
    w: &mut tokio::net::tcp::OwnedWriteHalf,
    tracks: &HashMap<Track, WriterTrack>,
    wire_now: u64,
    ntp_epoch_us: u64,
    stall: Duration,
) -> Result<(), ()> {
    let mut out = BytesMut::new();
    // An SR maps a wire time the client has seen media for
    for t in tracks.values().filter(|t| t.state.packets > 0) {
        let ntp_us = ntp_epoch_us + wire_now;
        let secs = ntp_us / 1_000_000;
        let frac = ((ntp_us % 1_000_000) << 32) / 1_000_000;
        let mut p = BytesMut::new();
        p.put_u8(0x80);
        p.put_u8(200);
        p.put_u16(6);
        p.put_u32(t.state.ssrc);
        p.put_u32(secs as u32);
        p.put_u32(frac as u32);
        p.put_u32(rtp_ts(wire_now, t.state));
        p.put_u32(t.state.packets);
        p.put_u32(t.state.octets);
        interleave(&mut out, t.state.channels.1, &p);
    }
    timed(stall, w.write_all(&out)).await
}

/// RR with no report blocks: a keepalive when SRs are off
async fn send_rr(
    w: &mut tokio::net::tcp::OwnedWriteHalf,
    tracks: &HashMap<Track, WriterTrack>,
    stall: Duration,
) -> Result<(), ()> {
    let mut out = BytesMut::new();
    for t in tracks.values() {
        let mut p = BytesMut::new();
        p.put_u8(0x80);
        p.put_u8(201);
        p.put_u16(1);
        p.put_u32(t.state.ssrc);
        interleave(&mut out, t.state.channels.1, &p);
    }
    timed(stall, w.write_all(&out)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_base() {
        assert_eq!(
            content_base("rtsp://h:1/argus/main"),
            "rtsp://h:1/argus/main/"
        );
        assert_eq!(
            content_base("rtsp://h:1/argus/main/"),
            "rtsp://h:1/argus/main/"
        );
        assert_eq!(
            content_base("rtsp://h:1/argus/main/stream=1"),
            "rtsp://h:1/argus/main/"
        );
    }

    #[test]
    fn test_rtp_ts_audio_steps_exactly() {
        let t = TrackState {
            channels: (2, 3),
            ssrc: 1,
            seq: 0,
            ts_offset: u32::MAX - 100,
            clock: 16000,
            packets: 0,
            octets: 0,
        };
        let a = rtp_ts(10_000_000, t);
        let b = rtp_ts(10_064_000, t);
        assert_eq!(b.wrapping_sub(a), 1024);
        let v = TrackState { clock: 90_000, ..t };
        assert_eq!(rtp_ts(1_000_000, v).wrapping_sub(rtp_ts(0, v)), 90_000);
    }
}
