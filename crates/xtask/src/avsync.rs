//! `xtask avsync`: the A/V sync detector for SPEC A16 (PLAN 3.10)
//!
//! The fixtures flash the picture white and beep together for 0.2 s every 2 s. This decodes
//! a recording with ffmpeg, finds flash onsets (mean luma) and beep onsets (RMS over 10 ms
//! windows), pairs each flash with the nearest beep and reports the offsets.

use anyhow::{ensure, Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::process::Command;

#[derive(Parser)]
pub struct Opts {
    /// The recording (any container ffmpeg reads; `-c copy` from the RTSP server)
    input: PathBuf,
    /// ffmpeg binary (default: $NEOLINK_FFMPEG, then ffmpeg on PATH)
    #[arg(long)]
    ffmpeg: Option<PathBuf>,
    /// Fail when any |audio − video| exceeds this
    #[arg(long, default_value_t = 150.0)]
    max_ms: f64,
    /// Fail with fewer flash/beep pairs than this
    #[arg(long, default_value_t = 2)]
    min_pairs: usize,
}

fn ffmpeg(opts: &Opts) -> PathBuf {
    opts.ffmpeg
        .clone()
        .or_else(|| std::env::var_os("NEOLINK_FFMPEG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

/// `(pts_time, value)` per frame from ffmpeg's metadata=print output
fn metadata_series(opts: &Opts, args: &[&str], key: &str) -> Result<Vec<(f64, f64)>> {
    let out = Command::new(ffmpeg(opts))
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(&opts.input)
        .args(args)
        .args(["-f", "null", "-"])
        .output()
        .context("running ffmpeg")?;
    ensure!(
        out.status.success(),
        "ffmpeg failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let mut series = vec![];
    let mut t = None;
    for line in text.lines() {
        if let Some(p) = line
            .split_whitespace()
            .find_map(|w| w.strip_prefix("pts_time:"))
        {
            t = p.parse::<f64>().ok();
        } else if let Some(v) = line.strip_prefix(&format!("{key}=")) {
            if let (Some(t), Ok(v)) = (t, v.trim().parse::<f64>()) {
                series.push((t, v));
            }
        }
    }
    Ok(series)
}

/// Times where the value rises through the midpoint of its range, at least 1 s apart
fn onsets(series: &[(f64, f64)], floor: f64) -> Vec<f64> {
    let vals = series.iter().map(|s| s.1.max(floor));
    let (lo, hi) = vals.fold((f64::MAX, f64::MIN), |(a, b), v| (a.min(v), b.max(v)));
    let mid = (lo + hi) / 2.0;
    let mut out: Vec<f64> = vec![];
    let mut above = true; // ignore a recording that starts mid-flash
    for (t, v) in series {
        let now = v.max(floor) > mid;
        if now && !above && out.last().is_none_or(|l| t - l > 1.0) {
            out.push(*t);
        }
        above = now;
    }
    out
}

pub struct Report {
    pub pairs: Vec<(f64, f64)>,
    pub flashes: usize,
    pub beeps: usize,
}

pub fn measure(opts: &Opts) -> Result<Report> {
    let video = metadata_series(
        opts,
        &[
            "-an",
            "-vf",
            "signalstats,metadata=print:key=lavfi.signalstats.YAVG:file=-",
        ],
        "lavfi.signalstats.YAVG",
    )?;
    let audio = metadata_series(
        opts,
        &[
            "-vn",
            "-af",
            "asetnsamples=n=160,astats=metadata=1:reset=1,\
             ametadata=print:key=lavfi.astats.Overall.RMS_level:file=-",
        ],
        "lavfi.astats.Overall.RMS_level",
    )?;
    let flashes = onsets(&video, 0.0);
    let beeps = onsets(&audio, -90.0);
    let mut pairs = vec![];
    for f in &flashes {
        if let Some(b) = beeps
            .iter()
            .min_by(|a, b| (*a - f).abs().total_cmp(&(*b - f).abs()))
        {
            if (b - f).abs() < 1.0 {
                pairs.push((*f, (b - f) * 1000.0));
            }
        }
    }
    Ok(Report {
        pairs,
        flashes: flashes.len(),
        beeps: beeps.len(),
    })
}

pub fn run(opts: Opts) -> Result<()> {
    let r = measure(&opts)?;
    for (t, off) in &r.pairs {
        println!("flash at {t:8.3} s: audio {off:+7.1} ms");
    }
    let worst = r.pairs.iter().map(|p| p.1.abs()).fold(0.0, f64::max);
    let mean = r.pairs.iter().map(|p| p.1).sum::<f64>() / r.pairs.len().max(1) as f64;
    println!(
        "pairs {} (flashes {}, beeps {}), mean {mean:+.1} ms, worst |{worst:.1}| ms",
        r.pairs.len(),
        r.flashes,
        r.beeps
    );
    ensure!(
        r.pairs.len() >= opts.min_pairs,
        "only {} flash/beep pairs",
        r.pairs.len()
    );
    ensure!(
        worst <= opts.max_ms,
        "A/V offset {worst:.1} ms over {} ms",
        opts.max_ms
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_onsets() {
        let s: Vec<(f64, f64)> = (0..400)
            .map(|i| {
                let t = i as f64 * 0.01;
                let on = (t + 1.5) % 2.0 < 0.2;
                (t, if on { -9.0 } else { -120.0 })
            })
            .collect();
        let o = onsets(&s, -90.0);
        assert_eq!(o.len(), 2);
        assert!(
            (o[0] - 0.5).abs() < 0.011 && (o[1] - 2.5).abs() < 0.011,
            "{o:?}"
        );
    }
}
