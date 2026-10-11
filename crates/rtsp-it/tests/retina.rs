//! retina against the native server (SPEC A6, A17 retina part, A22 SR part)

use bytes::Bytes;
use futures::StreamExt;
use neolink_rtsp::codec::aac::adts_access_units;
use neolink_rtsp::codec::annexb::{allowed, param_kind, split_nals};
use neolink_rtsp::hub::task::{spawn, HubConfig, HubHandle};
use neolink_rtsp::rtsp::auth::{Allowed, Auth, Schemes, User};
use neolink_rtsp::rtsp::route::{aliases, MountTarget, Routes, StreamKind};
use neolink_rtsp::rtsp::server::{Provider, Server, ServerConfig};
use neolink_rtsp::source::{parse_bcm, FixtureSource, FixtureUnit, SourceUnit};
use retina::client::{
    Credentials, PlayOptions, SessionOptions, SetupOptions, TcpTransportOptions, Transport,
};
use retina::codec::{CodecItem, FrameFormat, ParameterSetInsertion};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

struct TestProvider {
    hubs: HashMap<MountTarget, HubHandle>,
    allowed: Allowed,
}

impl Provider for TestProvider {
    fn hub(&self, target: &MountTarget) -> Option<HubHandle> {
        self.hubs.get(target).cloned()
    }
    fn allowed(&self, _camera: &str) -> Allowed {
        self.allowed.clone()
    }
}

fn fixture(name: &str) -> Vec<FixtureUnit> {
    let path = format!("{}/../../fixtures/synth/{name}", env!("CARGO_MANIFEST_DIR"));
    let data = std::fs::read(&path)
        .unwrap_or_else(|_| panic!("{path}: run `cargo xtask mkfixture --standard` first"));
    parse_bcm(&data)
}

struct Harness {
    addr: SocketAddr,
    cancel: CancellationToken,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// The server with `file` on argus/main
async fn start(file: &str, users: Vec<User>, schemes: Schemes) -> Harness {
    let _ = env_logger::builder().is_test(true).try_init();
    let source = Arc::new(FixtureSource::new(fixture(file), 1.0, Some(false)));
    let mut hc = HubConfig::new("main");
    hc.battery = false;
    let mut hubs = HashMap::new();
    hubs.insert(
        MountTarget {
            camera: "argus".into(),
            kind: StreamKind::Main,
        },
        spawn(hc, source),
    );
    let have_users = !users.is_empty();
    let routes = Routes::default();
    routes.set(aliases("argus", &[StreamKind::Main]));
    let provider = Arc::new(TestProvider {
        hubs,
        allowed: Allowed::from_config(None, have_users),
    });
    let server = Server::new(
        ServerConfig::default(),
        routes,
        Auth::new(users, schemes),
        provider,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    tokio::spawn(server.serve(listener, cancel.clone()));
    Harness { addr, cancel }
}

#[derive(Default)]
struct Received {
    /// Per access unit, its NALs (Annex B split)
    video: Vec<(i64, bool, Vec<Bytes>)>,
    /// Raw AAC access units with their timestamps
    audio: Vec<(i64, Bytes)>,
    /// (stream index, NTP timestamp, extended RTP timestamp, clock rate)
    srs: Vec<(usize, u64, i64, u32)>,
    video_stream: usize,
}

/// Play argus/main with retina over TCP for `secs`; frames byte-exact (Annex B, no inserted
/// parameter sets, raw AAC)
async fn play(
    h: &Harness,
    creds: Option<Credentials>,
    secs: u64,
    max_jump: Option<u32>,
) -> Received {
    let url = url::Url::parse(&format!("rtsp://{}/argus/main", h.addr)).unwrap();
    let mut session = retina::client::Session::describe(
        url,
        SessionOptions::default()
            .creds(creds)
            .user_agent("neolink-rtsp-it".into()),
    )
    .await
    .expect("DESCRIBE");
    let format = FrameFormat {
        h26x_framing: retina::codec::h26x::Framing::AnnexB,
        parameter_set_insertion: ParameterSetInsertion::Never,
        aac_framing: retina::codec::aac::Framing::Raw,
    };
    let mut video_stream = 0;
    for i in 0..session.streams().len() {
        if session.streams()[i].media() == "video" {
            video_stream = i;
        }
        session
            .setup(
                i,
                SetupOptions::default()
                    .transport(Transport::Tcp(TcpTransportOptions::default()))
                    .frame_format(format),
            )
            .await
            .expect("SETUP");
    }
    let mut opts = PlayOptions::default();
    if let Some(s) = max_jump {
        opts = opts.enforce_timestamps_with_max_jump_secs(std::num::NonZeroU32::new(s).unwrap());
    }
    let playing = session.play(opts).await.expect("PLAY");
    let mut demuxed = playing.demuxed().expect("demuxed");
    let mut out = Received {
        video_stream,
        ..Default::default()
    };
    let end = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let item = tokio::select! {
            i = demuxed.next() => i,
            _ = tokio::time::sleep_until(end) => break,
        };
        match item {
            None => break,
            Some(Err(e)) => panic!("retina: {e}"),
            Some(Ok(CodecItem::VideoFrame(f))) => {
                let ts = f.timestamp().elapsed();
                let rap = f.is_random_access_point();
                out.video
                    .push((ts, rap, split_nals(&Bytes::from(f.into_data()))));
            }
            Some(Ok(CodecItem::AudioFrame(f))) => {
                out.audio
                    .push((f.timestamp().elapsed(), Bytes::copy_from_slice(f.data())));
            }
            Some(Ok(CodecItem::Rtcp(r))) => {
                if let (Some(ts), Some(pkt)) = (r.rtp_timestamp(), r.pkts().next()) {
                    if let Ok(Some(sr)) = pkt.as_sender_report() {
                        out.srs.push((
                            r.stream_id(),
                            sr.ntp_timestamp().0,
                            ts.timestamp(),
                            ts.clock_rate().get(),
                        ));
                    }
                }
            }
            Some(Ok(_)) => {}
        }
    }
    out
}

/// What the server should send: whitelisted NALs without in-band parameter sets (the
/// default `parameter_sets = strip`), and raw AAC access units
fn expected(units: &[FixtureUnit]) -> (Vec<Vec<Bytes>>, Vec<Bytes>) {
    let (mut video, mut audio) = (vec![], vec![]);
    for u in units {
        match &u.unit {
            SourceUnit::Video { codec, annexb, .. } => {
                let nals: Vec<Bytes> = split_nals(annexb)
                    .into_iter()
                    .filter(|n| allowed(*codec, n) && param_kind(*codec, n).is_none())
                    .collect();
                if !nals.is_empty() {
                    video.push(nals);
                }
            }
            SourceUnit::Aac { adts } => {
                if let Ok((_, aus)) = adts_access_units(adts) {
                    audio.extend(aus);
                }
            }
            _ => {}
        }
    }
    (video, audio)
}

/// `got` is a contiguous run of `want`, from wherever it starts (units can repeat, so
/// every candidate start is tried)
fn assert_contiguous<T: PartialEq + std::fmt::Debug>(what: &str, want: &[T], got: &[T]) {
    assert!(!got.is_empty(), "{what}: nothing received");
    let starts: Vec<usize> = (0..want.len()).filter(|i| want[*i] == got[0]).collect();
    assert!(
        !starts.is_empty(),
        "{what}: the first unit received is not in the input"
    );
    let best = starts
        .iter()
        .map(|s| {
            let n = got
                .iter()
                .zip(&want[*s..])
                .take_while(|(g, w)| g == w)
                .count();
            (n, *s)
        })
        .max()
        .unwrap();
    assert_eq!(
        best.0,
        got.len(),
        "{what}: only {} of {} units match the input from index {}",
        best.0,
        got.len(),
        best.1
    );
}

/// A6: the depacketized media is byte-identical to the input after the R2 filter, with
/// Basic and with Digest
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_a6_byte_identical_basic_and_digest() {
    let units = fixture("plain_h265.bcm");
    let (want_v, want_a) = expected(&units);
    for schemes in [Schemes::Basic, Schemes::Digest] {
        let users = vec![User {
            name: "alice".into(),
            pass: Some("alicepw".into()),
        }];
        let h = start("plain_h265.bcm", users, schemes).await;
        let got = play(
            &h,
            Some(Credentials {
                username: "alice".into(),
                password: "alicepw".into(),
            }),
            8,
            None,
        )
        .await;
        let got_v: Vec<Vec<Bytes>> = got.video.iter().map(|v| v.2.clone()).collect();
        let got_a: Vec<Bytes> = got.audio.iter().map(|a| a.1.clone()).collect();
        assert!(
            got_v.len() > 80,
            "{schemes:?}: {} video frames",
            got_v.len()
        );
        assert!(
            got_a.len() > 60,
            "{schemes:?}: {} audio frames",
            got_a.len()
        );
        assert!(
            got.video[0].1,
            "{schemes:?}: the first frame is a random access point"
        );
        // A late joiner first gets the last released IRAP (with EOS only after a CRA/BLA,
        // not after the fixture's IDRs), then live media from the next IRAP (DESIGN §4.6)
        let first = got_v[0].clone();
        assert!(
            first.iter().all(|n| (n[0] >> 1) & 0x3f != 36),
            "{schemes:?}: no EOS after an IDR"
        );
        assert!(
            want_v.contains(&first),
            "{schemes:?}: the cached IRAP is from the input"
        );
        assert_contiguous(&format!("{schemes:?} video"), &want_v, &got_v[1..]);
        assert_contiguous(&format!("{schemes:?} audio"), &want_a, &got_a);
    }
}

/// A17 (retina part): u32 wrap and a counter reset give a timeline retina accepts, with
/// no jump over 1 s
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_a17_retina_accepts_wrap_and_reset() {
    for file in ["wrap_h265.bcm", "reset_h264.bcm"] {
        let h = start(file, vec![], Schemes::Both).await;
        // retina's default timestamp policy; the bound below is from the first live frame on
        let got = play(&h, None, 16, None).await;
        assert!(
            got.video.len() > 150,
            "{file}: {} video frames",
            got.video.len()
        );
        for w in got.video[1..].windows(2) {
            let d = w[1].0 - w[0].0;
            assert!(d > 0 && d <= 90_000, "{file}: video step {d} ticks");
        }
        for w in got.audio.windows(2) {
            let d = w[1].0 - w[0].0;
            assert!(d > 0 && d <= 16_000, "{file}: audio step {d} ticks");
        }
    }
}

/// A22 (SR part): the SR NTP↔RTP mapping is the same wire clock on both tracks
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_a22_sr_mapping_identical_across_tracks() {
    let h = start("plain_h265.bcm", vec![], Schemes::Both).await;
    let got = play(&h, None, 9, None).await;
    let v = got.video_stream;
    let video: Vec<_> = got.srs.iter().filter(|s| s.0 == v).collect();
    let audio: Vec<_> = got.srs.iter().filter(|s| s.0 != v).collect();
    assert!(video.len() >= 3, "{} video SRs", video.len());
    assert!(audio.len() >= 2, "{} audio SRs", audio.len());
    let ntp_s = |n: u64| (n >> 32) as f64 + (n & 0xffff_ffff) as f64 / 4_294_967_296.0;
    // Each track's mapping advances at its clock rate, without drift
    for (name, srs) in [("video", &video), ("audio", &audio)] {
        for w in srs.windows(2) {
            let dn = ntp_s(w[1].1) - ntp_s(w[0].1);
            let dr = (w[1].2 - w[0].2) as f64 / w[0].3 as f64;
            assert!((dn - dr).abs() < 0.001, "{name}: NTP {dn} s vs RTP {dr} s");
        }
    }
    // Both tracks' SRs at one emission share the NTP time
    for a in &audio {
        assert!(
            video.iter().any(|v| v.1 == a.1),
            "audio SR NTP {} has no video SR at the same instant",
            a.1
        );
    }
}
