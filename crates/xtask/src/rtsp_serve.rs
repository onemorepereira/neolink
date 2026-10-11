//! `xtask rtsp-serve`: the native RTSP server on fixture sources, for client tests and the
//! golden diff before neolink is integrated (PLAN 3.4, 3.9, 3.11)

use anyhow::{Context, Result};
use clap::Parser;
use neolink_rtsp::hub::task::{spawn, HubConfig, HubHandle};
use neolink_rtsp::rtsp::auth::{Allowed, Auth, Schemes, User};
use neolink_rtsp::rtsp::route::{aliases, MountTarget, Routes, StreamKind};
use neolink_rtsp::rtsp::server::{Provider, Server, ServerConfig};
use neolink_rtsp::source::{parse_bcm, FixtureSource};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
pub struct Opts {
    #[arg(long, default_value = "127.0.0.1:8554")]
    bind: String,
    /// Camera name (the mount prefix)
    #[arg(long, default_value = "argus")]
    camera: String,
    /// Fixture for the main stream
    #[arg(long)]
    main: Option<PathBuf>,
    /// Fixture for the sub stream
    #[arg(long)]
    sub: Option<PathBuf>,
    /// Fixture for the extern stream
    #[arg(long = "extern")]
    extern_: Option<PathBuf>,
    /// Users as name:pass (repeatable)
    #[arg(long = "user")]
    users: Vec<String>,
    /// The camera's permitted_users (repeatable); unset: everyone (or anonymous)
    #[arg(long = "permit")]
    permitted: Vec<String>,
    /// both, digest or basic
    #[arg(long, default_value = "both")]
    auth: String,
    /// Replay the fixtures forever with a continuous camera clock
    #[arg(long)]
    r#loop: bool,
    /// Treat the camera as battery powered (learning starts rate-limited)
    #[arg(long)]
    battery: bool,
    /// Stall the feeds this many seconds after each start …
    #[arg(long)]
    stall_at: Option<f64>,
    /// … for this many seconds (a camera outage)
    #[arg(long, default_value_t = 60.0)]
    stall_for: f64,
    /// Linger after the last client, seconds
    #[arg(long, default_value_t = 5.0)]
    linger: f64,
    /// Write per-stream counters here (JSON) once a second
    #[arg(long)]
    stats: Option<PathBuf>,
    /// Create this file once listening
    #[arg(long)]
    ready_file: Option<PathBuf>,
}

struct FixtureProvider {
    hubs: HashMap<MountTarget, HubHandle>,
    allowed: Allowed,
}

impl Provider for FixtureProvider {
    fn hub(&self, target: &MountTarget) -> Option<HubHandle> {
        self.hubs.get(target).cloned()
    }
    fn allowed(&self, _camera: &str) -> Allowed {
        self.allowed.clone()
    }
}

pub fn run(opts: Opts) -> Result<()> {
    tokio::runtime::Runtime::new()?.block_on(async move {
        let mut hubs = HashMap::new();
        let mut sources: Vec<(StreamKind, HubHandle, Arc<FixtureSource>)> = vec![];
        let mut kinds = vec![];
        for (kind, path) in [
            (StreamKind::Main, &opts.main),
            (StreamKind::Sub, &opts.sub),
            (StreamKind::Extern, &opts.extern_),
        ] {
            let Some(path) = path else { continue };
            let data = std::fs::read(path).with_context(|| path.display().to_string())?;
            let source = Arc::new(FixtureSource::new(
                parse_bcm(&data),
                1.0,
                Some(opts.battery),
            ));
            source.looping.store(opts.r#loop, SeqCst);
            let mut cfg = HubConfig::new(&format!("{}/{}", opts.camera, kind.name()));
            cfg.battery = opts.battery;
            cfg.linger = Duration::from_secs_f64(opts.linger);
            let hub = spawn(cfg, source.clone());
            hubs.insert(
                MountTarget {
                    camera: opts.camera.clone(),
                    kind,
                },
                hub.clone(),
            );
            sources.push((kind, hub, source));
            kinds.push(kind);
        }
        anyhow::ensure!(!kinds.is_empty(), "give --main and/or --sub");
        let users: Vec<User> = opts
            .users
            .iter()
            .map(|u| {
                let (name, pass) = u.split_once(':').unwrap_or((u, ""));
                User {
                    name: name.into(),
                    pass: Some(pass.into()),
                }
            })
            .collect();
        let schemes = match opts.auth.as_str() {
            "digest" => Schemes::Digest,
            "basic" => Schemes::Basic,
            _ => Schemes::Both,
        };
        let permitted = (!opts.permitted.is_empty()).then_some(opts.permitted.as_slice());
        let allowed = Allowed::from_config(permitted, !users.is_empty());
        let routes = Routes::default();
        routes.set(aliases(&opts.camera, &kinds));
        let server = Server::new(
            ServerConfig::default(),
            routes,
            Auth::new(users, schemes),
            Arc::new(FixtureProvider { hubs, allowed }),
        );
        let listener = tokio::net::TcpListener::bind(&opts.bind).await?;
        eprintln!("rtsp-serve: listening on {}", listener.local_addr()?);
        if let Some(f) = &opts.ready_file {
            std::fs::write(f, listener.local_addr()?.to_string())?;
        }
        let cancel = CancellationToken::new();
        tokio::spawn(server.serve(listener, cancel.clone()));
        // Stall schedule, per source start
        if let Some(at) = opts.stall_at {
            for (_, _, source) in &sources {
                let source = source.clone();
                let stall_for = opts.stall_for;
                tokio::spawn(async move {
                    let mut seen = 0;
                    loop {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        let n = source.starts.lock().unwrap().len();
                        if n > seen {
                            seen = n;
                            tokio::time::sleep(Duration::from_secs_f64(at)).await;
                            eprintln!("rtsp-serve: stalling for {stall_for} s");
                            source.stalled.store(true, SeqCst);
                            tokio::time::sleep(Duration::from_secs_f64(stall_for)).await;
                            source.stalled.store(false, SeqCst);
                            eprintln!("rtsp-serve: resumed");
                        }
                    }
                });
            }
        }
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                _ = sigterm() => break,
                _ = tick.tick() => {}
            }
            if let Some(path) = &opts.stats {
                let mut out = serde_json::Map::new();
                for (kind, hub, source) in &sources {
                    let st = hub.stats().await.unwrap_or_default();
                    out.insert(
                        kind.name().into(),
                        serde_json::json!({
                            "starts": source.starts.lock().unwrap().len(),
                            "learning_starts": st.learning_starts,
                            "running": source.running.load(SeqCst),
                            "sessions": st.sessions,
                            "playing": st.playing,
                            "frames_out": st.frames_out,
                            "guard_engaged": st.guard_engaged,
                            "dropped_by_budget": st.dropped_by_budget,
                            "bridged_us": st.bridged_us,
                        }),
                    );
                }
                let tmp = path.with_extension("tmp");
                std::fs::write(&tmp, serde_json::Value::Object(out).to_string())?;
                std::fs::rename(&tmp, path)?;
            }
        }
        cancel.cancel();
        Ok(())
    })
}

#[cfg(unix)]
async fn sigterm() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(_) => std::future::pending().await,
    }
}
