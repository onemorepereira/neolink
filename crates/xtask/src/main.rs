//! Development tasks: `cargo xtask <command>`

use anyhow::Context;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use xtask::{avsync, fakecam, golden, mkfixture, rtsp_serve};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate synthetic `.bcm` fixtures with ffmpeg
    Mkfixture(mkfixture::Opts),
    /// Run the scripted fake camera as a process (end-to-end tests)
    Fakecam(fakecam::Opts),
    /// A/V sync of a recording of the flash/beep fixtures (A16)
    Avsync(avsync::Opts),
    /// The native RTSP server on fixture sources
    RtspServe(rtsp_serve::Opts),
    /// Golden RTSP transcripts
    #[command(subcommand)]
    Golden(GoldenCmd),
}

#[derive(Subcommand)]
enum GoldenCmd {
    /// Proxy RTSP between a client and a server and record the transcript
    Record {
        #[arg(long, default_value = "127.0.0.1:8555")]
        listen: String,
        #[arg(long, default_value = "127.0.0.1:8554")]
        upstream: String,
        #[arg(long, short)]
        out: PathBuf,
        /// Exit after this many client connections have closed
        #[arg(long, default_value_t = 1)]
        conns: usize,
        /// Create this file once listening
        #[arg(long)]
        ready_file: Option<PathBuf>,
    },
    /// Normalise a recorded transcript (in place unless --out is given)
    Normalize {
        input: PathBuf,
        #[arg(long, short)]
        out: Option<PathBuf>,
    },
    /// Structural diff of two transcripts; exits 1 when they differ
    Diff {
        a: PathBuf,
        b: PathBuf,
        /// Drop request/response pairs with this status from B first (e.g. 461, after
        /// which a client retries the same request with another transport)
        #[arg(long)]
        drop_status: Option<u16>,
    },
}

fn read(p: &PathBuf) -> anyhow::Result<Vec<golden::Entry>> {
    let text = std::fs::read_to_string(p).with_context(|| p.display().to_string())?;
    golden::parse(&text).with_context(|| p.display().to_string())
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Cmd::Mkfixture(opts) => mkfixture::run(opts),
        Cmd::Fakecam(opts) => fakecam::run(opts),
        Cmd::RtspServe(opts) => rtsp_serve::run(opts),
        Cmd::Avsync(opts) => avsync::run(opts),
        Cmd::Golden(GoldenCmd::Record {
            listen,
            upstream,
            out,
            conns,
            ready_file,
        }) => golden::record(&listen, &upstream, &out, conns, ready_file.as_deref()),
        Cmd::Golden(GoldenCmd::Normalize { input, out }) => {
            let n = golden::render(&golden::normalize(&read(&input)?));
            std::fs::write(out.unwrap_or(input), n)?;
            Ok(())
        }
        Cmd::Golden(GoldenCmd::Diff { a, b, drop_status }) => {
            let mut b = read(&b)?;
            if let Some(status) = drop_status {
                b = golden::drop_status(b, status);
            }
            let d = golden::structural_diff(&golden::normalize(&read(&a)?), &golden::normalize(&b));
            for line in &d {
                println!("{}", line);
            }
            if !d.is_empty() {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}
