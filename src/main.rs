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
use std::fs;
use validator::Validate;

mod battery;
mod cmdline;
mod common;
mod config;
#[cfg(feature = "gstreamer")]
mod image;
mod mqtt;
mod pir;
mod ptz;
mod reboot;
#[cfg(feature = "gstreamer")]
mod rtsp;
mod services;
mod snapshot;
mod statusled;
#[cfg(feature = "gstreamer")]
mod talk;
mod users;
mod utils;

use cmdline::{Command, Opt};
use common::NeoReactor;
use config::Config;

pub(crate) type AnyResult<T> = Result<T, anyhow::Error>;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    info!(
        "Neolink {} {}",
        env!("NEOLINK_VERSION"),
        env!("NEOLINK_PROFILE")
    );

    raise_fd_limit();
    let opt = Opt::parse_from(args_with_mode(std::env::args_os().collect()));

    let conf_path = opt.config.context("Must supply --config file")?;
    let config_text = fs::read_to_string(&conf_path)
        .with_context(|| format!("Failed to read {:?}", conf_path))?;
    let config: Config = toml::from_str(&config_text)
        .with_context(|| format!("Failed to parse the {:?} config file", conf_path))?;
    config::warn_unknown_keys(&config_text, &conf_path.display().to_string());

    config
        .validate()
        .with_context(|| format!("Failed to validate the {:?} config file", conf_path))?;

    let neo_reactor = NeoReactor::new(config.clone()).await;

    // Run the command until it ends or a signal arrives, then stop every camera cleanly so
    // that it is logged out instead of dropped (PLAN 1.5)
    let cmd = opt.cmd;
    let run = async {
        match cmd {
            #[cfg(feature = "gstreamer")]
            None => {
                warn!(
                    "Deprecated command line option. Please use: `neolink rtsp --config={:?}`",
                    conf_path
                );
                rtsp::main(rtsp::Opt {}, neo_reactor.clone()).await?;
            }
            #[cfg(not(feature = "gstreamer"))]
            None => {
                // When gstreamer is disabled the default command is MQTT
                warn!(
                    "Deprecated command line option. Please use: `neolink mqtt --config={:?}`",
                    conf_path
                );
                mqtt::main(mqtt::Opt {}, neo_reactor.clone()).await?;
            }
            #[cfg(feature = "gstreamer")]
            Some(Command::Rtsp(opts)) => {
                rtsp::main(opts, neo_reactor.clone()).await?;
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
            #[cfg(feature = "gstreamer")]
            Some(Command::MqttRtsp(opts)) => {
                tokio::select! {
                    v = mqtt::main(opts, neo_reactor.clone()) => v,
                    v = rtsp::main(rtsp::Opt {}, neo_reactor.clone()) => v,
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
        sig = shutdown_signal() => {
            info!("Received {sig}, shutting down");
            Ok(())
        }
    };
    neo_reactor.shutdown().await;
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

/// SIGTERM (podman/docker stop) or SIGINT (Ctrl-C)
async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            (Ok(mut term), Ok(mut int)) => tokio::select! {
                _ = term.recv() => "SIGTERM",
                _ = int.recv() => "SIGINT",
            },
            _ => std::future::pending().await,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl-C"
    }
}

/// `NEO_LINK_MODE` (the container's mode variable) supplies the subcommand when the
/// command line has none, so the image can use exec-form CMD with neolink as PID 1 and
/// still let users pick the mode with an environment variable (PLAN 1.9)
fn args_with_mode(mut args: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    use clap::CommandFactory;
    let Some(mode) = std::env::var_os("NEO_LINK_MODE").filter(|m| !m.is_empty()) else {
        return args;
    };
    let command = Opt::command();
    let has_subcommand = args.iter().skip(1).any(|a| {
        command
            .get_subcommands()
            .any(|c| a.to_str() == Some(c.get_name()))
    });
    if !has_subcommand {
        args.insert(1.min(args.len()), mode);
    }
    args
}

/// Raise the open-file soft limit to the hard limit (the entrypoint's `ulimit -n` did
/// this before); each RTSP client and camera connection needs descriptors
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

    #[test]
    fn test_args_with_mode() {
        let args = |v: &[&str]| v.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        std::env::set_var("NEO_LINK_MODE", "mqtt");
        assert_eq!(
            args_with_mode(args(&["neolink", "--config", "/etc/neolink.toml"])),
            args(&["neolink", "mqtt", "--config", "/etc/neolink.toml"])
        );
        assert_eq!(
            args_with_mode(args(&["neolink", "battery", "--config", "c", "cam"])),
            args(&["neolink", "battery", "--config", "c", "cam"])
        );
        std::env::remove_var("NEO_LINK_MODE");
        assert_eq!(
            args_with_mode(args(&["neolink", "--config", "c"])),
            args(&["neolink", "--config", "c"])
        );
    }
}
