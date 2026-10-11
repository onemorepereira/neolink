//! The Hub task: one per camera × stream kind (DESIGN §4, §4.6–4.8)
//!
//! ```text
//! SourceUnit ─▶ Ingest ─▶ MediaClock ─▶ Desc ─▶ Pacer ─▶ WireClock ─▶ per-session queues
//!                                                          ▲
//!                                                 Bridge ──┘
//! ```
//!
//! A single task owns all state: no locks on the media path. Sessions talk to it through
//! [`HubHandle`]. It never awaits a session: frames go out with `try_send` under a
//! per-session byte budget.

use super::clock::{AudioTime, MediaClock};
use super::desc::{DescCache, DescEvent, DescLearner, IrapWritePolicy, StreamDesc};
use super::pacer::{Paced, Pacer};
use super::wire::{eos_nal, Bridge, BridgeOut, SessionGuard, SourceState, Track, WireClock};
use crate::codec::aac::{adts_access_units, silent_au, AdtsHeader};
use crate::codec::annexb::{is_irap, split_nals, NalFilter, VideoCodec};
use crate::rtp::packetize::{self, RtpChunk};
use crate::source::{SourceUnit, StartMode, StreamSource};
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// A packetized frame, shared by every session
#[derive(Debug)]
pub struct Frame {
    pub track: Track,
    pub irap: bool,
    pub wire_us: u64,
    pub rtp: Arc<[RtpChunk]>,
    pub bytes: usize,
    pub gen: u32,
}

#[derive(Debug, Clone)]
pub struct HubConfig {
    pub name: String,
    pub latency_us: u64,
    pub bridge_fps: f64,
    pub bridge_max_s: u64,
    pub use_splash: bool,
    /// Keep the source running this long after the last session leaves
    pub linger: Duration,
    pub rtp_mtu: usize,
    pub audio: bool,
    /// Battery or unknown: learning starts are rate-limited
    pub battery: bool,
    pub cache: Option<DescCache>,
    pub skew_estimate_us: i64,
    /// How long DESCRIBE waits for a learning start
    pub describe_wait: Duration,
}

impl HubConfig {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            latency_us: 1_500_000,
            bridge_fps: 0.2,
            bridge_max_s: 120,
            use_splash: true,
            linger: Duration::from_secs(4),
            rtp_mtu: 1400,
            audio: true,
            battery: true,
            cache: None,
            skew_estimate_us: 0,
            describe_wait: Duration::from_secs(2),
        }
    }
}

/// Why a DESCRIBE has no answer yet
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescribeError {
    /// No cached desc and no learning start allowed now (answer 503, Retry-After)
    NotReady,
    /// The Hub has stopped
    Gone,
}

/// A session's receiving end
pub struct Subscription {
    pub desc: StreamDesc,
    pub rx: mpsc::Receiver<Arc<Frame>>,
    queued: Arc<AtomicUsize>,
    /// Fires when the Hub closes this session (gen change, bridge_max, shutdown)
    pub closed: CancellationToken,
}

impl Subscription {
    /// The next frame; accounts for the byte budget
    pub async fn recv(&mut self) -> Option<Arc<Frame>> {
        let f = self.rx.recv().await?;
        self.queued.fetch_sub(f.bytes, Ordering::Relaxed);
        Some(f)
    }

    /// Bytes waiting in this session's queue
    pub fn queued_bytes(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }
}

enum Cmd {
    Describe(oneshot::Sender<Result<StreamDesc, DescribeError>>),
    Subscribe(u64, oneshot::Sender<Result<Subscription, DescribeError>>),
    Play(u64),
    Pause(u64),
    Unsubscribe(u64),
    Stats(oneshot::Sender<HubStats>),
}

/// Talk to a Hub
#[derive(Clone)]
pub struct HubHandle {
    tx: mpsc::Sender<Cmd>,
}

impl HubHandle {
    pub async fn describe(&self) -> Result<StreamDesc, DescribeError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Describe(tx))
            .await
            .map_err(|_| DescribeError::Gone)?;
        rx.await.map_err(|_| DescribeError::Gone)?
    }

    pub async fn subscribe(&self, session: u64) -> Result<Subscription, DescribeError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Subscribe(session, tx))
            .await
            .map_err(|_| DescribeError::Gone)?;
        rx.await.map_err(|_| DescribeError::Gone)?
    }

    pub async fn play(&self, session: u64) {
        let _ = self.tx.send(Cmd::Play(session)).await;
    }

    pub async fn pause(&self, session: u64) {
        let _ = self.tx.send(Cmd::Pause(session)).await;
    }

    pub async fn unsubscribe(&self, session: u64) {
        let _ = self.tx.send(Cmd::Unsubscribe(session)).await;
    }

    pub async fn stats(&self) -> Option<HubStats> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Cmd::Stats(tx)).await.ok()?;
        rx.await.ok()
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HubStats {
    pub sessions: usize,
    pub playing: usize,
    pub source_running: bool,
    pub frames_out: u64,
    pub resyncs: u64,
    pub dropped_by_budget: u64,
    pub budget: usize,
    pub learning_starts: u64,
    pub bridged_us: u64,
    pub drift_corrections: u64,
    pub guard_engaged: u64,
    pub pacer_underruns: u64,
    pub max_queued_bytes: usize,
    /// N2: added latency (release lateness + processing) per frame, µs, log2 buckets:
    /// bucket i counts latencies in [2^i, 2^(i+1))
    pub added_latency_hist: [u64; 24],
}

impl HubStats {
    /// Upper bound of the bucket holding the p-th percentile, µs
    pub fn added_latency_pct(&self, p: f64) -> u64 {
        let total: u64 = self.added_latency_hist.iter().sum();
        if total == 0 {
            return 0;
        }
        let want = (total as f64 * p).ceil() as u64;
        let mut acc = 0;
        for (i, n) in self.added_latency_hist.iter().enumerate() {
            acc += n;
            if acc >= want {
                return 1u64 << (i + 1);
            }
        }
        1 << 24
    }
}

struct Session {
    tx: mpsc::Sender<Arc<Frame>>,
    queued: Arc<AtomicUsize>,
    guard: SessionGuard,
    playing: bool,
    gen: u32,
    closed: CancellationToken,
    /// Resync timestamps for R13 (3 in 60 s close the session)
    resyncs: Vec<Instant>,
    /// Audio is held back until this wire time (the IRAP the session restarts on)
    audio_from: Option<u64>,
}

/// A paced item on its way to the wire
struct Item {
    track: Track,
    media_us: u64,
    irap: bool,
    discontinuity: bool,
    /// Video NALs, or one audio AU
    data: Vec<Bytes>,
}

impl Paced for Item {
    fn media_us(&self) -> u64 {
        self.media_us
    }
    fn is_irap(&self) -> bool {
        self.track == Track::Video && self.irap
    }
}

/// Start a supervised Hub for `source`
///
/// A panicking Hub is restarted with backoff (1 s doubling to 60 s); its sessions are
/// closed. After 5 panics in 10 minutes it stops and every request gets `Gone`
/// (DESIGN §4.7).
pub fn spawn(config: HubConfig, source: Arc<dyn StreamSource>) -> HubHandle {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(supervise(config, source, rx));
    HubHandle { tx }
}

async fn supervise(config: HubConfig, source: Arc<dyn StreamSource>, mut rx: mpsc::Receiver<Cmd>) {
    use futures::FutureExt;
    let mut panics: Vec<Instant> = vec![];
    let mut backoff = Duration::from_secs(1);
    loop {
        let hub = Hub::new(config.clone(), source.clone());
        match std::panic::AssertUnwindSafe(hub.run(&mut rx))
            .catch_unwind()
            .await
        {
            Ok(()) => return,
            Err(e) => {
                let what = e
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| e.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                let now = Instant::now();
                panics.retain(|t| now.duration_since(*t) < Duration::from_secs(600));
                panics.push(now);
                if panics.len() >= 5 {
                    log::error!(
                        "{}: hub panicked 5 times in 10 minutes ({what}); giving up on this stream",
                        config.name
                    );
                    return;
                }
                log::error!(
                    "{}: hub panicked ({what}); restarting in {backoff:?}",
                    config.name
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

struct Hub {
    cfg: HubConfig,
    source: Arc<dyn StreamSource>,
    // Pipeline
    clock: MediaClock,
    learner: DescLearner,
    pacer: Pacer<Item>,
    wire: WireClock,
    bridge: Bridge,
    filter: NalFilter,
    codec: Option<VideoCodec>,
    restarted: bool,
    ever_started: bool,
    state: SourceState,
    // Source
    source_cancel: Option<CancellationToken>,
    source_rx: Option<mpsc::Receiver<SourceUnit>>,
    source_mode: Option<StartMode>,
    learn_deadline: Option<Instant>,
    last_learn: Option<Instant>,
    linger_until: Option<Instant>,
    // Output
    sessions: HashMap<u64, Session>,
    describe_waiters: Vec<oneshot::Sender<Result<StreamDesc, DescribeError>>>,
    cached_irap: Option<Vec<Bytes>>,
    irap_policy: IrapWritePolicy,
    last_video_wall: Option<u64>,
    gop_bytes: usize,
    max_gop_bytes: usize,
    audio_sent: bool,
    audio_channels: u8,
    epoch: Instant,
    stats: HubStats,
}

const LEARN_INTERVAL: Duration = Duration::from_secs(600);
const LEARN_MAX: Duration = Duration::from_secs(10);
const MIN_BUDGET: usize = 4 * 1024 * 1024;

impl Hub {
    fn new(cfg: HubConfig, source: Arc<dyn StreamSource>) -> Self {
        let cached = cfg.cache.as_ref().and_then(|c| c.load_desc());
        let cached_irap = match (&cfg.cache, &cached) {
            (Some(c), Some(d)) => c.load_irap(d),
            _ => None,
        };
        let au_us = if cfg.audio { 64_000 } else { 0 };
        Self {
            clock: MediaClock::new(16000, cfg.skew_estimate_us),
            learner: DescLearner::new(cfg.audio, cached),
            pacer: Pacer::new(cfg.latency_us),
            wire: WireClock::new(),
            bridge: Bridge::new(cfg.latency_us, cfg.bridge_fps, au_us, cfg.bridge_max_s),
            filter: NalFilter::default(),
            codec: None,
            restarted: false,
            ever_started: false,
            state: SourceState::Streaming,
            source_cancel: None,
            source_rx: None,
            source_mode: None,
            learn_deadline: None,
            last_learn: None,
            linger_until: None,
            sessions: HashMap::new(),
            describe_waiters: vec![],
            cached_irap,
            irap_policy: IrapWritePolicy::default(),
            last_video_wall: None,
            gop_bytes: 0,
            max_gop_bytes: 0,
            audio_sent: false,
            audio_channels: 1,
            epoch: Instant::now(),
            stats: HubStats::default(),
            cfg,
            source,
        }
    }

    fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }

    fn budget(&self) -> usize {
        MIN_BUDGET.max(2 * self.max_gop_bytes)
    }

    fn playing(&self) -> usize {
        self.sessions.values().filter(|s| s.playing).count()
    }

    async fn run(mut self, cmds: &mut mpsc::Receiver<Cmd>) {
        let mut stats_tick = tokio::time::interval(Duration::from_secs(60));
        stats_tick.tick().await;
        loop {
            let wake = self.next_wake();
            tokio::select! {
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => self.command(cmd).await,
                    None => break,
                },
                unit = recv_opt(&mut self.source_rx) => match unit {
                    Some(unit) => self.ingest(unit),
                    None => {
                        // The source ended: treat as an outage until demand restarts it
                        self.source_rx = None;
                        self.state = SourceState::Retrying;
                    }
                },
                _ = sleep_opt(wake) => {}
                _ = stats_tick.tick() => self.log_stats(),
            }
            self.release();
            self.housekeeping().await;
        }
        self.stop_source();
        for s in self.sessions.values() {
            s.closed.cancel();
        }
        if let (Some(cache), Some(desc), Some(irap)) =
            (&self.cfg.cache, self.learner.current(), &self.cached_irap)
        {
            let _ = cache.save_irap(desc, irap);
        }
    }

    fn next_wake(&self) -> Option<Instant> {
        let mut t: Vec<u64> = vec![];
        if let Some(d) = self.pacer.next_due() {
            t.push(d);
        }
        if self.playing() > 0 && self.cfg.use_splash {
            if let Some(v) = self.last_video_wall {
                t.push(self.bridge.next_wake(v));
            }
        }
        let mut wake = t
            .into_iter()
            .min()
            .map(|us| self.epoch + Duration::from_micros(us));
        for e in [self.linger_until, self.learn_deadline]
            .into_iter()
            .flatten()
        {
            wake = Some(wake.map_or(e, |w| w.min(e)));
        }
        wake
    }

    async fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Describe(reply) => {
                if let Some(d) = self.learner.current() {
                    let _ = reply.send(Ok(d.clone()));
                    return;
                }
                if !self.learning_allowed() {
                    let _ = reply.send(Err(DescribeError::NotReady));
                    return;
                }
                if self.source_rx.is_none() {
                    self.start_source(StartMode::Learn).await;
                }
                let wait = self.cfg.describe_wait;
                let (tx, rx) = oneshot::channel();
                self.describe_waiters.push(tx);
                // Answer NotReady if nothing arrives in time
                tokio::spawn(async move {
                    match tokio::time::timeout(wait, rx).await {
                        Ok(Ok(r)) => {
                            let _ = reply.send(r);
                        }
                        _ => {
                            let _ = reply.send(Err(DescribeError::NotReady));
                        }
                    }
                });
            }
            Cmd::Subscribe(id, reply) => {
                let Some(desc) = self.learner.current().cloned() else {
                    let _ = reply.send(Err(DescribeError::NotReady));
                    return;
                };
                let (tx, rx) = mpsc::channel(4096);
                let queued = Arc::new(AtomicUsize::new(0));
                let closed = CancellationToken::new();
                self.sessions.insert(
                    id,
                    Session {
                        tx,
                        queued: queued.clone(),
                        guard: SessionGuard::default(),
                        playing: false,
                        gen: desc.gen,
                        closed: closed.clone(),
                        resyncs: vec![],
                        audio_from: None,
                    },
                );
                let _ = reply.send(Ok(Subscription {
                    desc,
                    rx,
                    queued,
                    closed,
                }));
            }
            Cmd::Play(id) => {
                let now = self.now_us();
                let late = self.cached_irap.clone();
                let codec = self.codec;
                let mut restamp = None;
                if let Some(s) = self.sessions.get_mut(&id) {
                    s.playing = true;
                    // Late joiner: the cached IRAP restamped at the last wire time, plus EOS
                    if let (Some(nals), Some(codec)) = (late, codec) {
                        let wire = self.wire.virtual_now(now);
                        restamp = Some((wire, nals, codec));
                    }
                }
                if let Some((wire, mut nals, codec)) = restamp {
                    nals.push(eos_nal(codec));
                    let frame = self.video_frame(codec, &nals, true, wire);
                    let budget = self.budget();
                    if let Some(s) = self.sessions.get_mut(&id) {
                        if s.guard.admit(Track::Video, wire, true) {
                            s.audio_from = Some(wire);
                            send(s, frame, budget, &mut self.stats);
                        }
                    }
                }
                self.linger_until = None;
                if self.source_rx.is_none() || self.source_mode == Some(StartMode::Learn) {
                    if self.source_mode == Some(StartMode::Learn) {
                        self.stop_source();
                    }
                    self.start_source(StartMode::Demand).await;
                }
            }
            Cmd::Pause(id) => {
                if let Some(s) = self.sessions.get_mut(&id) {
                    s.playing = false;
                }
            }
            Cmd::Unsubscribe(id) => {
                if let Some(s) = self.sessions.remove(&id) {
                    s.closed.cancel();
                }
            }
            Cmd::Stats(reply) => {
                let mut st = self.stats.clone();
                st.sessions = self.sessions.len();
                st.playing = self.playing();
                st.source_running = self.source_rx.is_some();
                st.budget = self.budget();
                st.bridged_us = self.bridge.bridged_us;
                st.drift_corrections = self.clock.drift_corrections;
                st.guard_engaged = self.wire.stats.guard_engaged;
                st.pacer_underruns = self.pacer.stats.underruns;
                let _ = reply.send(st);
            }
        }
    }

    /// The stats line (DESIGN §4.8), logged at debug every 60 s
    fn log_stats(&self) {
        log::debug!(
            "{}: clients={} playing={} source={} frames_out={} resyncs={} budget_drops={} drift_corrections={} guard_engaged={} pacer_underruns={} bridged={}s learning_starts={}",
            self.cfg.name,
            self.sessions.len(),
            self.playing(),
            self.source_rx.is_some(),
            self.stats.frames_out,
            self.stats.resyncs,
            self.stats.dropped_by_budget,
            self.clock.drift_corrections,
            self.wire.stats.guard_engaged,
            self.pacer.stats.underruns,
            self.bridge.bridged_us / 1_000_000,
            self.stats.learning_starts,
        );
    }

    fn learning_allowed(&self) -> bool {
        !self.cfg.battery
            || self
                .last_learn
                .is_none_or(|t| t.elapsed() >= LEARN_INTERVAL)
    }

    async fn start_source(&mut self, mode: StartMode) {
        let cancel = CancellationToken::new();
        if mode == StartMode::Learn {
            self.last_learn = Some(Instant::now());
            self.learn_deadline = Some(Instant::now() + LEARN_MAX);
            self.stats.learning_starts += 1;
        } else {
            self.learn_deadline = None;
        }
        match self.source.start(mode, cancel.clone()).await {
            Ok(rx) => {
                if self.ever_started {
                    // Not the first start: its timeline is discontinuous with the last
                    self.restarted = true;
                }
                log::info!("{}: source started ({mode:?})", self.cfg.name);
                self.source_rx = Some(rx);
                self.source_cancel = Some(cancel);
                self.source_mode = Some(mode);
                self.state = SourceState::Streaming;
            }
            Err(e) => {
                log::warn!("{}: source failed to start: {e}", self.cfg.name);
                self.state = SourceState::Retrying;
            }
        }
    }

    fn stop_source(&mut self) {
        if let Some(c) = self.source_cancel.take() {
            c.cancel();
            log::info!("{}: source stopped", self.cfg.name);
        }
        self.source_rx = None;
        self.source_mode = None;
        self.learn_deadline = None;
        // Frames of a stopped source are never released
        self.pacer.clear();
        self.ever_started = true;
    }

    fn ingest(&mut self, unit: SourceUnit) {
        let now = self.now_us();
        match unit {
            SourceUnit::Video {
                codec,
                irap,
                cam_us,
                annexb,
            } => {
                self.codec = Some(codec);
                let nals: Vec<Bytes> = split_nals(&annexb)
                    .into_iter()
                    .filter(|n| self.filter.keep(codec, n, &self.cfg.name))
                    .collect();
                if nals.is_empty() {
                    return;
                }
                let irap = irap || nals.iter().any(|n| is_irap(codec, n));
                let restarted = std::mem::take(&mut self.restarted);
                let t = self.clock.video(cam_us, restarted, now);
                if restarted || t.discontinuity {
                    self.pacer.restart();
                }
                if let Some(ev) = self.learner.video(codec, &nals, irap, t.media_us, now) {
                    self.desc_event(ev);
                }
                if irap {
                    self.cached_irap = Some(nals.clone());
                    if let (Some(cache), Some(desc)) = (&self.cfg.cache, self.learner.current()) {
                        if self.irap_policy.should_write(now, false, false) {
                            let _ = cache.save_irap(desc, &nals);
                        }
                    }
                }
                self.pacer.push(
                    now,
                    Item {
                        track: Track::Video,
                        media_us: t.media_us,
                        irap,
                        discontinuity: t.discontinuity,
                        data: nals,
                    },
                );
            }
            SourceUnit::Aac { adts } => {
                if !self.cfg.audio {
                    return;
                }
                let Ok(header) = AdtsHeader::parse(&adts) else {
                    return;
                };
                self.audio_channels = header.channel_config.max(1);
                if let Some(ev) = self.learner.audio(&header, now) {
                    self.desc_event(ev);
                }
                let Ok((_, aus)) = adts_access_units(&adts) else {
                    log::debug!("{}: unusable ADTS frame dropped", self.cfg.name);
                    return;
                };
                for au in aus {
                    if let AudioTime::Send { media_us, .. } = self.clock.audio(now, self.audio_sent)
                    {
                        self.pacer.push(
                            now,
                            Item {
                                track: Track::Audio,
                                media_us,
                                irap: false,
                                discontinuity: false,
                                data: vec![au],
                            },
                        );
                    }
                }
            }
            SourceUnit::Restarted => self.restarted = true,
            SourceUnit::State(s) => self.state = s,
        }
        if let Some(ev) = self.learner.check_timeout(now) {
            self.desc_event(ev);
        }
    }

    fn desc_event(&mut self, ev: DescEvent) {
        match ev {
            DescEvent::Ready(d) => {
                log::info!(
                    "{}: stream ready: {:?} {}x{}",
                    self.cfg.name,
                    d.video.codec,
                    d.video.width,
                    d.video.height
                );
                if let Some(c) = &self.cfg.cache {
                    let _ = c.save_desc(&d);
                }
                for w in self.describe_waiters.drain(..) {
                    let _ = w.send(Ok(d.clone()));
                }
            }
            DescEvent::Changed(d) => {
                log::info!(
                    "{}: stream changed (gen {}): closing sessions",
                    self.cfg.name,
                    d.gen
                );
                if let Some(c) = &self.cfg.cache {
                    let _ = c.save_desc(&d);
                }
                for s in self.sessions.values() {
                    if s.gen != d.gen {
                        s.closed.cancel();
                    }
                }
                self.sessions.retain(|_, s| !s.closed.is_cancelled());
            }
            DescEvent::LateAudio(d) => {
                if let Some(c) = &self.cfg.cache {
                    let _ = c.save_desc(&d);
                }
            }
        }
    }

    fn video_frame(
        &self,
        codec: VideoCodec,
        nals: &[Bytes],
        irap: bool,
        wire_us: u64,
    ) -> Arc<Frame> {
        let rtp: Arc<[RtpChunk]> = packetize::video(codec, nals, self.cfg.rtp_mtu).into();
        let bytes = rtp.iter().map(|c| c.len() + 12).sum();
        Arc::new(Frame {
            track: Track::Video,
            irap,
            wire_us,
            rtp,
            bytes,
            gen: self.learner.current().map(|d| d.gen).unwrap_or(0),
        })
    }

    fn audio_frame(&self, au: &Bytes, wire_us: u64) -> Arc<Frame> {
        let rtp: Arc<[RtpChunk]> = packetize::aac(au, self.cfg.rtp_mtu).into();
        let bytes = rtp.iter().map(|c| c.len() + 12).sum();
        Arc::new(Frame {
            track: Track::Audio,
            irap: false,
            wire_us,
            rtp,
            bytes,
            gen: self.learner.current().map(|d| d.gen).unwrap_or(0),
        })
    }

    fn release(&mut self) {
        let now = self.now_us();
        let due = self.pacer.next_due();
        let started = std::time::Instant::now();
        let mut released = 0u64;
        for item in self.pacer.pop_due(now) {
            released += 1;
            let Some(codec) = self.codec else { continue };
            let wire = match item.track {
                Track::Video => {
                    // Resuming after a bridge, or a MediaClock discontinuity, re-latches
                    if (item.irap && self.bridge.live(now)) || item.discontinuity {
                        self.wire.relatch(item.media_us, now);
                    }
                    self.last_video_wall = Some(now);
                    self.wire.stamp(Track::Video, item.media_us, now)
                }
                Track::Audio => {
                    if self.bridge.active() {
                        continue; // live audio is dropped while bridging
                    }
                    self.wire.stamp(Track::Audio, item.media_us, now)
                }
            };
            let frame = match item.track {
                Track::Video => self.video_frame(codec, &item.data, item.irap, wire),
                Track::Audio => self.audio_frame(&item.data[0], wire),
            };
            if item.track == Track::Video {
                if item.irap {
                    self.max_gop_bytes = self.max_gop_bytes.max(self.gop_bytes);
                    self.gop_bytes = 0;
                }
                self.gop_bytes += frame.bytes;
            } else {
                self.audio_sent = true;
            }
            self.fan_out(frame);
        }
        if released > 0 {
            // N2: lateness against the due time plus the work done here, per frame
            let late = due.map_or(0, |d| now.saturating_sub(d));
            let work = started.elapsed().as_micros() as u64 / released;
            let us = (late + work).max(1);
            let bucket = (63 - us.leading_zeros() as usize).min(23);
            self.stats.added_latency_hist[bucket] += released;
        }
        // Bridge (only with someone watching)
        if self.cfg.use_splash && self.playing() > 0 {
            if let (Some(last), Some(codec)) = (self.last_video_wall, self.codec) {
                for out in self.bridge.tick(now, last, self.state, &mut self.wire) {
                    match out {
                        BridgeOut::Video { wire_us } => {
                            if let Some(mut nals) = self.cached_irap.clone() {
                                nals.push(eos_nal(codec));
                                let f = self.video_frame(codec, &nals, true, wire_us);
                                self.fan_out(f);
                            }
                        }
                        BridgeOut::Audio { wire_us } => {
                            if let Some(au) = silent_au(self.audio_channels) {
                                let f = self.audio_frame(&Bytes::from_static(au), wire_us);
                                self.fan_out(f);
                            }
                        }
                        BridgeOut::Expired => {
                            log::info!(
                                "{}: bridge_max reached during an outage; closing sessions",
                                self.cfg.name
                            );
                            for s in self.sessions.values() {
                                s.closed.cancel();
                            }
                            self.sessions.clear();
                            self.stop_source();
                        }
                    }
                }
            }
        }
    }

    fn fan_out(&mut self, frame: Arc<Frame>) {
        let budget = self.budget();
        let mut close = vec![];
        for (id, s) in self.sessions.iter_mut() {
            if !s.playing {
                continue;
            }
            if frame.track == Track::Audio && s.audio_from.is_some_and(|w| frame.wire_us < w) {
                continue;
            }
            if !s.guard.admit(frame.track, frame.wire_us, frame.irap) {
                continue;
            }
            if !send(s, frame.clone(), budget, &mut self.stats) {
                // Over budget: resync on the next IRAP; 3 in 60 s close the session (R13)
                let now = Instant::now();
                s.resyncs
                    .retain(|t| now.duration_since(*t) < Duration::from_secs(60));
                s.resyncs.push(now);
                s.guard.resync();
                self.stats.resyncs += 1;
                if s.resyncs.len() >= 3 {
                    log::info!("{}: session {id} too slow, closing", self.cfg.name);
                    close.push(*id);
                }
            }
            self.stats.max_queued_bytes = self
                .stats
                .max_queued_bytes
                .max(s.queued.load(Ordering::Relaxed));
        }
        for id in close {
            if let Some(s) = self.sessions.remove(&id) {
                s.closed.cancel();
            }
        }
    }

    async fn housekeeping(&mut self) {
        let now = Instant::now();
        // Sessions whose receivers went away
        let gone: Vec<u64> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.tx.is_closed())
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            self.sessions.remove(&id);
        }
        // Demand: linger, then stop
        if self.source_mode == Some(StartMode::Demand) {
            if self.playing() == 0 {
                match self.linger_until {
                    None => self.linger_until = Some(now + self.cfg.linger),
                    Some(t) if now >= t => {
                        self.linger_until = None;
                        self.stop_source();
                    }
                    _ => {}
                }
            } else {
                self.linger_until = None;
            }
        }
        // A learning start ends at its deadline, or once the desc is ready
        if self.source_mode == Some(StartMode::Learn) {
            let done =
                self.learner.current().is_some() || self.learn_deadline.is_some_and(|d| now >= d);
            if done && self.playing() == 0 {
                self.stop_source();
            }
        }
        // Restart after an outage while there is demand
        if self.source_rx.is_none() && self.playing() > 0 && self.state == SourceState::Retrying {
            self.start_source(StartMode::Demand).await;
        }
    }
}

/// Send under the byte budget; false when over budget (the frame is not sent)
fn send(s: &mut Session, frame: Arc<Frame>, budget: usize, stats: &mut HubStats) -> bool {
    let queued = s.queued.load(Ordering::Relaxed);
    if queued + frame.bytes > budget {
        stats.dropped_by_budget += 1;
        return false;
    }
    let bytes = frame.bytes;
    match s.tx.try_send(frame) {
        Ok(()) => {
            s.queued.fetch_add(bytes, Ordering::Relaxed);
            stats.frames_out += 1;
            true
        }
        Err(_) => {
            stats.dropped_by_budget += 1;
            false
        }
    }
}

async fn recv_opt(rx: &mut Option<mpsc::Receiver<SourceUnit>>) -> Option<SourceUnit> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn sleep_opt(at: Option<Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{parse_bcm, FixtureSource};

    fn fixture(name: &str) -> Vec<crate::source::FixtureUnit> {
        let path = format!("{}/../../fixtures/synth/{name}", env!("CARGO_MANIFEST_DIR"));
        let data = std::fs::read(&path)
            .unwrap_or_else(|_| panic!("{path}: run `cargo xtask mkfixture --standard` first"));
        parse_bcm(&data)
    }

    fn hub(name: &str, battery: bool) -> (HubHandle, Arc<FixtureSource>) {
        let source = Arc::new(FixtureSource::new(fixture(name), 1.0, Some(battery)));
        let mut cfg = HubConfig::new("test");
        cfg.battery = battery;
        (spawn(cfg, source.clone()), source)
    }

    /// Drain a subscription for `secs` of virtual time, checking per-track monotonicity
    async fn drain(sub: &mut Subscription, secs: u64) -> Vec<Arc<Frame>> {
        let mut out: Vec<Arc<Frame>> = vec![];
        let end = Instant::now() + Duration::from_secs(secs);
        let mut last: HashMap<Track, u64> = HashMap::new();
        loop {
            tokio::select! {
                f = sub.recv() => match f {
                    Some(f) => {
                        if let Some(l) = last.get(&f.track) {
                            assert!(f.wire_us > *l, "{:?} went from {} to {}", f.track, l, f.wire_us);
                        }
                        last.insert(f.track, f.wire_us);
                        out.push(f);
                    }
                    None => break,
                },
                _ = tokio::time::sleep_until(end) => break,
            }
        }
        out
    }

    #[tokio::test(start_paused = true)]
    async fn test_describe_learns_then_play_streams() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (h, source) = hub("plain_h265.bcm", true);
        let desc = h.describe().await.unwrap();
        assert_eq!((desc.video.width, desc.video.height), (640, 360));
        assert!(desc.audio.is_some());
        assert_eq!(
            source.starts.lock().unwrap().as_slice(),
            &[StartMode::Learn]
        );
        let mut sub = h.subscribe(1).await.unwrap();
        h.play(1).await;
        let frames = drain(&mut sub, 10).await;
        let video = frames.iter().filter(|f| f.track == Track::Video).count();
        let audio = frames.iter().filter(|f| f.track == Track::Audio).count();
        assert!(video > 100, "{video} video frames in 10 s");
        assert!(audio > 100, "{audio} audio frames in 10 s");
        // The first video frame a session gets is an IRAP
        assert!(
            frames
                .iter()
                .find(|f| f.track == Track::Video)
                .unwrap()
                .irap
        );
        let st = h.stats().await.unwrap();
        assert_eq!(st.guard_engaged, 0, "{st:?}");
        // N2 (virtual time: only processing counts): p99 well under 50 ms
        assert!(st.added_latency_pct(0.99) <= 50_000, "{st:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn test_linger_then_stop() {
        let (h, source) = hub("plain_h264.bcm", false);
        h.describe().await.unwrap();
        let mut sub = h.subscribe(1).await.unwrap();
        h.play(1).await;
        drain(&mut sub, 3).await;
        h.unsubscribe(1).await;
        drop(sub);
        tokio::time::sleep(Duration::from_secs(6)).await;
        // nudge the Hub so it runs housekeeping
        let st = h.stats().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let st2 = h.stats().await.unwrap();
        assert!(!st2.source_running, "{st:?} {st2:?}");
        assert_eq!(source.running.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn test_stuck_session_is_bounded() {
        let (h, _source) = hub("plain_h265.bcm", false);
        h.describe().await.unwrap();
        let mut healthy = vec![];
        for id in 1..=3 {
            let s = h.subscribe(id).await.unwrap();
            h.play(id).await;
            healthy.push(s);
        }
        let stuck = h.subscribe(9).await.unwrap();
        h.play(9).await;
        let mut counts = vec![];
        let (a, b, c) = {
            let mut it = healthy.iter_mut();
            (it.next().unwrap(), it.next().unwrap(), it.next().unwrap())
        };
        let (fa, fb, fc) = tokio::join!(drain(a, 15), drain(b, 15), drain(c, 15));
        counts.extend([fa.len(), fb.len(), fc.len()]);
        let st = h.stats().await.unwrap();
        assert!(
            stuck.queued_bytes() <= st.budget + st.budget / 10,
            "stuck queue {} > budget {}",
            stuck.queued_bytes(),
            st.budget
        );
        assert!(counts.iter().all(|&n| n > 200), "{counts:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn test_bridge_fills_an_outage() {
        let (h, _source) = hub("outage_h265.bcm", true);
        h.describe().await.unwrap();
        let mut sub = h.subscribe(1).await.unwrap();
        h.play(1).await;
        let frames = drain(&mut sub, 40).await;
        // During the 10 s outage the session keeps getting frames: bridged IRAPs ~5 s apart
        // and silent audio, never a gap longer than the bridge threshold plus one interval
        let mut wall: Vec<u64> = frames.iter().map(|f| f.wire_us).collect();
        wall.sort_unstable();
        let max_gap = wall.windows(2).map(|w| w[1] - w[0]).max().unwrap();
        assert!(max_gap < 5_000_000, "max wire gap {max_gap} µs");
        let st = h.stats().await.unwrap();
        assert!(
            st.bridged_us > 0 || frames.iter().any(|f| f.bytes < 100),
            "{st:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_learning_start_is_rate_limited_on_battery() {
        let source = Arc::new(FixtureSource::new(vec![], 1.0, Some(true)));
        let mut cfg = HubConfig::new("test");
        cfg.battery = true;
        let h = spawn(cfg, source.clone());
        assert_eq!(h.describe().await, Err(DescribeError::NotReady));
        assert_eq!(h.describe().await, Err(DescribeError::NotReady));
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(h.describe().await, Err(DescribeError::NotReady));
        assert_eq!(
            source.starts.lock().unwrap().len(),
            1,
            "one learning start per 10 min"
        );
    }

    /// A17 (clock part): the u32 wrap and a counter reset give a monotonic wire clock
    /// with no jump over 1 s between consecutive video frames
    #[tokio::test(start_paused = true)]
    async fn test_a17_wrap_and_reset() {
        // Drain for the fixture's length only: after it ends the bridge takes over
        for (name, secs) in [("wrap_h265.bcm", 30), ("reset_h264.bcm", 20)] {
            let (h, _source) = hub(name, false);
            h.describe().await.unwrap();
            let mut sub = h.subscribe(1).await.unwrap();
            h.play(1).await;
            let frames = drain(&mut sub, secs).await;
            let video: Vec<u64> = frames
                .iter()
                .filter(|f| f.track == Track::Video)
                .map(|f| f.wire_us)
                .collect();
            assert!(video.len() > 200, "{name}: {} video frames", video.len());
            let (i, max_jump) = video
                .windows(2)
                .map(|w| w[1] - w[0])
                .enumerate()
                .max_by_key(|(_, j)| *j)
                .unwrap();
            assert!(
                max_jump <= 1_000_000,
                "{name}: wire jump {max_jump} µs after video frame {i} of {}",
                video.len()
            );
        }
    }

    struct PanicSource(std::sync::atomic::AtomicU32);
    impl crate::source::StreamSource for PanicSource {
        fn start(
            &self,
            _: StartMode,
            _: CancellationToken,
        ) -> futures::future::BoxFuture<'static, crate::source::StartResult> {
            let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                if n < 2 {
                    panic!("boom {n}");
                }
                Err("no camera".to_string())
            })
        }
        fn availability(&self) -> tokio::sync::watch::Receiver<crate::source::Availability> {
            tokio::sync::watch::channel(Default::default()).1
        }
    }

    /// A Hub that panics is restarted and keeps answering
    #[tokio::test(start_paused = true)]
    async fn test_supervisor_restarts_after_panic() {
        let source = Arc::new(PanicSource(Default::default()));
        let mut cfg = HubConfig::new("panic");
        cfg.battery = false;
        let h = spawn(cfg, source.clone());
        for _ in 0..3 {
            let r = h.describe().await;
            assert!(
                matches!(r, Err(DescribeError::NotReady) | Err(DescribeError::Gone)),
                "{r:?}"
            );
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        // After two panics the third start runs normally; the Hub is alive
        assert!(h.stats().await.is_some());
        assert!(source.0.load(std::sync::atomic::Ordering::SeqCst) >= 3);
    }
}
