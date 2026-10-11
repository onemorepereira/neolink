//! Development tasks: `cargo xtask <command>`

mod annexb;
mod mkfixture;

use clap::{Parser, Subcommand};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate synthetic `.bcm` fixtures with ffmpeg
    Mkfixture(mkfixture::Opts),
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Cmd::Mkfixture(opts) => mkfixture::run(opts),
    }
}
