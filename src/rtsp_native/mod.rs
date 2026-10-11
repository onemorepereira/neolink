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
use neolink_rtsp::hub::task::{spawn as spawn_hub, HubConfig, HubHandle, ParameterSets};
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

/// What the server asks neolink for
#[derive(Default)]
struct NativeProvider {
    hubs: RwLock<HashMap<MountTarget, HubHandle>>,
    /// camera → `permitted_users`
    permitted: RwLock<HashMap<String, Option<Vec<String>>>>,
    have_users: AtomicBool,
    /// camera → its mounts; the route table is their union
    mounts: RwLock<HashMap<String, Vec<(String, MountTarget)>>>,
    routes: Routes,
}

impl Provider for NativeProvider {
    fn hub(&self, target: &MountTarget) -> Option<HubHandle> {
        self.hubs.read().unwrap().get(target).cloned()
    }

    fn allowed(&self, camera: &str) -> Allowed {
        let permitted = self.permitted.read().unwrap();
        let list = permitted.get(camera).cloned().flatten();
        Allowed::from_config(list.as_deref(), self.have_users.load(Ordering::SeqCst))
    }
}

impl NativeProvider {
    fn set_mounts(&self, camera: &str, mounts: Vec<(String, MountTarget)>) {
        let mut all = self.mounts.write().unwrap();
        if mounts.is_empty() {
            all.remove(camera);
        } else {
            all.insert(camera.to_string(), mounts);
        }
        self.routes.set(
            all.values()
                .flat_map(|m| m.iter().cloned())
                .collect::<Vec<_>>(),
        );
    }
}

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
        max_unauthenticated_total: l
            .max_unauthenticated_per_ip
            .min(l.max_connections / 2)
            .max(1),
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
    h
}

pub(crate) async fn main(reactor: NeoReactor) -> Result<()> {
    let mut config_rx = reactor.config().await?;
    let config = config_rx.borrow_and_update().clone();
    check_unsupported(&config)?;
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
    let cancel = CancellationToken::new();
    // Dropping this future (shutdown) ends the server: sessions get BYE
    let _guard = cancel.clone().drop_guard();
    let mut set: JoinSet<Result<()>> = JoinSet::new();

    // Cached snapshots for sleeping cameras (SPEC P5)
    {
        let (reactor, cancel) = (reactor.clone(), cancel.clone());
        set.spawn(async move { crate::snapshot::serve(reactor, cancel).await });
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
                if let Err(e) = check_unsupported(&config) {
                    log::error!("Config reload rejected: {e}");
                    continue;
                }
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

    // Cameras added and removed
    {
        let (provider, cancel, reactor) = (provider.clone(), cancel.clone(), reactor.clone());
        let mut rx = reactor.config().await?;
        set.spawn(async move {
            let mut running: HashMap<String, CancellationToken> = HashMap::new();
            let mut tasks: JoinSet<()> = JoinSet::new();
            loop {
                let config = rx.borrow_and_update().clone();
                if check_unsupported(&config).is_ok() {
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
                        let rtp_mtu = config.rtp_mtu;
                        tasks.spawn(async move {
                            if let Err(e) =
                                camera_main(&name, reactor, provider, token, rtp_mtu).await
                            {
                                log::error!("{name}: RTSP stopped: {e:#}");
                            }
                        });
                    }
                }
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    r = rx.changed() => r.context("config watch")?,
                    Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                }
            }
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
            server.serve(listener, cancel).await;
            Ok(())
        });
    }

    while let Some(r) = set.join_next().await {
        match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                cancel.cancel();
                return Err(e);
            }
            Err(e) => {
                cancel.cancel();
                return Err(e.into());
            }
        }
    }
    Ok(())
}

/// One camera: its Hubs, mounts and permitted users, until removed or cancelled
async fn camera_main(
    name: &str,
    reactor: NeoReactor,
    provider: Arc<NativeProvider>,
    cancel: CancellationToken,
    rtp_mtu: u16,
) -> Result<()> {
    let instance = reactor.get(name).await?;
    let mut config_rx = instance.config().await?;
    let supported = supported_streams(instance.clone(), cancel.clone()).await?;
    let result = async {
        loop {
            let config = config_rx.borrow_and_update().clone();
            let shape = HubShape::of(&config);
            provider
                .permitted
                .write()
                .unwrap()
                .insert(name.to_string(), config.permitted_users.clone());
            let (availability_tx, availability_rx) = watch::channel(Availability {
                supported: Some(true),
                battery: Some(config.sleeps()),
            });
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
                hubs.push((k, spawn_hub(hub_config(&config, k, rtp_mtu), source)));
            }
            {
                let mut all = provider.hubs.write().unwrap();
                for (k, h) in &hubs {
                    all.insert(
                        MountTarget {
                            camera: name.to_string(),
                            kind: *k,
                        },
                        h.clone(),
                    );
                }
            }
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
                provider.set_mounts(name, aliases(name, &kinds));
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
                        provider
                            .permitted
                            .write()
                            .unwrap()
                            .insert(name.to_string(), new.permitted_users.clone());
                        if HubShape::of(&new) != shape {
                            break true;
                        }
                    }
                }
            };
            // Tear this generation down: mounts first, then the Hubs (BYE to sessions)
            provider.set_mounts(name, vec![]);
            {
                let mut all = provider.hubs.write().unwrap();
                for (k, _) in &hubs {
                    all.remove(&MountTarget {
                        camera: name.to_string(),
                        kind: *k,
                    });
                }
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
    provider.permitted.write().unwrap().remove(name);
    result
}

/// The camera's stream kinds, from its stream info every 15 s without waking it; a camera
/// that sleeps starts from the configured main and sub (SPEC P16)
async fn supported_streams(
    instance: NeoInstance,
    cancel: CancellationToken,
) -> Result<watch::Receiver<HashSet<BcStreamKind>>> {
    let config = instance.config().await?.borrow().clone();
    let seed: HashSet<BcStreamKind> = if config.sleeps() {
        let kinds = config.stream.as_stream_kinds();
        if kinds.len() > 1 {
            kinds
                .into_iter()
                .filter(|k| *k != BcStreamKind::Extern)
                .collect()
        } else {
            kinds.into_iter().collect()
        }
    } else {
        HashSet::new()
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
