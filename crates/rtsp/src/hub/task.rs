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
use crate::codec::annexb::{is_irap, param_kind, split_nals, NalFilter, VideoCodec};
use crate::rtp::packetize::{self, RtpChunk};
use crate::source::{SourceUnit, StartMode, StartResult, StreamSource};
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
    /// In-band VPS/SPS/PPS handling (SPEC R10)
    pub parameter_sets: ParameterSets,
}

/// In-band parameter sets (SPEC R10, DESIGN §5.1). The SDP always carries them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParameterSets {
    /// Remove in-band copies; clients use the SDP (provisional default, A4b)
    #[default]
    Strip,
    /// Forward the camera's copies unchanged
    Passthrough,
    /// VPS/SPS/PPS from the current desc as separate NALs before every IRAP, replacing
    /// the camera's copies
    Inject,
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
            parameter_sets: ParameterSets::default(),
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
    /// Frames waiting in the pacer (Hub memory)
    pub pacer_queued: usize,
    /// N2: added latency (release lateness + processing) per frame, µs, log2 buckets:
    /// bucket i counts latencies in [2^i, 2^(i+1))
    pub added_latency_hist: [u64; 24],
    /// Hub loop iterations (a spinning loop shows here)
    pub wakeups: u64,
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
    /// Drop video until the first IRAP after a (re)start (DESIGN §4.4)
    need_irap: bool,
    state: SourceState,
    // Source
    source_cancel: Option<CancellationToken>,
    source_rx: Option<mpsc::Receiver<SourceUnit>>,
    source_mode: Option<StartMode>,
    /// A start in progress: it never blocks the Hub (a battery wake can take seconds)
    starting: Option<oneshot::Receiver<(StartMode, CancellationToken, StartResult)>>,
    starting_mode: Option<StartMode>,
    retry_at: Option<Instant>,
    retry_backoff: Duration,
    started_at: Option<Instant>,
    learn_deadline: Option<Instant>,
    last_learn: Option<Instant>,
    linger_until: Option<Instant>,
    // Output
    sessions: HashMap<u64, Session>,
    describe_waiters: Vec<oneshot::Sender<Result<StreamDesc, DescribeError>>>,
    /// The last *released* IRAP: what late joiners and the bridge show
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
/// After the desc is ready, a learning start waits this long for the PLAY
const LEARN_AFTER_READY: Duration = Duration::from_secs(3);
const MIN_BUDGET: usize = 4 * 1024 * 1024;
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

impl Drop for Hub {
    /// Also on a panic: the camera stream and the sessions must not outlive the Hub
    fn drop(&mut self) {
        if let Some(c) = self.source_cancel.take() {
            c.cancel();
        }
        for s in self.sessions.values() {
            s.closed.cancel();
        }
    }
}

impl Hub {
    fn new(cfg: HubConfig, source: Arc<dyn StreamSource>) -> Self {
        let cached = cfg.cache.as_ref().and_then(|c| c.load_desc());
        let cached_irap = match (&cfg.cache, &cached) {
            (Some(c), Some(d)) => c.load_irap(d),
            _ => None,
        };
        let rate = cached
            .as_ref()
            .and_then(|d| d.audio.as_ref())
            .map(|a| a.sample_rate)
            .unwrap_or(16000);
        let mut clock = MediaClock::new(rate, cfg.skew_estimate_us);
        clock.set_sample_rate(rate);
        let au_us = if cfg.audio { clock.au_us() } else { 0 };
        let codec = cached.as_ref().map(|d| d.video.codec.into());
        Self {
            clock,
            learner: DescLearner::new(cfg.audio, cached),
            pacer: Pacer::new(cfg.latency_us),
            wire: WireClock::new(),
            bridge: Bridge::new(cfg.latency_us, cfg.bridge_fps, au_us, cfg.bridge_max_s),
            filter: NalFilter::default(),
            codec,
            restarted: false,
            ever_started: false,
            need_irap: true,
            state: SourceState::Streaming,
            source_cancel: None,
            source_rx: None,
            source_mode: None,
            starting: None,
            starting_mode: None,
            retry_at: None,
            retry_backoff: RETRY_MIN,
            started_at: None,
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

    fn running_mode(&self) -> Option<StartMode> {
        self.source_mode.or(self.starting_mode)
    }

    async fn run(mut self, cmds: &mut mpsc::Receiver<Cmd>) {
        let mut stats_tick = tokio::time::interval(Duration::from_secs(60));
        stats_tick.tick().await;
        loop {
            self.stats.wakeups += 1;
            let wake = self.next_wake();
            tokio::select! {
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => self.command(cmd),
                    None => break,
                },
                started = recv_start(&mut self.starting) => self.started(started),
                unit = recv_opt(&mut self.source_rx) => match unit {
                    Some(unit) => self.ingest(unit),
                    None => self.source_ended(),
                },
                _ = sleep_opt(wake) => {}
                _ = stats_tick.tick() => self.log_stats(),
            }
            self.release();
            self.housekeeping();
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
        // Linger and learning deadlines only act on a running source; one left behind
        // after the source went away must not wake the loop (a past one would spin it)
        let running = self.running_mode().is_some();
        let deadlines = [
            self.linger_until.filter(|_| running),
            self.learn_deadline.filter(|_| running),
            self.retry_at,
        ];
        for e in deadlines.into_iter().flatten() {
            wake = Some(wake.map_or(e, |w| w.min(e)));
        }
        // A pending outage still needs bridge_max checks
        if self.state == SourceState::Retrying && self.playing() > 0 {
            let e = Instant::now() + Duration::from_secs(1);
            wake = Some(wake.map_or(e, |w| w.min(e)));
        }
        wake
    }

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Describe(reply) => {
                if let Some(d) = self.learner.current() {
                    let _ = reply.send(Ok(d.clone()));
                    return;
                }
                // A learning (or any) start already running: wait for it
                let running = self.running_mode().is_some();
                if !running {
                    if !self.learning_allowed() {
                        let _ = reply.send(Err(DescribeError::NotReady));
                        return;
                    }
                    self.start_source(StartMode::Learn);
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
                let old = self.sessions.insert(
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
                if let Some(old) = old {
                    old.closed.cancel();
                }
                let _ = reply.send(Ok(Subscription {
                    desc,
                    rx,
                    queued,
                    closed,
                }));
            }
            Cmd::Play(id) => {
                let now = self.now_us();
                let restamp = match (&self.cached_irap, self.codec, self.sessions.get(&id)) {
                    (Some(nals), Some(codec), Some(s)) if !s.playing => {
                        // Late joiner: the last released IRAP, at the last wire time, plus
                        // EOS where needed; live video then flows from the next IRAP (DESIGN §4.6)
                        let wire = self
                            .wire
                            .last_out_wire()
                            .unwrap_or_else(|| self.wire.virtual_now(now));
                        let mut nals = nals.clone();
                        nals.extend(eos_nal(codec, &nals));
                        Some(self.video_frame(codec, &nals, true, wire))
                    }
                    _ => None,
                };
                let budget = self.budget();
                if let Some(s) = self.sessions.get_mut(&id) {
                    s.playing = true;
                    if let Some(frame) = restamp {
                        let wire = frame.wire_us;
                        if s.guard.admit(Track::Video, wire, true) {
                            s.audio_from = Some(wire);
                            send(s, frame, budget, &mut self.stats);
                            s.guard.after_restamp();
                        }
                    }
                }
                self.linger_until = None;
                match self.running_mode() {
                    // A learning start simply becomes the demand start: no second wake
                    Some(StartMode::Learn) => {
                        if self.source_mode == Some(StartMode::Learn) {
                            self.source_mode = Some(StartMode::Demand);
                        } else {
                            self.starting_mode = Some(StartMode::Demand);
                        }
                        self.learn_deadline = None;
                    }
                    Some(StartMode::Demand) => {}
                    None => self.start_source(StartMode::Demand),
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
                let now = self.now_us();
                let mut st = self.stats.clone();
                st.sessions = self.sessions.len();
                st.playing = self.playing();
                st.source_running = self.source_rx.is_some();
                st.budget = self.budget();
                st.bridged_us = self.bridge.bridged_total(now);
                st.drift_corrections = self.clock.drift_corrections;
                st.guard_engaged = self.wire.stats.guard_engaged;
                st.pacer_underruns = self.pacer.stats.underruns;
                st.pacer_queued = self.pacer.queued();
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
            self.bridge.bridged_total(self.now_us()) / 1_000_000,
            self.stats.learning_starts,
        );
    }

    fn learning_allowed(&self) -> bool {
        !self.cfg.battery
            || self
                .last_learn
                .is_none_or(|t| t.elapsed() >= LEARN_INTERVAL)
    }

    /// Start the source without blocking the Hub; the result arrives in `started`
    fn start_source(&mut self, mode: StartMode) {
        if self.running_mode().is_some() {
            return;
        }
        let cancel = CancellationToken::new();
        if mode == StartMode::Learn {
            self.last_learn = Some(Instant::now());
            self.learn_deadline = Some(Instant::now() + LEARN_MAX);
            self.stats.learning_starts += 1;
        } else {
            self.learn_deadline = None;
        }
        self.retry_at = None;
        let fut = self.source.start(mode, cancel.clone());
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let r = fut.await;
            // The Hub is gone: cancellation is the only way the source is stopped
            if let Err((_, cancel, _)) = tx.send((mode, cancel, r)) {
                cancel.cancel();
            }
        });
        self.starting = Some(rx);
        self.starting_mode = Some(mode);
    }

    fn started(&mut self, started: Option<(StartMode, CancellationToken, StartResult)>) {
        self.starting = None;
        let mode = self.starting_mode.take();
        let Some((_, cancel, result)) = started else {
            // The start itself failed (it panicked): an outage
            self.linger_until = None;
            self.learn_deadline = None;
            self.state = SourceState::Retrying;
            self.schedule_retry();
            return;
        };
        match result {
            Ok(rx) => {
                if self.ever_started {
                    // Not the first start: its timeline is discontinuous with the last
                    self.restarted = true;
                }
                self.ever_started = true;
                self.need_irap = true;
                let mode = mode.unwrap_or(StartMode::Demand);
                log::info!("{}: source started ({mode:?})", self.cfg.name);
                self.source_rx = Some(rx);
                self.source_cancel = Some(cancel);
                self.source_mode = Some(mode);
                self.state = SourceState::Streaming;
                self.started_at = Some(Instant::now());
            }
            Err(e) => {
                cancel.cancel();
                self.linger_until = None;
                self.learn_deadline = None;
                log::warn!("{}: source failed to start: {e}", self.cfg.name);
                self.state = SourceState::Retrying;
                self.schedule_retry();
            }
        }
    }

    /// The source closed its channel: cancel it (the only way it is stopped) and retry
    /// with backoff while there is demand
    fn source_ended(&mut self) {
        log::info!("{}: source ended", self.cfg.name);
        // A source that ran for a while starts the backoff afresh; one that keeps ending
        // quickly backs off further
        if self
            .started_at
            .is_some_and(|t| t.elapsed() >= Duration::from_secs(30))
        {
            self.retry_backoff = RETRY_MIN;
        }
        if let Some(c) = self.source_cancel.take() {
            c.cancel();
        }
        self.source_rx = None;
        self.source_mode = None;
        self.linger_until = None;
        self.learn_deadline = None;
        self.state = SourceState::Retrying;
        self.schedule_retry();
    }

    fn schedule_retry(&mut self) {
        self.retry_at = Some(Instant::now() + self.retry_backoff);
        self.retry_backoff = (self.retry_backoff * 2).min(RETRY_MAX);
    }

    fn stop_source(&mut self) {
        if let Some(c) = self.source_cancel.take() {
            c.cancel();
            log::info!("{}: source stopped", self.cfg.name);
        }
        self.source_rx = None;
        self.source_mode = None;
        // A start still in flight is cancelled when it lands
        if let Some(mut rx) = self.starting.take() {
            tokio::spawn(async move {
                if let Ok((_, cancel, _)) = (&mut rx).await {
                    cancel.cancel();
                }
            });
        }
        self.starting_mode = None;
        self.learn_deadline = None;
        self.retry_at = None;
        // Frames of a stopped source are never released
        self.pacer.clear();
        self.bridge.reset();
    }

    /// After learning (which needs the in-band copies): strip, keep or inject
    fn apply_parameter_sets(&self, codec: VideoCodec, irap: bool, nals: Vec<Bytes>) -> Vec<Bytes> {
        let is_param = |n: &Bytes| param_kind(codec, n).is_some();
        match self.cfg.parameter_sets {
            ParameterSets::Passthrough => nals,
            ParameterSets::Strip => nals.into_iter().filter(|n| !is_param(n)).collect(),
            ParameterSets::Inject => {
                let current = self
                    .learner
                    .current()
                    .filter(|d| VideoCodec::from(d.video.codec) == codec);
                match current {
                    Some(d) if irap => {
                        let p = &d.video.params;
                        let mut out: Vec<Bytes> = vec![];
                        if let Some(vps) = &p.vps {
                            out.push(Bytes::copy_from_slice(vps));
                        }
                        out.push(Bytes::copy_from_slice(&p.sps));
                        out.push(Bytes::copy_from_slice(&p.pps));
                        out.extend(nals.into_iter().filter(|n| !is_param(n)));
                        out
                    }
                    // Not an IRAP: in-band copies elsewhere are dropped; before a desc,
                    // the camera's own copies are all there is
                    Some(_) => nals.into_iter().filter(|n| !is_param(n)).collect(),
                    None => nals,
                }
            }
        }
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
                if self.need_irap && !irap {
                    return; // nothing decodes before the first IRAP of a start
                }
                self.need_irap = false;
                let restarted = std::mem::take(&mut self.restarted);
                let t = self.clock.video(cam_us, restarted, now);
                if restarted || t.discontinuity {
                    self.pacer.restart();
                }
                if let Some(ev) = self.learner.video(codec, &nals, irap, t.media_us, now) {
                    self.desc_event(ev);
                }
                let nals = self.apply_parameter_sets(codec, irap, nals);
                if nals.is_empty() {
                    return;
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
                self.clock.set_sample_rate(header.sample_rate());
                self.bridge.set_au_us(self.clock.au_us());
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
            SourceUnit::Restarted => {
                self.restarted = true;
                self.need_irap = true;
            }
            SourceUnit::State(s) => self.state = s,
        }
        if let Some(ev) = self.learner.check_timeout(now) {
            self.desc_event(ev);
        }
    }

    /// Cache writes happen on the blocking pool, never on the Hub task
    fn save_desc(&self, d: &StreamDesc) {
        if let Some(c) = self.cfg.cache.clone() {
            let d = d.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = c.save_desc(&d) {
                    log::warn!("could not save the stream desc: {e}");
                }
            });
        }
    }

    fn save_irap(&self, nals: &[Bytes]) {
        if let (Some(c), Some(d)) = (self.cfg.cache.clone(), self.learner.current().cloned()) {
            let nals = nals.to_vec();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = c.save_irap(&d, &nals) {
                    log::warn!("could not save the cached IRAP: {e}");
                }
            });
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
                self.save_desc(&d);
                for w in self.describe_waiters.drain(..) {
                    let _ = w.send(Ok(d.clone()));
                }
                // A learning start now only waits a little for the PLAY
                if self.source_mode == Some(StartMode::Learn) {
                    let soon = Instant::now() + LEARN_AFTER_READY;
                    self.learn_deadline = Some(self.learn_deadline.map_or(soon, |d| d.min(soon)));
                }
            }
            DescEvent::Changed(d) => {
                log::info!(
                    "{}: stream changed (gen {}): closing sessions",
                    self.cfg.name,
                    d.gen
                );
                self.save_desc(&d);
                // The cached IRAP belongs to the old stream
                self.cached_irap = None;
                self.irap_policy = IrapWritePolicy::default();
                for s in self.sessions.values() {
                    if s.gen != d.gen {
                        s.closed.cancel();
                    }
                }
                self.sessions.retain(|_, s| !s.closed.is_cancelled());
            }
            DescEvent::LateAudio(d) => self.save_desc(&d),
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
                    if self.bridge.active() {
                        if !item.irap {
                            continue; // live video resumes from an IRAP only
                        }
                        self.bridge.live(now);
                        self.wire.relatch(item.media_us, now);
                    } else if item.discontinuity {
                        self.wire.relatch(item.media_us, now);
                    }
                    self.last_video_wall = Some(now);
                    self.wire.stamp(Track::Video, item.media_us, now)
                }
                Track::Audio => {
                    if self.bridge.active() {
                        continue; // live audio is dropped while bridging
                    }
                    // Right after a resume, live audio can overlap the bridge's silent
                    // audio: drop it rather than send it out of order
                    match self.wire.stamp_if_after(Track::Audio, item.media_us, now) {
                        Some(w) => w,
                        None => continue,
                    }
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
                    if self.irap_policy.should_write(now, false, false) {
                        self.save_irap(&item.data);
                    }
                    self.cached_irap = Some(item.data.clone());
                }
                self.gop_bytes += frame.bytes;
            } else {
                self.audio_sent = true;
            }
            self.fan_out(frame);
        }
        if let Some(per) = (started.elapsed().as_micros() as u64).checked_div(released) {
            // N2: lateness against the due time plus the work done here, per frame
            let late = due.map_or(0, |d| now.saturating_sub(d));
            let work = per;
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
                                nals.extend(eos_nal(codec, &nals));
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
                        BridgeOut::Expired => self.expire(),
                    }
                }
            }
        }
    }

    /// bridge_max ran out during an outage: close the sessions and go Idle
    fn expire(&mut self) {
        log::info!(
            "{}: bridge_max reached during an outage; closing sessions",
            self.cfg.name
        );
        for s in self.sessions.values() {
            s.closed.cancel();
        }
        self.sessions.clear();
        self.stop_source();
        self.state = SourceState::Streaming;
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
            let was_waiting = s.guard.waiting();
            if !s.guard.admit(frame.track, frame.wire_us, frame.irap) {
                continue;
            }
            if was_waiting && frame.track == Track::Video {
                // Audio from this picture on
                s.audio_from = Some(frame.wire_us);
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

    fn housekeeping(&mut self) {
        let now = Instant::now();
        self.describe_waiters.retain(|w| !w.is_closed());
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
        if self.running_mode() == Some(StartMode::Demand) {
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
        // A learning start ends at its deadline unless a PLAY turned it into demand
        if self.running_mode() == Some(StartMode::Learn)
            && self.learn_deadline.is_some_and(|d| now >= d)
            && self.playing() == 0
        {
            self.stop_source();
        }
        // A retry time only matters during an outage with viewers; never leave a past one
        // behind (it would make the loop spin)
        let retrying = self.state == SourceState::Retrying && self.running_mode().is_none();
        if (!retrying || self.playing() == 0) && self.retry_at.is_some_and(|t| now >= t) {
            self.retry_at = None;
        }
        // An outage with nobody watching ends retries; with viewers, bridge_max applies
        // even before any video was released
        if retrying {
            if self.playing() == 0 {
                self.retry_at = None;
            } else if self.last_video_wall.is_none() && self.bridge.retrying(self.now_us()) {
                self.expire();
            } else if self.retry_at.is_some_and(|t| now >= t) {
                self.start_source(StartMode::Demand);
            }
        }
    }
}

/// Send under the byte budget; false when over budget (the frame is not sent)
fn send(s: &mut Session, frame: Arc<Frame>, budget: usize, stats: &mut HubStats) -> bool {
    let bytes = frame.bytes;
    // Count before handing the frame over, so the reader's decrement can't come first
    let queued = s.queued.fetch_add(bytes, Ordering::Relaxed);
    if queued + bytes > budget {
        s.queued.fetch_sub(bytes, Ordering::Relaxed);
        stats.dropped_by_budget += 1;
        return false;
    }
    match s.tx.try_send(frame) {
        Ok(()) => {
            stats.frames_out += 1;
            true
        }
        Err(_) => {
            s.queued.fetch_sub(bytes, Ordering::Relaxed);
            stats.dropped_by_budget += 1;
            false
        }
    }
}

async fn recv_start(
    rx: &mut Option<oneshot::Receiver<(StartMode, CancellationToken, StartResult)>>,
) -> Option<(StartMode, CancellationToken, StartResult)> {
    match rx {
        Some(rx) => rx.await.ok(),
        None => std::future::pending().await,
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
        // Hub memory: the stuck queue is capped by the budget, and the pacer holds only
        // about its latency's worth of frames
        assert!(st.pacer_queued < 200, "{st:?}");
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
        assert!(st.bridged_us > 1_000_000, "{st:?}");
        assert_eq!(st.guard_engaged, 0, "{st:?}");
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

    /// A21: a DESCRIBE every 10 s for 10 min against a cached battery mount starts the
    /// camera 0 times
    #[tokio::test(start_paused = true)]
    async fn test_a21_health_probe_on_cached_mount() {
        let dir = std::env::temp_dir().join(format!(
            "neolink-a21-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cache = || Some(crate::hub::desc::DescCache::new(&dir, "argus", "main"));
        // Learn once and persist the desc
        {
            let source = Arc::new(FixtureSource::new(
                fixture("plain_h265.bcm"),
                1.0,
                Some(true),
            ));
            let mut cfg = HubConfig::new("learn");
            cfg.cache = cache();
            let h = spawn(cfg, source);
            h.describe().await.unwrap();
            // The cache write runs on a blocking thread, outside virtual time
            for _ in 0..200 {
                if cache().unwrap().load_desc().is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
                tokio::task::yield_now().await;
            }
            assert!(cache().unwrap().load_desc().is_some(), "desc persisted");
        }
        let source = Arc::new(FixtureSource::new(
            fixture("plain_h265.bcm"),
            1.0,
            Some(true),
        ));
        let mut cfg = HubConfig::new("probe");
        cfg.cache = cache();
        let h = spawn(cfg, source.clone());
        for _ in 0..60 {
            let d = h.describe().await.unwrap();
            assert_eq!(d.video.codec, crate::hub::desc::VideoCodecSer::H265);
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        assert_eq!(source.starts.lock().unwrap().len(), 0, "camera starts");
        let _ = std::fs::remove_dir_all(&dir);
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
            // From the first live frame: the first is the restamped IRAP of a late join,
            // followed by the next live IRAP up to one GOP later
            let (i, max_jump) = video[1..]
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

    /// A source whose start panics doesn't take the Hub down
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

    fn delay_window(
        mut units: Vec<crate::source::FixtureUnit>,
        from: u64,
        to: u64,
    ) -> Vec<crate::source::FixtureUnit> {
        for u in units.iter_mut() {
            if u.at_us >= from && u.at_us < to {
                u.at_us = to;
            }
        }
        units
    }

    fn hub_with(
        units: Vec<crate::source::FixtureUnit>,
        latency_us: u64,
    ) -> (HubHandle, Arc<FixtureSource>) {
        let source = Arc::new(FixtureSource::new(units, 1.0, Some(false)));
        let mut cfg = HubConfig::new("test");
        cfg.battery = false;
        cfg.latency_us = latency_us;
        (spawn(cfg, source.clone()), source)
    }

    /// Review #1: while bridging, live P-frames are dropped; live resumes on an IRAP
    #[tokio::test(start_paused = true)]
    async fn test_bridge_resumes_on_irap_only() {
        let units = delay_window(fixture("plain_h265.bcm"), 6_300_000, 11_500_000);
        let (h, _s) = hub_with(units, 300_000);
        h.describe().await.unwrap();
        let mut sub = h.subscribe(1).await.unwrap();
        h.play(1).await;
        let frames = drain(&mut sub, 19).await;
        let st = h.stats().await.unwrap();
        assert_eq!(st.guard_engaged, 0, "{st:?}");
        assert!(st.bridged_us > 0, "{st:?}");
        let video: Vec<u64> = frames
            .iter()
            .filter(|f| f.track == Track::Video)
            .map(|f| f.wire_us)
            .collect();
        assert!(
            video.windows(2).all(|w| w[1] - w[0] >= 1_000),
            "packed frames"
        );
    }

    /// Review #2: a late joiner gets the cached IRAP, then nothing but the next live IRAP
    #[tokio::test(start_paused = true)]
    async fn test_late_joiner_starts_clean() {
        let (h, _s) = hub("plain_h265.bcm", false);
        h.describe().await.unwrap();
        let mut a = h.subscribe(1).await.unwrap();
        h.play(1).await;
        drain(&mut a, 3).await;
        let mut b = h.subscribe(2).await.unwrap();
        h.play(2).await;
        let (_fa, fb) = tokio::join!(drain(&mut a, 6), drain(&mut b, 6));
        let kinds: Vec<bool> = fb
            .iter()
            .filter(|f| f.track == Track::Video)
            .map(|f| f.irap)
            .collect();
        assert!(kinds[0], "the late joiner starts on an IRAP");
        assert!(
            kinds[1],
            "then the next live IRAP, not P-frames: {:?}",
            &kinds[..5]
        );
        let first_live = fb
            .iter()
            .filter(|f| f.track == Track::Video)
            .nth(1)
            .unwrap()
            .wire_us;
        assert!(fb
            .iter()
            .filter(|f| f.track == Track::Audio)
            .all(|f| f.wire_us >= first_live));
    }

    /// Review #3: audio timing follows the stream's sample rate
    #[tokio::test(start_paused = true)]
    async fn test_48k_audio_timing() {
        let mut out = vec![];
        for u in fixture("plain_h264.bcm") {
            match &u.unit {
                SourceUnit::Aac { adts } => {
                    let mut a = adts.to_vec();
                    a[2] = (a[2] & !0x3c) | (3 << 2);
                    for k in 0..3u64 {
                        out.push(crate::source::FixtureUnit {
                            at_us: u.at_us + k * 21_333,
                            unit: SourceUnit::Aac {
                                adts: Bytes::from(a.clone()),
                            },
                        });
                    }
                }
                _ => out.push(u),
            }
        }
        out.sort_by_key(|u| u.at_us);
        let (h, _s) = hub_with(out, 1_500_000);
        let d = h.describe().await.unwrap();
        assert_eq!(d.audio.as_ref().unwrap().sample_rate, 48000);
        let mut sub = h.subscribe(1).await.unwrap();
        h.play(1).await;
        let frames = drain(&mut sub, 10).await;
        let mut aw: Vec<u64> = frames
            .iter()
            .filter(|f| f.track == Track::Audio)
            .map(|f| f.wire_us)
            .collect();
        aw.sort_unstable();
        let mut deltas: Vec<u64> = aw.windows(2).map(|w| w[1] - w[0]).collect();
        deltas.sort_unstable();
        let median = deltas[deltas.len() / 2];
        assert!(
            (21_000..=21_700).contains(&median),
            "median audio delta {median} µs"
        );
        let st = h.stats().await.unwrap();
        assert!(st.drift_corrections <= 1, "{st:?}");
    }

    struct Ending(
        Arc<Vec<crate::source::FixtureUnit>>,
        Arc<std::sync::atomic::AtomicU32>,
    );
    impl StreamSource for Ending {
        fn start(
            &self,
            _: StartMode,
            _: CancellationToken,
        ) -> futures::future::BoxFuture<'static, StartResult> {
            self.1.fetch_add(1, Ordering::SeqCst);
            let units = self.0.clone();
            Box::pin(async move {
                let (tx, rx) = mpsc::channel(512);
                tokio::spawn(async move {
                    for u in units.iter().take(60) {
                        let _ = tx.send(u.unit.clone()).await;
                    }
                });
                Ok(rx)
            })
        }
        fn availability(&self) -> tokio::sync::watch::Receiver<crate::source::Availability> {
            tokio::sync::watch::channel(Default::default()).1
        }
    }

    /// Review #4: a source that keeps ending is retried with backoff, not in a storm
    #[tokio::test(start_paused = true)]
    async fn test_retry_backoff() {
        let starts = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let src = Arc::new(Ending(Arc::new(fixture("plain_h264.bcm")), starts.clone()));
        let mut cfg = HubConfig::new("test");
        cfg.battery = false;
        let h = spawn(cfg, src);
        h.describe().await.unwrap();
        let mut sub = h.subscribe(1).await.unwrap();
        h.play(1).await;
        drain(&mut sub, 20).await;
        let n = starts.load(Ordering::SeqCst);
        assert!(n <= 8, "{n} starts in 20 s");
    }

    /// R10: in-band parameter sets per `parameter_sets`
    #[tokio::test(start_paused = true)]
    async fn test_parameter_sets_modes() {
        for mode in [
            ParameterSets::Strip,
            ParameterSets::Passthrough,
            ParameterSets::Inject,
        ] {
            let source = Arc::new(FixtureSource::new(
                fixture("plain_h265.bcm"),
                1.0,
                Some(false),
            ));
            let mut cfg = HubConfig::new("test");
            cfg.battery = false;
            cfg.parameter_sets = mode;
            let h = spawn(cfg, source);
            h.describe().await.unwrap();
            let mut sub = h.subscribe(1).await.unwrap();
            h.play(1).await;
            let frames = drain(&mut sub, 8).await;
            // (IRAPs, IRAPs led by VPS, SPS, PPS as single NALs, param NALs elsewhere)
            let (mut iraps, mut led, mut elsewhere) = (0, 0, 0);
            for f in frames.iter().filter(|f| f.track == Track::Video) {
                let types: Vec<u8> = f
                    .rtp
                    .iter()
                    .filter(|c| c.prefix_len == 0)
                    .map(|c| (c.body[0] >> 1) & 0x3f)
                    .collect();
                let params = types.iter().filter(|t| (32..=34).contains(*t)).count();
                if f.irap {
                    iraps += 1;
                    if types.starts_with(&[32, 33, 34]) && params == 3 {
                        led += 1;
                    }
                } else {
                    elsewhere += params;
                }
                if mode == ParameterSets::Strip {
                    assert_eq!(params, 0, "{mode:?}: in-band parameter set");
                }
            }
            assert!(iraps >= 2, "{mode:?}: {iraps} IRAPs");
            assert_eq!(elsewhere, 0, "{mode:?}");
            match mode {
                ParameterSets::Strip => assert_eq!(led, 0),
                // The fixture, like the Argus, sends them with every IRAP
                ParameterSets::Passthrough | ParameterSets::Inject => {
                    assert_eq!(led, iraps, "{mode:?}")
                }
            }
        }
    }

    /// A spinning Hub never lets paused time advance, so these tests would hang: fail
    /// loudly instead after a real-time limit
    fn spin_watchdog(name: &'static str) -> Arc<std::sync::atomic::AtomicBool> {
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d = done.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(60));
            if !d.load(Ordering::SeqCst) {
                eprintln!("{name}: the Hub loop is spinning (virtual time stalled)");
                std::process::exit(101);
            }
        });
        done
    }

    /// Re-review #1: a failed learning start leaves no deadline behind to spin on
    #[tokio::test(start_paused = true)]
    async fn test_failed_learning_start_does_not_spin() {
        let done = spin_watchdog("test_failed_learning_start_does_not_spin");
        let (h, source) = hub("plain_h264.bcm", true);
        source.fail_starts.store(1, Ordering::SeqCst);
        assert!(h.describe().await.is_err());
        let w0 = h.stats().await.unwrap().wakeups;
        tokio::time::sleep(Duration::from_secs(120)).await;
        let w = h.stats().await.unwrap().wakeups - w0;
        assert!(w < 50, "{w} wakeups in 120 s idle");
        done.store(true, Ordering::SeqCst);
    }

    /// Re-review #2: the source ending during linger leaves no linger deadline behind
    #[tokio::test(start_paused = true)]
    async fn test_source_ending_in_linger_does_not_spin() {
        let done = spin_watchdog("test_source_ending_in_linger_does_not_spin");
        let source = Arc::new(FixtureSource::new(
            fixture("plain_h264.bcm"),
            1.0,
            Some(false),
        ));
        source.end_at_eof.store(true, Ordering::SeqCst);
        let mut cfg = HubConfig::new("test");
        cfg.battery = false;
        // Longer than the fixture, so the source ends while lingering
        cfg.linger = Duration::from_secs(3600);
        let h = spawn(cfg, source.clone());
        h.describe().await.unwrap();
        let mut sub = h.subscribe(1).await.unwrap();
        h.play(1).await;
        drain(&mut sub, 3).await;
        h.pause(1).await;
        tokio::time::sleep(Duration::from_secs(600)).await;
        let w0 = h.stats().await.unwrap().wakeups;
        tokio::time::sleep(Duration::from_secs(120)).await;
        let w = h.stats().await.unwrap().wakeups - w0;
        assert!(w < 50, "{w} wakeups in 120 s idle");
        done.store(true, Ordering::SeqCst);
    }

    /// Review #5, #6: a second DESCRIBE during the learning start waits for it, and the
    /// PLAY continues the same start (one camera wake)
    #[tokio::test(start_paused = true)]
    async fn test_one_start_for_describe_describe_play() {
        let (h, source) = hub("plain_h265.bcm", true);
        let h2 = h.clone();
        let first = tokio::spawn(async move { h2.describe().await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(h.describe().await.is_ok());
        assert!(first.await.unwrap().is_ok());
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut sub = h.subscribe(1).await.unwrap();
        h.play(1).await;
        let frames = drain(&mut sub, 5).await;
        assert!(frames.len() > 50);
        assert_eq!(
            source.starts.lock().unwrap().as_slice(),
            &[StartMode::Learn]
        );
    }

    /// DESIGN §4.4: the congestion fixture at both default latencies keeps wire time clean
    #[tokio::test(start_paused = true)]
    async fn test_congestion_at_both_latencies() {
        for latency in [300_000u64, 1_500_000] {
            let (h, _s) = hub_with(fixture("congestion_h265.bcm"), latency);
            h.describe().await.unwrap();
            let mut sub = h.subscribe(1).await.unwrap();
            h.play(1).await;
            let frames = drain(&mut sub, 110).await;
            assert!(frames.len() > 1000, "{latency}: {}", frames.len());
            let st = h.stats().await.unwrap();
            assert_eq!(st.guard_engaged, 0, "{latency}: {st:?}");
        }
    }
}
