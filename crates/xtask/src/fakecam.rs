//! `cargo xtask fakecam`: run `neolink_core::test_util::FakeCamera` as a process
//!
//! For end-to-end tests of the neolink binary: point a camera's `address` at the printed
//! address. The counters are written as JSON to `--stats` every 100 ms and on exit.
//! Control by signals: SIGUSR1 drops every connection, SIGUSR2 toggles unreachable.

use anyhow::{Context, Result};
use neolink_core::fixture::read_all;
use neolink_core::test_util::{FakeCamera, FakeCameraConfig};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(clap::Args, Clone, Debug)]
pub struct Opts {
    /// Port on 127.0.0.1 is chosen by the OS; the address is written here
    #[arg(long)]
    addr_file: PathBuf,
    /// Where the counters go (JSON)
    #[arg(long)]
    stats: PathBuf,
    /// Fixture for mainStream
    #[arg(long)]
    main: Option<PathBuf>,
    /// Fixture for subStream
    #[arg(long)]
    sub: Option<PathBuf>,
    #[arg(long, default_value = "fakepass")]
    password: String,
    /// Battery percent; 0 makes it a mains camera
    #[arg(long, default_value_t = 80)]
    battery: u32,
}

fn load(p: &Option<PathBuf>) -> Result<Arc<Vec<neolink_core::fixture::Record>>> {
    Ok(Arc::new(match p {
        Some(p) => read_all(std::fs::File::open(p).with_context(|| p.display().to_string())?)?,
        None => vec![],
    }))
}

pub fn run(opts: Opts) -> Result<()> {
    tokio::runtime::Runtime::new()?.block_on(async move {
        let cam = FakeCamera::start(FakeCameraConfig {
            password: opts.password.clone(),
            battery_percent: if opts.battery == 0 {
                None
            } else {
                Some(opts.battery)
            },
            main: load(&opts.main)?,
            sub: load(&opts.sub)?,
            loop_media: true,
            ..Default::default()
        })
        .await?;
        std::fs::write(&opts.addr_file, cam.addr().to_string())?;
        eprintln!("fake camera at {}", cam.addr());
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;
        let mut usr1 = signal(SignalKind::user_defined1())?;
        let mut usr2 = signal(SignalKind::user_defined2())?;
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
        let mut unreachable = false;
        let write = |cam: &FakeCamera| -> Result<()> {
            let tmp = opts.stats.with_extension("tmp");
            std::fs::write(&tmp, serde_json::to_vec(&cam.stats())?)?;
            std::fs::rename(&tmp, &opts.stats)?;
            Ok(())
        };
        loop {
            tokio::select! {
                _ = term.recv() => break,
                _ = int.recv() => break,
                _ = usr1.recv() => cam.drop_connections(),
                _ = usr2.recv() => {
                    unreachable = !unreachable;
                    cam.set_unreachable(unreachable);
                }
                _ = tick.tick() => write(&cam)?,
            }
        }
        write(&cam)
    })
}
