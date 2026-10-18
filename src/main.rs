#![warn(unused_crate_dependencies)]
#![warn(missing_docs)]
#![warn(clippy::todo)]
//!
//! # Neolink
//!
//! Neolink is a small program that acts a general contol interface for Reolink IP cameras.
//!
//! It contains sub commands for running an rtsp proxy which can be used on Reolink cameras
//! that do not nativly support RTSP.
//!
//! This program is free software: you can redistribute it and/or modify it under the terms of the
//! GNU General Public License as published by the Free Software Foundation, either version 3 of
//! the License, or (at your option) any later version.
//!
//! This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
//! without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See
//! the GNU General Public License for more details.
//!
//! You should have received a copy of the GNU General Public License along with this program. If
//! not, see <https://www.gnu.org/licenses/>.
//!
//! Neolink source code is available online at <https://github.com/QuantumEntangledAndy/neolink>
//!
#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

use anyhow::{Context, Result};
use clap::Parser;
use env_logger::Env;
use log::*;
use std::ffi::{OsStr, OsString};
use std::fs;
use validator::Validate;

mod battery;
mod cmdline;
mod common;
mod config;
#[cfg(feature = "api")]
mod api;
#[cfg(feature = "gstreamer")]
mod image;
mod mqtt;
mod pir;
mod ptz;
mod reboot;
#[cfg(feature = "gstreamer")]
mod rtsp;
#[cfg(any(feature = "gstreamer", feature = "native-rtsp"))]
mod rtsp_cmd;
#[cfg(feature = "native-rtsp")]
mod rtsp_native;
mod services;
mod snapshot;
#[cfg(feature = "native-snapshot")]
mod snap_stream;
mod statusled;
#[cfg(feature = "gstreamer")]
mod talk;
mod users;
mod utils;

use cmdline::{Command, Opt};
use common::NeoReactor;
use config::Config;

pub(crate) type AnyResult<T> = Result<T, anyhow::Error>;

fn main() -> Result<()> {
    // Before anything else (no logger, no runtime): the container image runs neolink as its
    // entrypoint, so its command line may be neolink's arguments or a whole other command
    raise_fd_limit();
    let args = std::env::args_os().collect();
    match entry(args, std::env::var_os("NEO_LINK_MODE"), &is_program) {
        Entry::Exec(argv) => exec_command(argv),
        Entry::Neolink(args) => neolink_main(args),
    }
}

/// Makes ring rustls' process-wide crypto provider (MQTT over TLS, push notifications).
/// rustls picks one by itself only when exactly one is built in; the `pushnoti` feature
/// also builds aws-lc-rs (reqwest's default, through fcm-push-listener).
pub(crate) fn install_crypto_provider() {
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        // Only the first call installs one; a later call (tests) changes nothing
        debug!("rustls crypto provider already installed");
    }
}

#[tokio::main]
async fn neolink_main(args: Vec<OsString>) -> Result<()> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
    install_crypto_provider();

    info!(
        "Neolink {} {}",
        env!("NEOLINK_VERSION"),
        env!("NEOLINK_PROFILE")
    );

    let opt = Opt::parse_from(args);
    // Fail now, not on the first `docker stop`, if SIGTERM/SIGINT can't be caught: as PID 1
    // without a handler neolink would ignore SIGTERM and be killed without logging out
    let mut signals =
        ShutdownSignals::new().context("Could not install the SIGTERM/SIGINT handlers")?;

    let conf_path = opt.config.context("Must supply --config file")?;
    let config_text = fs::read_to_string(&conf_path)
        .with_context(|| format!("Failed to read {:?}", conf_path))?;
    let config: Config = toml::from_str(&config_text)
        .with_context(|| format!("Failed to parse the {:?} config file", conf_path))?;
    config::warn_unknown_keys(&config_text, &conf_path.display().to_string());

    config
        .validate()
        .with_context(|| format!("Failed to validate the {:?} config file", conf_path))?;
    config::warn_feature_mismatches(&config, &config_text);
    // The config is read (as root, if the image runs so): now become PUID/PGID if asked
    drop_privileges()?;
    check_state_dir();

    let neo_reactor = NeoReactor::new(config.clone()).await;

    #[cfg(feature = "api")]
    {
        let config_rx = neo_reactor.config().await?;
        let command_tx = neo_reactor.commander();
        let api_cancel = tokio_util::sync::CancellationToken::new();
        let api_cancel_child = api_cancel.child_token();
        let api_conf_path = conf_path.clone();
        let api_bind = config.api_bind.clone();
        let api_port = config.api_port;
        let api_reactor = neo_reactor.clone();
        tokio::spawn(async move {
            if let Err(e) = api::server::start(
                api_conf_path.into(),
                config_rx,
                command_tx,
                api_reactor,
                api_bind,
                api_port,
                api_cancel_child,
            )
            .await
            {
                log::error!("HTTP API error: {e:?}");
            }
        });
        log::info!("HTTP API started on {}:{}", config.api_bind, config.api_port);
    }

    // Run the command until it ends or a signal arrives, then stop every camera cleanly so
    // that it is logged out instead of dropped (PLAN 1.5)
    let cmd = opt.cmd;
    let run = async {
        match cmd {
            #[cfg(any(feature = "gstreamer", feature = "native-rtsp"))]
            None => {
                warn!(
                    "Deprecated command line option. Please use: `neolink rtsp --config={:?}`",
                    conf_path
                );
                rtsp_cmd::main(Default::default(), neo_reactor.clone()).await?;
            }
            #[cfg(not(any(feature = "gstreamer", feature = "native-rtsp")))]
            None => {
                // When gstreamer is disabled the default command is MQTT
                warn!(
                    "Deprecated command line option. Please use: `neolink mqtt --config={:?}`",
                    conf_path
                );
                mqtt::main(mqtt::Opt {}, neo_reactor.clone()).await?;
            }
            #[cfg(any(feature = "gstreamer", feature = "native-rtsp"))]
            Some(Command::Rtsp(opts)) => {
                rtsp_cmd::main(opts, neo_reactor.clone()).await?;
            }
            Some(Command::StatusLight(opts)) => {
                statusled::main(opts, neo_reactor.clone()).await?;
            }
            Some(Command::Reboot(opts)) => {
                reboot::main(opts, neo_reactor.clone()).await?;
            }
            Some(Command::Pir(opts)) => {
                pir::main(opts, neo_reactor.clone()).await?;
            }
            Some(Command::Ptz(opts)) => {
                ptz::main(opts, neo_reactor.clone()).await?;
            }
            #[cfg(feature = "gstreamer")]
            Some(Command::Talk(opts)) => {
                talk::main(opts, neo_reactor.clone()).await?;
            }
            Some(Command::Mqtt(opts)) => {
                mqtt::main(opts, neo_reactor.clone()).await?;
            }
            #[cfg(any(feature = "gstreamer", feature = "native-rtsp"))]
            Some(Command::MqttRtsp(opts)) => {
                tokio::select! {
                    v = mqtt::main(opts, neo_reactor.clone()) => v,
                    v = rtsp_cmd::main(Default::default(), neo_reactor.clone()) => v,
                }?;
            }
            #[cfg(feature = "gstreamer")]
            Some(Command::Image(opts)) => {
                image::main(opts, neo_reactor.clone()).await?;
            }
            Some(Command::Battery(opts)) => {
                battery::main(opts, neo_reactor.clone()).await?;
            }
            Some(Command::Services(opts)) => {
                services::main(opts, neo_reactor.clone()).await?;
            }
            Some(Command::Users(opts)) => {
                users::main(opts, neo_reactor.clone()).await?;
            }
        }
        AnyResult::Ok(())
    };
    #[cfg(all(unix, feature = "debug-stats"))]
    {
        // SIGUSR1 logs task and permit counts (PLAN 1.10)
        let reactor = neo_reactor.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let Ok(mut usr1) = signal(SignalKind::user_defined1()) else {
                return;
            };
            while usr1.recv().await.is_some() {
                let tasks = tokio::runtime::Handle::current()
                    .metrics()
                    .num_alive_tasks();
                info!("stats: tasks={tasks}");
                for line in reactor.stats().await {
                    info!("stats: {line}");
                }
            }
        });
    }
    let result = tokio::select! {
        v = run => v,
        sig = signals.recv() => {
            info!("Received {sig}, shutting down");
            Ok(())
        }
    };
    // R27: the native RTSP server ends first (BYE, streams stopped), then the cameras log
    // out. Each step has its own bound; the whole is held to 6 s, well inside docker's
    // 10 s stop timeout
    let stop = async {
        #[cfg(feature = "native-rtsp")]
        rtsp_native::shutdown().await;
        neo_reactor.shutdown().await;
    };
    if tokio::time::timeout(std::time::Duration::from_secs(6), stop)
        .await
        .is_err()
    {
        warn!("Shutdown did not finish within 6 s; exiting");
    }
    // Exit now: dropping the runtime would wait for GStreamer's blocking threads
    let code = match result {
        Ok(()) => 0,
        Err(e) => {
            log::error!("{e:?}");
            1
        }
    };
    std::process::exit(code)
}

/// SIGTERM (podman/docker stop) and SIGINT (Ctrl-C), registered up front so that a
/// failure to register them stops neolink at startup instead of leaving it deaf to them
struct ShutdownSignals {
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    int: tokio::signal::unix::Signal,
}

impl ShutdownSignals {
    fn new() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            Ok(Self {
                term: signal(SignalKind::terminate())?,
                int: signal(SignalKind::interrupt())?,
            })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    async fn recv(&mut self) -> &'static str {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.term.recv() => "SIGTERM",
                _ = self.int.recv() => "SIGINT",
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            "Ctrl-C"
        }
    }
}

/// What the command line asks for
#[derive(Debug, PartialEq)]
enum Entry {
    /// Run neolink with these arguments (argv\[0\] first)
    Neolink(Vec<OsString>),
    /// Replace neolink with this command (the program first)
    Exec(Vec<OsString>),
}

/// Reads the command line the way the container image needs it. neolink is the image's
/// entrypoint (exec form, so it is PID 1 and gets `docker stop`'s SIGTERM), which means its
/// command line is the container's command. That command can be:
/// - neolink's arguments (`--config /etc/neolink.toml`, `rtsp --config ...`). Without a
///   subcommand, `NEO_LINK_MODE` supplies one;
/// - neolink itself, as compose files written for the old shell entrypoint have it
///   (`neolink mqtt --config ...`, `/usr/local/bin/neolink rtsp ...`): it is dropped;
/// - any other program (`sh -c "..."`, `/custom/script.sh`), as the old entrypoint ran
///   them: neolink hands over to it ([`Entry::Exec`]). A neolink subcommand always wins
///   over a program of the same name (`users`).
///
/// `is_program` tells whether a bare name is an executable in `PATH`.
fn entry(
    mut args: Vec<OsString>,
    mode: Option<OsString>,
    is_program: &dyn Fn(&OsStr) -> bool,
) -> Entry {
    if args
        .get(1)
        .is_some_and(|a| std::path::Path::new(a).file_name() == Some(OsStr::new("neolink")))
    {
        args.remove(1);
    }
    let Some(first) = args.get(1) else {
        return Entry::Neolink(with_mode(args, mode));
    };
    let first_text = first.to_string_lossy();
    if first_text.starts_with('-') {
        return Entry::Neolink(with_mode(args, mode));
    }
    if is_subcommand(first) {
        return Entry::Neolink(args);
    }
    if first_text.contains('/') || is_program(first) {
        return Entry::Exec(args.split_off(1));
    }
    // Not neolink's and not a program: clap reports the unknown subcommand after this
    eprintln!(
        "neolink: {first_text:?} is not a neolink subcommand, and there is no program of \
         that name in PATH (the native image has no shell)"
    );
    Entry::Neolink(args)
}

/// Adds `NEO_LINK_MODE` as the subcommand when the arguments parse and have none. Parsing
/// (rather than looking for a subcommand's name) keeps option values such as
/// `--config rtsp` from being taken for one.
fn with_mode(mut args: Vec<OsString>, mode: Option<OsString>) -> Vec<OsString> {
    let Some(mode) = mode.filter(|m| !m.is_empty()) else {
        return args;
    };
    // Errors (bad arguments, --help, --version) are left for the real parse to report
    if let Ok(Opt { cmd: None, .. }) = Opt::try_parse_from(&args) {
        args.insert(1.min(args.len()), mode);
    }
    args
}

fn is_subcommand(name: &OsStr) -> bool {
    use clap::CommandFactory;
    let Some(name) = name.to_str() else {
        return false;
    };
    name == "help" || Opt::command().find_subcommand(name).is_some()
}

/// An executable file of this name in `PATH`
fn is_program(name: &OsStr) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if name.is_empty() {
            return false;
        }
        let path = std::env::var_os("PATH").unwrap_or_else(|| {
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()
        });
        std::env::split_paths(&path).any(|dir| {
            fs::metadata(dir.join(name))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        false
    }
}

/// Becomes the given command, as the old `/entrypoint.sh` ran it: same process (so PID 1
/// stays PID 1), same environment, with the raised open-file limit. `PUID`/`PGID` are left
/// to the neolink that the command starts, after it has read its config.
fn exec_command(argv: Vec<OsString>) -> Result<()> {
    let program = &argv[0];
    eprintln!("neolink: running {program:?} (not a neolink subcommand)");
    let mut command = std::process::Command::new(program);
    command.args(&argv[1..]);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = command.exec();
        eprintln!("neolink: could not run {program:?}: {err}");
        std::process::exit(if err.kind() == std::io::ErrorKind::NotFound {
            127
        } else {
            126
        })
    }
    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .with_context(|| format!("could not run {program:?}"))?;
        std::process::exit(status.code().unwrap_or(1))
    }
}

/// `PUID`/`PGID` (the container convention): switch to that user and group after reading
/// the config, when running as root (DESIGN §3.3). Nothing happens without `PUID`.
fn drop_privileges() -> Result<()> {
    #[cfg(unix)]
    {
        let Some(uid) = std::env::var("PUID").ok().filter(|v| !v.is_empty()) else {
            return Ok(());
        };
        let uid: libc::uid_t = uid.parse().context("PUID must be a number")?;
        let gid: libc::gid_t = match std::env::var("PGID").ok().filter(|v| !v.is_empty()) {
            Some(g) => g.parse().context("PGID must be a number")?,
            None => uid as libc::gid_t,
        };
        // SAFETY: geteuid has no preconditions
        let euid = unsafe { libc::geteuid() };
        if euid != 0 {
            if euid != uid {
                warn!("PUID={uid} ignored: not running as root");
            }
            return Ok(());
        }
        // The state directory (desc cache, learning gate, snapshots) must stay writable,
        // or every restart forgets them and a battery camera is woken more often
        hand_over_state_dir(&crate::common::state_dir(), uid, gid);
        let groups = [gid];
        // SAFETY: plain syscalls on integers; the group list is one valid gid
        unsafe {
            if libc::setgroups(1, groups.as_ptr()) != 0
                || libc::setgid(gid) != 0
                || libc::setuid(uid) != 0
            {
                anyhow::bail!(
                    "could not switch to PUID={uid} PGID={gid}: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
        info!("Running as uid {uid}, gid {gid}");
    }
    Ok(())
}

/// What neolink writes in the state directory, besides the directory itself: `camera/`
/// (state files, snapshots, locks), `cache/<camera>-<stream>/` (stream descriptions) and
/// `learn-<camera>.txt`. Nothing else is listed, so a state directory pointed at a shared
/// path is not handed over wholesale.
#[cfg(unix)]
fn state_dir_entries(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    fn walk(p: std::path::PathBuf, depth: usize, out: &mut Vec<std::path::PathBuf>) {
        // symlink_metadata: a symlink is listed but never followed
        let is_dir = fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir());
        out.push(p.clone());
        if is_dir && depth > 0 {
            for e in fs::read_dir(&p).into_iter().flatten().flatten() {
                walk(e.path(), depth - 1, out);
            }
        }
    }
    let mut out = vec![];
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name == "camera" || name == "cache" || name.starts_with("learn-") {
            walk(e.path(), 2, &mut out);
        }
    }
    out
}

/// Gives the state directory, and the root-owned parts of it that neolink writes, to
/// `uid`/`gid`: a volume first used by a root neolink holds root-owned `camera/` files
/// that the dropped user could not replace. Symlinks inside it are not followed.
#[cfg(unix)]
fn hand_over_state_dir(dir: &std::path::Path, uid: u32, gid: u32) {
    use std::os::unix::fs::MetadataExt;
    if dir.parent().is_none() {
        warn!(
            "The state directory is {}: not changing its owner",
            dir.display()
        );
        return;
    }
    let _ = fs::create_dir_all(dir.join("camera"));
    let give = |p: &std::path::Path, top: bool| {
        let meta = if top {
            fs::metadata(p)
        } else {
            fs::symlink_metadata(p)
        };
        let Ok(meta) = meta else { return };
        if meta.uid() != 0 || meta.file_type().is_symlink() {
            return;
        }
        let res = if top {
            std::os::unix::fs::chown(p, Some(uid), Some(gid))
        } else {
            std::os::unix::fs::lchown(p, Some(uid), Some(gid))
        };
        if let Err(e) = res {
            warn!("could not hand {} to PUID={uid}: {e}", p.display());
        }
    };
    give(dir, true);
    for p in state_dir_entries(dir) {
        give(&p, false);
    }
}

/// Logs an error when the state directory or its `camera/` subdirectory can't be written:
/// the stream cache, the learning-start limit and snapshots would then be lost on every
/// restart, so a battery camera is woken more often
fn check_state_dir() {
    let dir = crate::common::state_dir();
    for d in [dir.clone(), dir.join("camera")] {
        let probe = d.join(".neolink-write-test");
        match fs::create_dir_all(&d).and_then(|()| fs::write(&probe, b"ok")) {
            Ok(()) => {
                let _ = fs::remove_file(&probe);
            }
            Err(e) => {
                #[cfg(unix)]
                // SAFETY: geteuid/getegid have no preconditions
                let who = unsafe { format!("uid {}, gid {}", libc::geteuid(), libc::getegid()) };
                #[cfg(not(unix))]
                let who = "this user".to_string();
                error!(
                    "The state directory {} is not writable by {who} ({e}): the stream cache, \
                     the learning-start limit and snapshots will not survive a restart, so a \
                     battery camera is woken more often. Make it writable by that user, or \
                     run as root with PUID/PGID so that neolink hands it over",
                    d.display()
                );
                return;
            }
        }
    }
}

/// Raise the open-file soft limit to the hard limit (the old entrypoint's `ulimit -n` did
/// this); each RTSP client and camera connection needs descriptors
fn raise_fd_limit() {
    #[cfg(unix)]
    // SAFETY: getrlimit/setrlimit only read and write the struct we pass
    unsafe {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 && lim.rlim_cur < lim.rlim_max {
            lim.rlim_cur = lim.rlim_max;
            let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
        }
    }
}

#[cfg(test)]
mod main_tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    /// `sh`, `bash` and `users` are programs; nothing else is
    fn programs(name: &OsStr) -> bool {
        matches!(name.to_str(), Some("sh" | "bash" | "users"))
    }

    fn run(v: &[&str], mode: Option<&str>) -> Entry {
        entry(args(v), mode.map(OsString::from), &programs)
    }

    fn neolink(v: &[&str]) -> Entry {
        Entry::Neolink(args(v))
    }

    #[test]
    fn test_mode_supplies_the_subcommand() {
        let m = Some("mqtt");
        // The image's CMD
        assert_eq!(
            run(&["neolink", "--config", "/etc/neolink.toml"], m),
            neolink(&["neolink", "mqtt", "--config", "/etc/neolink.toml"])
        );
        assert_eq!(
            run(
                &["neolink", "--config=/etc/neolink.toml"],
                Some("mqtt-rtsp")
            ),
            neolink(&["neolink", "mqtt-rtsp", "--config=/etc/neolink.toml"])
        );
        // A subcommand on the command line wins
        assert_eq!(
            run(&["neolink", "battery", "--config", "c", "cam"], m),
            neolink(&["neolink", "battery", "--config", "c", "cam"])
        );
        assert_eq!(
            run(&["neolink", "--config", "c", "rtsp"], m),
            neolink(&["neolink", "--config", "c", "rtsp"])
        );
        // No mode, or an empty one: unchanged
        assert_eq!(
            run(&["neolink", "--config", "c"], None),
            neolink(&["neolink", "--config", "c"])
        );
        assert_eq!(
            run(&["neolink", "--config", "c"], Some("")),
            neolink(&["neolink", "--config", "c"])
        );
    }

    #[test]
    fn test_option_values_are_not_subcommands() {
        // A config file named like a subcommand, or a path containing one
        for v in [
            &["neolink", "--config", "rtsp"][..],
            &["neolink", "-c", "mqtt"],
            &["neolink", "--config=rtsp"],
            &["neolink", "--config", "/etc/neolink/rtsp"],
            &["neolink", "--config", "/my neolink dir/neolink.toml"],
        ] {
            let mut want = args(v);
            want.insert(1, "mqtt".into());
            assert_eq!(run(v, Some("mqtt")), Entry::Neolink(want), "{v:?}");
        }
    }

    #[test]
    fn test_help_and_version_are_left_alone() {
        for v in [
            &["neolink", "--help"][..],
            &["neolink", "-h"],
            &["neolink", "--version"],
            &["neolink", "-V"],
            &["neolink", "help"],
            &["neolink", "rtsp", "--help"],
            &["neolink"],
        ] {
            assert_eq!(run(v, Some("rtsp")), neolink(v), "{v:?}");
        }
    }

    #[test]
    fn test_neolink_itself_is_dropped() {
        assert_eq!(
            run(
                &["neolink", "/usr/local/bin/neolink", "mqtt", "--config", "c"],
                None
            ),
            neolink(&["neolink", "mqtt", "--config", "c"])
        );
        assert_eq!(
            run(
                &["neolink", "neolink", "rtsp", "--config=/etc/neolink.toml"],
                Some("mqtt")
            ),
            neolink(&["neolink", "rtsp", "--config=/etc/neolink.toml"])
        );
        // Then NEO_LINK_MODE applies as usual
        assert_eq!(
            run(&["neolink", "neolink", "--config", "c"], Some("mqtt")),
            neolink(&["neolink", "mqtt", "--config", "c"])
        );
    }

    #[test]
    fn test_other_commands_are_executed() {
        assert_eq!(
            run(
                &["neolink", "sh", "-c", "echo hi; neolink rtsp"],
                Some("rtsp")
            ),
            Entry::Exec(args(&["sh", "-c", "echo hi; neolink rtsp"]))
        );
        assert_eq!(
            run(&["neolink", "bash", "-lc", "true"], None),
            Entry::Exec(args(&["bash", "-lc", "true"]))
        );
        // Paths are always programs (a missing one fails at exec with a clear error)
        assert_eq!(
            run(&["neolink", "/bin/sh", "/custom/script.sh"], None),
            Entry::Exec(args(&["/bin/sh", "/custom/script.sh"]))
        );
        assert_eq!(
            run(&["neolink", "./run.sh"], None),
            Entry::Exec(args(&["./run.sh"]))
        );
        // A file whose name merely contains "neolink"
        assert_eq!(
            run(
                &["neolink", "/opt/neolink-wrapper.sh", "--config", "c"],
                None
            ),
            Entry::Exec(args(&["/opt/neolink-wrapper.sh", "--config", "c"]))
        );
    }

    #[test]
    fn test_subcommands_win_over_programs() {
        // coreutils has a `users` program; neolink has a `users` subcommand
        assert_eq!(
            run(
                &["neolink", "users", "--config", "c", "cam", "list"],
                Some("rtsp")
            ),
            neolink(&["neolink", "users", "--config", "c", "cam", "list"])
        );
        // Unknown words go to clap, which reports them
        assert_eq!(
            run(&["neolink", "rstp", "--config", "c"], Some("rtsp")),
            neolink(&["neolink", "rstp", "--config", "c"])
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_state_dir_entries() {
        let dir = std::env::temp_dir().join(format!("neolink-state-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("camera")).unwrap();
        fs::create_dir_all(dir.join("cache/cam-main")).unwrap();
        fs::create_dir_all(dir.join("unrelated/deep")).unwrap();
        fs::write(dir.join("camera/cam.json"), b"{}").unwrap();
        fs::write(dir.join("cache/cam-main/desc.json"), b"{}").unwrap();
        fs::write(dir.join("learn-cam.txt"), b"").unwrap();
        fs::write(dir.join("other.txt"), b"").unwrap();
        std::os::unix::fs::symlink("/etc", dir.join("camera/link")).unwrap();
        let mut got: Vec<_> = state_dir_entries(&dir)
            .into_iter()
            .map(|p| p.strip_prefix(&dir).unwrap().display().to_string())
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                "cache",
                "cache/cam-main",
                "cache/cam-main/desc.json",
                "camera",
                "camera/cam.json",
                "camera/link",
                "learn-cam.txt",
            ]
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
