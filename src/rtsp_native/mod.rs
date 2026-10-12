//! The native RTSP backend (`rtsp_backend = "native"`, PLAN Phase 4)
//!
//! One `neolink_rtsp` server for every camera; one Hub per camera × stream kind, fed by a
//! [`source::NativeSource`]. Config changes apply live:
//! - users and `rtsp_auth`: the server's auth is swapped;
//! - `permitted_users`: read per request;
//! - cameras added or removed, and any change that shapes a stream (stream kinds,
//!   `pause.on_motion`, `audio`, pacing, bridging, parameter sets, battery): that camera's
//!   Hubs are shut down (sessions get BYE) and recreated;
//! - a reload that adds `certificate` or `tls_client_auth` is rejected and the running
//!   server keeps going (SPEC R7).
//!
//! Server-wide settings (`bind`, `bind_port`, `limits`, `rtp_mtu`) apply at start.

use crate::common::{state_dir, NeoInstance, NeoReactor};
use crate::config::{CameraConfig, Config, UserConfig};
use anyhow::{bail, Context, Result};
use neolink_core::bc_protocol::StreamKind as BcStreamKind;
use neolink_rtsp::hub::desc::DescCache;
use neolink_rtsp::hub::task::{spawn as spawn_hub, HubConfig, HubHandle, LearnGate, ParameterSets};
use neolink_rtsp::rtsp::auth::{Allowed, Auth, Schemes, User};
use neolink_rtsp::rtsp::route::{aliases, MountTarget, Routes, StreamKind};
use neolink_rtsp::rtsp::server::{Provider, Server, ServerConfig};
use neolink_rtsp::source::Availability;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub(crate) mod source;

/// What the server asks neolink for. Each entry carries the generation of the
/// `camera_main` that made it, so a teardown never removes its successor's entries.
/// A camera's mounts, tagged with the generation that set them
type Mounts = (u64, Vec<(String, MountTarget)>);
/// A camera's `permitted_users`, tagged with the generation that set them
type Permitted = (u64, Option<Vec<String>>);

#[derive(Default)]
struct NativeProvider {
    hubs: RwLock<HashMap<MountTarget, (u64, HubHandle)>>,
    /// camera → `permitted_users`
    permitted: RwLock<HashMap<String, Permitted>>,
    have_users: AtomicBool,
    /// camera → its mounts; the route table is their union
    mounts: RwLock<HashMap<String, Mounts>>,
    routes: Routes,
}

impl Provider for NativeProvider {
    fn hub(&self, target: &MountTarget) -> Option<HubHandle> {
        self.hubs
            .read()
            .unwrap()
            .get(target)
            .map(|(_, h)| h.clone())
    }

    fn allowed(&self, camera: &str) -> Allowed {
        let permitted = self.permitted.read().unwrap();
        let list = permitted.get(camera).and_then(|(_, l)| l.clone());
        Allowed::from_config(list.as_deref(), self.have_users.load(Ordering::SeqCst))
    }
}

impl NativeProvider {
    fn set_mounts(&self, camera: &str, gen: u64, mounts: Vec<(String, MountTarget)>) {
        let mut all = self.mounts.write().unwrap();
        if mounts.is_empty() {
            if all.get(camera).is_some_and(|(g, _)| *g == gen) {
                all.remove(camera);
            }
        } else {
            all.insert(camera.to_string(), (gen, mounts));
        }
        self.routes.set(
            all.values()
                .flat_map(|(_, m)| m.iter().cloned())
                .collect::<Vec<_>>(),
        );
    }

    fn set_permitted(&self, camera: &str, gen: u64, list: Option<Vec<String>>) {
        self.permitted
            .write()
            .unwrap()
            .insert(camera.to_string(), (gen, list));
    }

    fn remove_permitted(&self, camera: &str, gen: u64) {
        let mut p = self.permitted.write().unwrap();
        if p.get(camera).is_some_and(|(g, _)| *g == gen) {
            p.remove(camera);
        }
    }

    fn remove_hub(&self, target: &MountTarget, gen: u64) {
        let mut h = self.hubs.write().unwrap();
        if h.get(target).is_some_and(|(g, _)| *g == gen) {
            h.remove(target);
        }
    }
}

static NEXT_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn kind_of(k: BcStreamKind) -> StreamKind {
    match k {
        BcStreamKind::Main => StreamKind::Main,
        BcStreamKind::Sub => StreamKind::Sub,
        BcStreamKind::Extern => StreamKind::Extern,
    }
}

/// SPEC R7: the native backend has no RTSPS yet
fn check_unsupported(config: &Config) -> Result<()> {
    if config.certificate.is_some() {
        bail!(
            "RTSPS is not yet supported by the native backend: remove `certificate` or use \
             rtsp_backend = \"gstreamer\""
        );
    }
    if config.tls_client_auth != "none" {
        bail!(
            "tls_client_auth is not supported by the native backend (no RTSPS yet): remove it \
             or use rtsp_backend = \"gstreamer\""
        );
    }
    Ok(())
}

fn schemes(config: &Config) -> Schemes {
    let digest = config.rtsp_auth.iter().any(|s| s == "digest");
    let basic = config.rtsp_auth.iter().any(|s| s == "basic");
    match (digest, basic) {
        (true, false) => Schemes::Digest,
        (false, true) => Schemes::Basic,
        _ => Schemes::Both,
    }
}

fn auth(config: &Config) -> Auth {
    let users = config
        .users
        .iter()
        .map(|u: &UserConfig| User {
            name: u.name.clone(),
            pass: u.pass.clone(),
        })
        .collect::<Vec<_>>();
    if !users.is_empty() && config.rtsp_auth.iter().any(|s| s == "basic") {
        log::warn!(
            "RTSP Basic auth is enabled without TLS: passwords cross the network in clear \
             text. Use rtsp_auth = [\"digest\"] where every client supports Digest"
        );
    }
    Auth::new(users, schemes(config))
}

fn server_config(config: &Config) -> ServerConfig {
    let l = &config.limits;
    ServerConfig {
        max_connections: l.max_connections,
        max_unauthenticated_per_ip: l.max_unauthenticated_per_ip,
        // Half the connections, whatever the per-address cap: the rest are kept for
        // clients that authenticated or are playing
        max_unauthenticated_total: (l.max_connections / 2).max(1),
        first_request_timeout: Duration::from_secs(l.first_request_timeout_s),
        idle_timeout: Duration::from_secs(l.idle_timeout_s),
        requests_per_second: l.requests_per_second as f64,
        request_burst: l.requests_per_second as f64 * 2.0,
        failed_auth_per_minute: l.failed_auth_per_minute,
        // rtcp_sr is per camera in the config but per server here: off only if every
        // camera turns it off
        rtcp_sr: config.cameras.iter().any(|c| c.rtcp_sr) || config.cameras.is_empty(),
        ..Default::default()
    }
}

/// The settings that shape a camera's Hubs: a change recreates them
#[derive(Clone, PartialEq)]
struct HubShape {
    kinds: Vec<BcStreamKind>,
    on_motion: bool,
    audio: String,
    pacing_latency_ms: Option<u64>,
    parameter_sets: String,
    bridge_fps_milli: u64,
    bridge_max_s: u64,
    use_splash: bool,
    persist_irap: bool,
    sleeps: bool,
}

impl HubShape {
    fn of(c: &CameraConfig) -> Self {
        Self {
            kinds: c.stream.as_stream_kinds(),
            on_motion: c.pause.on_motion,
            audio: c.audio.clone(),
            pacing_latency_ms: c.pacing_latency_ms,
            parameter_sets: c.parameter_sets.clone(),
            bridge_fps_milli: (c.bridge_fps * 1000.0) as u64,
            bridge_max_s: c.bridge_max_s,
            use_splash: c.use_splash,
            persist_irap: c.persist_irap,
            sleeps: c.sleeps(),
        }
    }
}

pub(crate) fn hub_config(c: &CameraConfig, kind: StreamKind, rtp_mtu: u16) -> HubConfig {
    let mut h = HubConfig::new(&format!("{}/{}", c.name, kind.name()));
    if let Some(ms) = c.pacing_latency_ms {
        h.latency_us = ms * 1000;
    }
    h.bridge_fps = c.bridge_fps;
    h.bridge_max_s = c.bridge_max_s;
    h.use_splash = c.use_splash;
    h.rtp_mtu = rtp_mtu as usize;
    h.audio = c.audio == "aac";
    // Waking a sleeping camera is costly: learning starts are rate-limited (SPEC R11)
    h.battery = c.sleeps();
    h.persist_irap = c.persist_irap;
    h.parameter_sets = match c.parameter_sets.as_str() {
        "passthrough" => ParameterSets::Passthrough,
        "inject" => ParameterSets::Inject,
        _ => ParameterSets::Strip,
    };
    h.cache = Some(DescCache::new(&state_dir(), &c.name, kind.name()));
    // The camera connection's own idle_grace_s already covers a quick reconnect: a long
    // linger here would only add to the awake time (SPEC P17)
    h.linger = Duration::from_millis(500);
    h
}

/// The camera's learning-start gate, shared by its streams and kept in the state dir
/// (SPEC P18)
fn learn_gate(c: &CameraConfig) -> Arc<LearnGate> {
    let safe: String = c
        .name
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect();
    Arc::new(LearnGate::standard(Some(
        state_dir().join(format!("learn-{safe}.txt")),
    )))
}

/// Servers whose `main` was dropped and that are still tearing down, for [`shutdown`]
static STOPPING: std::sync::Mutex<Vec<tokio::task::JoinHandle<Result<()>>>> =
    std::sync::Mutex::new(Vec::new());

/// Cancels a server when its `main` ends or is dropped; a server dropped while running is
/// handed to [`shutdown`] to await
struct StopOnDrop {
    cancel: CancellationToken,
    running: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(h) = self.running.take() {
            STOPPING.lock().unwrap().push(h);
        }
    }
}

/// Runs the native backend. The work happens in a task of its own: dropping this future
/// (a signal in `main`) cancels it, and [`shutdown`] waits for its orderly end.
pub(crate) async fn main(reactor: NeoReactor) -> Result<()> {
    let config = reactor.config().await?.borrow().clone();
    check_unsupported(&config)?;
    crate::config::set_native_rtsp_active(true);
    let cancel = CancellationToken::new();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            let r = run(reactor, cancel).await;
            let ok = r.as_ref().map(|_| ()).map_err(|e| format!("{e:#}"));
            let _ = done_tx.send(ok);
            r
        }
    });
    let mut guard = StopOnDrop {
        cancel,
        running: Some(handle),
    };
    let r = done_rx.await;
    guard.running = None;
    match r {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(anyhow::anyhow!(e)),
        Err(_) => Err(anyhow::anyhow!("the RTSP server task ended abnormally")),
    }
}

/// R27: wait (≤ 2 s) for the native servers to end in order after their `main` was
/// dropped: Hubs shut down (sessions get BYE, camera streams stop), connections drain
/// (≤ 1 s). Call before the reactor's shutdown, which logs the cameras out (≤ 5.5 s);
/// `main` holds the two together to 6 s.
pub(crate) async fn shutdown() {
    let handles: Vec<_> = std::mem::take(&mut *STOPPING.lock().unwrap());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    for h in handles {
        if tokio::time::timeout_at(deadline, h).await.is_err() {
            log::warn!("RTSP server (native) did not stop within 2 s");
        }
    }
}

async fn run(reactor: NeoReactor, cancel: CancellationToken) -> Result<()> {
    let config = reactor.config().await?.borrow().clone();
    let provider = Arc::new(NativeProvider::default());
    provider
        .have_users
        .store(!config.users.is_empty(), Ordering::SeqCst);
    let server = Server::new(
        server_config(&config),
        provider.routes.clone(),
        auth(&config),
        provider.clone(),
    );
    let mut set: JoinSet<Result<()>> = JoinSet::new();

    // Cached snapshots for sleeping cameras (SPEC P5). Not fatal: its port may be taken
    // (8555 is also go2rtc's WebRTC default)
    {
        let (reactor, cancel) = (reactor.clone(), cancel.clone());
        set.spawn(async move {
            if let Err(e) = crate::snapshot::serve(reactor, cancel).await {
                log::error!("Snapshot endpoint stopped: {e:?}");
            }
            Ok(())
        });
    }

    // Users and auth schemes
    {
        let (server, provider, cancel) = (server.clone(), provider.clone(), cancel.clone());
        let mut rx = reactor.config().await?;
        set.spawn(async move {
            let key = |c: &Config| (c.users.clone(), c.rtsp_auth.clone());
            let mut current = key(&rx.borrow_and_update());
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    r = rx.changed() => r.context("config watch")?,
                }
                let config = rx.borrow_and_update().clone();
                if key(&config) != current {
                    current = key(&config);
                    provider
                        .have_users
                        .store(!config.users.is_empty(), Ordering::SeqCst);
                    server.set_auth(auth(&config));
                    log::info!("RTSP users updated ({} users)", config.users.len());
                }
            }
        });
    }

    // Cameras added and removed; a camera whose task ended is started again. The cameras
    // configured at start report once their mounts are set (`mounted`), so the server
    // accepts no client before then: a DESCRIBE right after startup must not get 404 for
    // a configured camera
    let (mounted_tx, mounted_rx) = tokio::sync::oneshot::channel::<()>();
    {
        let (provider, cancel, reactor) = (provider.clone(), cancel.clone(), reactor.clone());
        let mut rx = reactor.config().await?;
        set.spawn(async move {
            let mut running: HashMap<String, CancellationToken> = HashMap::new();
            let mut tasks: JoinSet<String> = JoinSet::new();
            let mut mounted_tx = Some(mounted_tx);
            loop {
                // Only the first pass's cameras hold the server back
                let mut starting = vec![];
                let config = rx.borrow_and_update().clone();
                let wanted: HashSet<String> = config
                    .cameras
                    .iter()
                    .filter(|c| c.enabled)
                    .map(|c| c.name.clone())
                    .collect();
                running.retain(|name, token| {
                    let keep = wanted.contains(name);
                    if !keep {
                        log::info!("{name}: RTSP stopping (removed from the config)");
                        token.cancel();
                    }
                    keep
                });
                for name in wanted {
                    if running.contains_key(&name) {
                        continue;
                    }
                    log::info!("{name}: RTSP starting");
                    let token = cancel.child_token();
                    running.insert(name.clone(), token.clone());
                    let (provider, reactor) = (provider.clone(), reactor.clone());
                    let ready = mounted_tx.is_some().then(|| {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        starting.push(rx);
                        tx
                    });
                    tasks.spawn(async move {
                        let ended =
                            camera_main(&name, reactor, provider, token.clone(), ready).await;
                        if let Err(e) = ended {
                            log::error!("{name}: RTSP stopped: {e:#}; restarting in 5 s");
                            tokio::select! {
                                _ = token.cancelled() => {}
                                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                            }
                        }
                        name
                    });
                }
                if let Some(tx) = mounted_tx.take() {
                    // A camera that fails before mounting drops its sender: it counts as done
                    tokio::spawn(async move {
                        for rx in starting {
                            let _ = rx.await;
                        }
                        let _ = tx.send(());
                    });
                }
                tokio::select! {
                    // Shutdown first: a camera task that ends because of it is not restarted
                    biased;
                    _ = cancel.cancelled() => break,
                    r = rx.changed() => r.context("config watch")?,
                    Some(ended) = tasks.join_next(), if !tasks.is_empty() => {
                        if cancel.is_cancelled() {
                            break;
                        }
                        // Ended on its own (error, or its instance went away): forget it,
                        // so the loop starts it again if it is still configured
                        if let Ok(name) = ended {
                            running.remove(&name);
                        }
                    }
                }
            }
            // Shutdown: every camera tears down (Hubs shut down, sessions get BYE)
            for t in running.values() {
                t.cancel();
            }
            while tasks.join_next().await.is_some() {}
            Ok(())
        });
    }

    let addr = format!("{}:{}", config.bind_addr, config.bind_port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("RTSP: could not listen on {addr}"))?;
    log::info!("Starting RTSP Server (native) at {addr}");
    {
        let (server, cancel) = (server.clone(), cancel.clone());
        set.spawn(async move {
            // Bound already (a taken port fails at once); clients that connect meanwhile
            // wait in the backlog. Setting mounts takes milliseconds: the bound only keeps
            // a stuck camera from holding back the others
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                r = tokio::time::timeout(Duration::from_secs(10), mounted_rx) => {
                    if r.is_err() {
                        log::warn!("RTSP: cameras still setting up after 10 s; serving anyway");
                    }
                }
            }
            server.serve(listener, cancel).await;
            Ok(())
        });
    }

    let mut result = Ok(());
    while let Some(r) = set.join_next().await {
        let r = match r {
            Ok(r) => r,
            Err(e) => Err(e.into()),
        };
        if let Err(e) = r {
            cancel.cancel();
            result = Err(e);
        }
    }
    // Every task has ended; let the connections send their BYE and close (≤ 1 s)
    let t = tokio::time::Instant::now();
    while server.connection_count() > 0 && t.elapsed() < Duration::from_secs(1) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    result
}

/// One camera: its Hubs, mounts and permitted users, until removed or cancelled
async fn camera_main(
    name: &str,
    reactor: NeoReactor,
    provider: Arc<NativeProvider>,
    cancel: CancellationToken,
    mut ready: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<()> {
    #[cfg(test)]
    tests::start_delay(name).await;
    let gen = NEXT_GEN.fetch_add(1, Ordering::SeqCst);
    let instance = reactor.get(name).await?;
    let mut config_rx = instance.config().await?;
    let global = reactor.config().await?;
    let supported = supported_streams(instance.clone(), cancel.clone()).await?;
    let result = async {
        loop {
            let config = config_rx.borrow_and_update().clone();
            let shape = HubShape::of(&config);
            provider.set_permitted(name, gen, config.permitted_users.clone());
            let (availability_tx, availability_rx) = watch::channel(Availability {
                supported: Some(true),
                battery: Some(config.sleeps()),
            });
            // rtp_mtu applies when a camera's Hubs are (re)created
            let rtp_mtu = global.borrow().rtp_mtu;
            let gate = learn_gate(&config);
            let mut hubs: Vec<(StreamKind, HubHandle)> = vec![];
            for kind in shape.kinds.iter().copied() {
                let source = Arc::new(source::NativeSource::new(
                    instance.clone(),
                    kind,
                    name.to_string(),
                    shape.on_motion,
                    availability_rx.clone(),
                ));
                let k = kind_of(kind);
                let mut hc = hub_config(&config, k, rtp_mtu);
                hc.learn_gate = Some(gate.clone());
                hubs.push((k, spawn_hub(hc, source)));
            }
            {
                let mut all = provider.hubs.write().unwrap();
                for (k, h) in &hubs {
                    all.insert(
                        MountTarget {
                            camera: name.to_string(),
                            kind: *k,
                        },
                        (gen, h.clone()),
                    );
                }
            }
            // SPEC P18: one wake teaches every stream kind. Whenever the camera connects,
            // each Hub without a desc learns it now, while that is free (a Hub that has
            // one answers at once without starting anything)
            let teach = {
                let mut cam = instance.camera();
                let have = supported.clone();
                let hubs: Vec<(StreamKind, HubHandle)> = hubs.clone();
                let cancel = cancel.child_token();
                let stop = cancel.clone();
                tokio::spawn(async move {
                    loop {
                        if cam.borrow_and_update().upgrade().is_some() {
                            // Only the kinds the camera has (no extern on an Argus)
                            let have = have.borrow().clone();
                            for (k, h) in &hubs {
                                if !have.iter().any(|b| kind_of(*b) == *k) {
                                    continue;
                                }
                                let h = h.clone();
                                tokio::spawn(async move {
                                    let _ = h.describe().await;
                                });
                            }
                        }
                        tokio::select! {
                            _ = cancel.cancelled() => break,
                            r = cam.changed() => if r.is_err() { break },
                        }
                    }
                });
                stop
            };
            let mut supported = supported.clone();
            let shape_changed = loop {
                // Mounts for the configured kinds the camera has (SPEC P16 seeds them)
                let have = supported.borrow_and_update().clone();
                let kinds: Vec<StreamKind> = shape
                    .kinds
                    .iter()
                    .filter(|k| have.contains(k))
                    .map(|k| kind_of(*k))
                    .collect();
                provider.set_mounts(name, gen, aliases(name, &kinds));
                if let Some(tx) = ready.take() {
                    let _ = tx.send(());
                }
                let _ = availability_tx.send_replace(Availability {
                    supported: Some(true),
                    battery: Some(config.sleeps()),
                });
                tokio::select! {
                    _ = cancel.cancelled() => break false,
                    r = supported.changed() => if r.is_err() { break false },
                    r = config_rx.changed() => {
                        if r.is_err() {
                            break false;
                        }
                        let new = config_rx.borrow().clone();
                        provider.set_permitted(name, gen, new.permitted_users.clone());
                        if HubShape::of(&new) != shape {
                            break true;
                        }
                    }
                }
            };
            // Tear this generation down: mounts first, then the Hubs (BYE to sessions)
            teach.cancel();
            provider.set_mounts(name, gen, vec![]);
            for (k, _) in &hubs {
                provider.remove_hub(
                    &MountTarget {
                        camera: name.to_string(),
                        kind: *k,
                    },
                    gen,
                );
            }
            for (_, h) in &hubs {
                h.shutdown().await;
            }
            if !shape_changed {
                break;
            }
            log::info!("{name}: stream settings changed; RTSP sessions restart");
        }
        Result::<()>::Ok(())
    }
    .await;
    provider.remove_permitted(name, gen);
    result
}

/// The camera's stream kinds, from its stream info every 15 s without waking it. Every
/// camera starts from the configured main and sub (SPEC P16): a sleeping one can't be
/// asked, and an always-connected one isn't connected yet at startup (its mounts would
/// answer 404 until then). The camera's own list replaces the seed once it arrives.
async fn supported_streams(
    instance: NeoInstance,
    cancel: CancellationToken,
) -> Result<watch::Receiver<HashSet<BcStreamKind>>> {
    let config = instance.config().await?.borrow().clone();
    let seed: HashSet<BcStreamKind> = {
        let kinds = config.stream.as_stream_kinds();
        if kinds.len() > 1 {
            kinds
                .into_iter()
                .filter(|k| *k != BcStreamKind::Extern)
                .collect()
        } else {
            kinds.into_iter().collect()
        }
    };
    let (tx, rx) = watch::channel(seed);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tx.closed() => break,
                _ = tick.tick() => {}
            }
            let info = tokio::select! {
                _ = cancel.cancelled() => break,
                r = instance.run_passive_task(|cam| {
                    Box::pin(async move { Ok(cam.get_stream_info().await?) })
                }) => r,
            };
            let Ok(info) = info else { continue };
            let kinds: HashSet<BcStreamKind> = info
                .stream_infos
                .iter()
                .flat_map(|s| s.encode_tables.clone())
                .filter_map(|e| match e.name.as_str() {
                    "mainStream" => Some(BcStreamKind::Main),
                    "subStream" => Some(BcStreamKind::Sub),
                    "externStream" => Some(BcStreamKind::Extern),
                    _ => None,
                })
                .collect();
            tx.send_if_modified(|old| {
                let changed = *old != kinds;
                *old = kinds;
                changed
            });
        }
    });
    Ok(rx)
}

#[cfg(test)]
mod tests;
