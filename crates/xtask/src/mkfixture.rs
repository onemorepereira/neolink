//! `cargo xtask mkfixture`: synthetic `.bcm` fixtures with an Argus-like arrival schedule
//!
//! ffmpeg renders `testsrc2` with a white flash and a 1 kHz beep at the same instants
//! (every 2 s, 0.5 s into each GOP), encodes them like the Argus does (15 fps, GOP 30, no
//! B-frames, parameter sets on every IRAP, AAC-LC 16 kHz mono), and this module writes
//! them as BcMedia records.
//!
//! **Arrival model.** The camera sends one ordered stream. It starts with a backlog
//! (`prebuffer_ms` of media that is already "ready" at t=0), then each unit becomes ready
//! at its media time. A unit leaves after the previous one, at `link_kbps`, using a
//! *scheduling size* instead of its real size (synthetic frames are tiny): IRAPs count as
//! `irap_kb`. At each IRAP the link may also stall for up to `stall_ms`. With the defaults
//! this reproduces the 2026-10-10 Argus capture: a ~1.9 s backlog at start, then a
//! 0.6–1.3 s arrival gap at every IRAP followed by a catch-up burst. The `congestion`
//! variant adds the episodes of the 2026-10-11 capture, where the link slowed down and
//! arrivals fell up to 17 s behind before catching up.
//!
//! Everything, including the stalls, is deterministic: the same arguments and the same
//! ffmpeg build give the same bytes on any x86-64 CPU, because ffmpeg and the encoders run
//! without CPU-specific code paths (`cargo xtask mkfixture --standard --check`).

use crate::annexb::{split_access_units, split_adts, AccessUnit};
use anyhow::{bail, ensure, Context, Result};
use neolink_core::fixture::{Kind, Record};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Video frame period at 15 fps in µs (the Argus reports 66,667)
const FPS: u32 = 15;
const GOP: usize = 30;
const AUDIO_RATE: u32 = 16000;
const AUDIO_AU_US: u64 = 1024 * 1_000_000 / AUDIO_RATE as u64;
/// Unix time of the first frame, for the I-frame `cam_time`
const EPOCH: u32 = 1_760_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Codec {
    H264,
    H265,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Variant {
    /// A steady stream
    Plain,
    /// `cam_us` starts 10 s before the u32 wrap (override with --cam-us-start)
    Wrap,
    /// `cam_us` jumps back to 1 s at the middle GOP boundary, without a Restarted record
    Reset,
    /// Arrivals stop for --outage-s at the middle, then a Restarted record and a new IRAP;
    /// the frames in between are lost
    Outage,
    /// The resolution changes at the middle GOP boundary (new parameter sets)
    Reschange,
    /// The link drops to --congestion-kbps for --congestion-s (host time) from the middle:
    /// the camera buffers and arrivals fall ~14 s behind, then catch up, like the 17 s and
    /// 7 s episodes of the 2026-10-11 10-minute capture. The rate is chosen per unit when
    /// it departs
    Congestion,
}

#[derive(clap::Args, Clone, Debug)]
pub struct Opts {
    /// Output file, or the output directory with --standard
    #[arg(long, short)]
    out: PathBuf,
    #[arg(long, value_enum, default_value_t = Codec::H265)]
    codec: Codec,
    #[arg(long, value_enum, default_value_t = Variant::Plain)]
    variant: Variant,
    /// Duration in seconds (rounded down to whole GOPs)
    #[arg(long, default_value_t = 20)]
    secs: u32,
    #[arg(long, default_value_t = 640)]
    width: u32,
    #[arg(long, default_value_t = 360)]
    height: u32,
    /// First `cam_us` value
    #[arg(long)]
    cam_us_start: Option<u32>,
    #[arg(long, default_value_t = 10)]
    outage_s: u32,
    #[arg(long, default_value_t = 1900)]
    prebuffer_ms: u64,
    #[arg(long, default_value_t = 4600)]
    link_kbps: u64,
    #[arg(long, default_value_t = 20)]
    congestion_s: u64,
    #[arg(long, default_value_t = 800)]
    congestion_kbps: u64,
    #[arg(long, default_value_t = 400)]
    irap_kb: u64,
    #[arg(long, default_value_t = 12)]
    p_kb: u64,
    #[arg(long, default_value_t = 600)]
    stall_ms: u64,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// ffmpeg binary (default: $NEOLINK_FFMPEG, then ffmpeg on PATH)
    #[arg(long)]
    ffmpeg: Option<PathBuf>,
    /// Generate the standard CI set into the --out directory and write SHA256SUMS
    #[arg(long)]
    standard: bool,
    /// With --standard: compare against an existing SHA256SUMS instead of writing it
    #[arg(long, requires = "standard")]
    check: Option<PathBuf>,
}

/// The fixtures CI generates and the tests use
fn standard_set(base: &Opts) -> Vec<(&'static str, Opts)> {
    let mk = |codec, variant, secs| Opts {
        codec,
        variant,
        secs,
        standard: false,
        check: None,
        ..base.clone()
    };
    vec![
        ("plain_h265.bcm", mk(Codec::H265, Variant::Plain, 20)),
        ("plain_h264.bcm", mk(Codec::H264, Variant::Plain, 20)),
        ("wrap_h265.bcm", mk(Codec::H265, Variant::Wrap, 30)),
        ("reset_h264.bcm", mk(Codec::H264, Variant::Reset, 20)),
        ("outage_h265.bcm", mk(Codec::H265, Variant::Outage, 30)),
        (
            "reschange_h264.bcm",
            mk(Codec::H264, Variant::Reschange, 20),
        ),
        (
            "congestion_h265.bcm",
            mk(Codec::H265, Variant::Congestion, 120),
        ),
    ]
}

pub fn run(opts: Opts) -> Result<()> {
    if !opts.standard {
        let recs = generate(&opts)?;
        write(&opts.out, &recs)?;
        println!("{}: {} records", opts.out.display(), recs.len());
        return Ok(());
    }
    std::fs::create_dir_all(&opts.out)?;
    let mut sums = String::new();
    for (name, o) in standard_set(&opts) {
        let path = opts.out.join(name);
        let recs = generate(&o).with_context(|| name.to_string())?;
        write(&path, &recs)?;
        let hash = Sha256::digest(std::fs::read(&path)?);
        let hex: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
        sums.push_str(&format!("{}  {}\n", hex, name));
        println!("{}: {} records", path.display(), recs.len());
    }
    match &opts.check {
        None => std::fs::write(opts.out.join("SHA256SUMS"), &sums)?,
        Some(expected) => {
            let expected = std::fs::read_to_string(expected)?;
            if expected != sums {
                bail!(
                    "fixtures differ from {}:\nexpected:\n{}got:\n{}",
                    opts.check.as_ref().unwrap().display(),
                    expected,
                    sums
                );
            }
            println!("fixtures match");
        }
    }
    Ok(())
}

fn write(path: &Path, recs: &[Record]) -> Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    for r in recs {
        r.write_to(&mut f)?;
    }
    Ok(())
}

fn ffmpeg_bin(opts: &Opts) -> PathBuf {
    opts.ffmpeg
        .clone()
        .or_else(|| std::env::var_os("NEOLINK_FFMPEG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

/// The flash/beep window: 0.2 s starting 0.5 s into every 2 s
const FLASH: &str = "lt(mod(t+1.5\\,2)\\,0.2)";

fn encode_video(opts: &Opts, secs: u32, width: u32, height: u32) -> Result<Vec<AccessUnit>> {
    let src = format!(
        "testsrc2=size={}x{}:rate={},drawbox=color=white:t=fill:enable='{}'",
        width, height, FPS, FLASH
    );
    let mut cmd = Command::new(ffmpeg_bin(opts));
    cmd.args([
        "-v",
        "error",
        "-nostdin",
        "-cpuflags",
        "0",
        "-sws_flags",
        "+bitexact",
    ])
    .args(["-f", "lavfi", "-i", &src])
    .args(["-t", &secs.to_string(), "-pix_fmt", "yuv420p"])
    .args(["-flags", "+bitexact", "-fflags", "+bitexact"]);
    let h265 = opts.codec == Codec::H265;
    if h265 {
        cmd.args(["-c:v", "libx265", "-preset", "ultrafast", "-b:v", "300k"])
            .args([
                "-x265-params",
                "keyint=30:min-keyint=30:scenecut=0:bframes=0:repeat-headers=1:open-gop=0:\
                 pools=none:frame-threads=1:wpp=0:info=0:asm=none:log-level=error",
            ])
            .args(["-f", "hevc", "pipe:1"]);
    } else {
        cmd.args(["-c:v", "libx264", "-preset", "ultrafast", "-b:v", "300k"])
            .args([
                "-g",
                "30",
                "-keyint_min",
                "30",
                "-sc_threshold",
                "0",
                "-bf",
                "0",
            ])
            .args([
                "-threads",
                "1",
                "-x264-params",
                "repeat-headers=1:cpu-independent=1",
            ])
            .args([
                "-bsf:v",
                "filter_units=remove_types=6",
                "-f",
                "h264",
                "pipe:1",
            ]);
    }
    let out = cmd.output().context("running ffmpeg")?;
    ensure!(
        out.status.success(),
        "ffmpeg video failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let aus = split_access_units(&out.stdout, h265);
    ensure!(
        aus.len() == (secs * FPS) as usize,
        "expected {} video AUs, got {}",
        secs * FPS,
        aus.len()
    );
    for (i, au) in aus.iter().enumerate() {
        ensure!(
            au.irap == (i % GOP == 0),
            "AU {} has unexpected IRAP={}",
            i,
            au.irap
        );
    }
    Ok(aus)
}

fn encode_audio(opts: &Opts, secs: u32) -> Result<Vec<Vec<u8>>> {
    let src = format!(
        "aevalsrc='if({}\\,0.5*sin(2*PI*1000*t)\\,0)':s={}:c=mono",
        FLASH, AUDIO_RATE
    );
    let out = Command::new(ffmpeg_bin(opts))
        .args([
            "-v",
            "error",
            "-nostdin",
            "-cpuflags",
            "0",
            "-sws_flags",
            "+bitexact",
        ])
        .args(["-f", "lavfi", "-i", &src])
        .args(["-t", &secs.to_string()])
        .args(["-flags", "+bitexact", "-fflags", "+bitexact"])
        .args(["-c:a", "aac", "-b:a", "32k", "-f", "adts", "pipe:1"])
        .output()
        .context("running ffmpeg")?;
    ensure!(
        out.status.success(),
        "ffmpeg audio failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    split_adts(&out.stdout)
}

/// splitmix64, so the schedule never depends on a crate's RNG algorithm
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}

/// A unit in camera send order, before scheduling
struct Unit {
    /// Media time, µs from the first frame
    media_us: u64,
    kind: Kind,
    cam_us: u32,
    cam_time: u32,
    data: Vec<u8>,
    /// Lost in an outage: not sent
    lost: bool,
    /// Send a Restarted record and resume after an outage before this unit
    restart_before: bool,
}

pub fn generate(opts: &Opts) -> Result<Vec<Record>> {
    let gops = (opts.secs as usize * FPS as usize) / GOP;
    ensure!(gops >= 2, "need at least 2 GOPs");
    let secs = (gops * GOP) as u32 / FPS;
    let mid_frame = (gops / 2) * GOP;

    let video = if opts.variant == Variant::Reschange {
        let first = encode_video(opts, mid_frame as u32 / FPS, opts.width, opts.height)?;
        let second = encode_video(opts, secs - mid_frame as u32 / FPS, 896, 512)?;
        first.into_iter().chain(second).collect()
    } else {
        encode_video(opts, secs, opts.width, opts.height)?
    };
    // The aac encoder's first AU holds its 1024 priming samples: drop it so that AU j
    // starts at media time j × 64 ms, and stop with the video
    let audio: Vec<Vec<u8>> = encode_audio(opts, secs)?
        .into_iter()
        .skip(1)
        .take((secs as u64 * 1_000_000 / AUDIO_AU_US) as usize)
        .collect();

    let cam_us_start = opts.cam_us_start.unwrap_or(match opts.variant {
        Variant::Wrap => u32::MAX - 10_000_000 + 1,
        _ => 5_000_000,
    });
    let frame_us = |i: usize| (i as u64 * 1_000_000) / FPS as u64;
    // Outage: media in [mid, resume) is lost; resume is the next GOP boundary
    let resume_frame = mid_frame + ((opts.outage_s * FPS) as usize).div_ceil(GOP) * GOP;
    ensure!(
        opts.variant != Variant::Outage || resume_frame < gops * GOP,
        "the outage ends after the media: use a longer --secs or a shorter --outage-s"
    );
    let lost = |media_us: u64| {
        opts.variant == Variant::Outage
            && media_us >= frame_us(mid_frame)
            && media_us < frame_us(resume_frame)
    };

    let mut units = vec![];
    for (i, au) in video.into_iter().enumerate() {
        let media_us = frame_us(i);
        let cam_us = if opts.variant == Variant::Reset && i >= mid_frame {
            1_000_000 + media_us - frame_us(mid_frame)
        } else {
            cam_us_start as u64 + media_us
        };
        units.push(Unit {
            media_us,
            kind: if au.irap { Kind::Iframe } else { Kind::Pframe },
            cam_us: cam_us as u32,
            cam_time: if au.irap {
                EPOCH + (media_us / 1_000_000) as u32
            } else {
                0
            },
            data: au.data,
            lost: lost(media_us),
            restart_before: opts.variant == Variant::Outage && i == resume_frame,
        });
    }
    for (j, frame) in audio.into_iter().enumerate() {
        let media_us = j as u64 * AUDIO_AU_US;
        units.push(Unit {
            media_us,
            kind: Kind::Aac,
            cam_us: 0,
            cam_time: 0,
            data: frame,
            lost: lost(media_us),
            restart_before: false,
        });
    }
    // Camera send order: by media time, video before audio at equal times
    units.sort_by_key(|u| (u.media_us, u.kind == Kind::Aac));

    // Arrival schedule
    let mut rng = Rng(opts.seed);
    let kbps_to_bytes_per_us = |kbps: u64| kbps as f64 * 1000.0 / 8.0 / 1e6;
    // The congested window is in host time, starting when the middle frame is ready
    let congestion_start = frame_us(mid_frame).saturating_sub(opts.prebuffer_ms * 1000);
    let congested = |host_us: u64| {
        opts.variant == Variant::Congestion
            && host_us >= congestion_start
            && host_us < congestion_start + opts.congestion_s * 1_000_000
    };
    let mut last_arrival = 0u64;
    let mut out = vec![];
    for u in units.into_iter().filter(|u| !u.lost) {
        let ready = u.media_us.saturating_sub(opts.prebuffer_ms * 1000);
        if u.restart_before {
            // The reconnect: the camera starts again with a backlog
            last_arrival = last_arrival.max(ready);
            out.push(Record {
                host_us: last_arrival,
                kind: Kind::Restarted,
                h265: false,
                cam_us: 0,
                cam_time: 0,
                data: vec![],
            });
        }
        let sched_bytes = match u.kind {
            Kind::Iframe => opts.irap_kb * 1000,
            Kind::Pframe => opts.p_kb * 1000,
            _ => u.data.len() as u64,
        };
        let mut depart = ready.max(last_arrival);
        if u.kind == Kind::Iframe {
            depart += rng.below(opts.stall_ms * 1000);
        }
        let rate = kbps_to_bytes_per_us(if congested(depart) {
            opts.congestion_kbps
        } else {
            opts.link_kbps
        });
        let arrival = depart + (sched_bytes as f64 / rate) as u64;
        last_arrival = arrival;
        out.push(Record {
            host_us: arrival,
            kind: u.kind,
            h265: u.kind.is_video() && opts.codec == Codec::H265,
            cam_us: u.cam_us,
            cam_time: u.cam_time,
            data: u.data,
        });
    }
    Ok(out)
}
