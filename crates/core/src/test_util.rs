//! A scripted Baichuan camera for tests (feature `test-util`)
//!
//! [`FakeCamera`] listens on a local TCP port and speaks enough of the camera side of the
//! protocol for neolink to log in, read abilities, version, UID, stream info and battery,
//! ping, set the time, stream `.bcm` fixtures and receive motion events. It runs
//! unencrypted.
//!
//! Every connection, login, logout and message is counted ([`FakeStats`]), so power tests
//! can assert that the camera was never contacted. Faults can be injected: dropping every
//! connection, going unreachable (connections are accepted and closed at once, like a
//! sleeping camera), and rejecting the first N battery reads with 400 (the Argus does
//! this right after a wake).
//!
//! ```no_run
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! use neolink_core::test_util::{FakeCamera, FakeCameraConfig};
//! let cam = FakeCamera::start(FakeCameraConfig::default()).await.unwrap();
//! let opts = cam.camera_opts("fake");
//! let camera = neolink_core::bc_protocol::BcCamera::new(&opts).await.unwrap();
//! camera.login().await.unwrap();
//! assert_eq!(cam.stats().logins, 1);
//! # })
//! ```

use crate::bc::{model::*, xml::*};
use crate::bc_protocol::{BcCameraOpt, ConnectionProtocol, Credentials, DiscoveryMethods};
use crate::bcmedia::model::*;
use crate::fixture::{Kind, Record};
use bytes::{Buf, BytesMut};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// How the fake camera behaves
#[derive(Clone, Debug)]
pub struct FakeCameraConfig {
    /// Accepted username
    pub username: String,
    /// Accepted password
    pub password: String,
    /// Reported UID
    pub uid: String,
    /// Reported model
    pub model: String,
    /// Battery percentage; `None` behaves like a mains camera (battery reads get 400)
    pub battery_percent: Option<u32>,
    /// Reject this many battery reads per connection with 400 before answering
    pub battery_busy_reads: u32,
    /// Media for `mainStream`
    pub main: Arc<Vec<Record>>,
    /// Media for `subStream`
    pub sub: Arc<Vec<Record>>,
    /// Replay speed: 1.0 is real time (by `host_us`), 0 sends as fast as possible
    pub speed: f64,
    /// Restart the media from the beginning when it ends
    pub loop_media: bool,
    /// Largest binary payload per Baichuan message
    pub chunk_size: usize,
    /// JPEG returned by snapshot requests; `None` answers them with 400
    pub snapshot: Option<Arc<Vec<u8>>>,
}

impl Default for FakeCameraConfig {
    fn default() -> Self {
        Self {
            username: "admin".to_string(),
            password: "fakepass".to_string(),
            uid: "FAKEUID0000000001".to_string(),
            model: "Argus PT Ultra (fake)".to_string(),
            battery_percent: Some(80),
            battery_busy_reads: 0,
            main: Arc::new(vec![]),
            sub: Arc::new(vec![]),
            speed: 1.0,
            loop_media: false,
            chunk_size: 40_000,
            snapshot: Some(Arc::new(b"\xff\xd8fake jpeg\xff\xd9".to_vec())),
        }
    }
}

/// Counters of everything the fake camera saw
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct FakeStats {
    /// TCP connections accepted (including those closed while unreachable)
    pub connections: u32,
    /// Connections open now
    pub open_connections: u32,
    /// Successful modern logins
    pub logins: u32,
    /// Failed modern logins
    pub failed_logins: u32,
    /// Logout messages
    pub logouts: u32,
    /// Video start requests
    pub video_starts: u32,
    /// Video stop requests
    pub video_stops: u32,
    /// Media records sent
    pub media_sent: u64,
    /// Snapshots sent
    pub snapshots: u32,
    /// Messages received, by msg_id
    pub messages: BTreeMap<u32, u32>,
}

struct Shared {
    config: FakeCameraConfig,
    stats: Mutex<FakeStats>,
    unreachable: AtomicBool,
    events: broadcast::Sender<Event>,
}

#[derive(Clone, Debug)]
enum Event {
    DropAll,
    Motion(bool),
}

/// A scripted camera on a local TCP port, see the module docs
pub struct FakeCamera {
    addr: SocketAddr,
    shared: Arc<Shared>,
    cancel: CancellationToken,
}

impl FakeCamera {
    /// Start listening on 127.0.0.1 on a free port
    pub async fn start(config: FakeCameraConfig) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (events, _) = broadcast::channel(16);
        let shared = Arc::new(Shared {
            config,
            stats: Mutex::new(FakeStats::default()),
            unreachable: AtomicBool::new(false),
            events,
        });
        let cancel = CancellationToken::new();
        let (thread_shared, thread_cancel) = (shared.clone(), cancel.clone());
        tokio::spawn(async move {
            loop {
                let (stream, _) = tokio::select! {
                    _ = thread_cancel.cancelled() => break,
                    v = listener.accept() => match v {
                        Ok(v) => v,
                        Err(_) => continue,
                    },
                };
                thread_shared.stats.lock().unwrap().connections += 1;
                if thread_shared.unreachable.load(Ordering::SeqCst) {
                    drop(stream);
                    continue;
                }
                let (shared, cancel) = (thread_shared.clone(), thread_cancel.child_token());
                tokio::spawn(async move {
                    shared.stats.lock().unwrap().open_connections += 1;
                    let _ = Conn::new(shared.clone(), stream).run(cancel).await;
                    shared.stats.lock().unwrap().open_connections -= 1;
                });
            }
        });
        Ok(Self {
            addr,
            shared,
            cancel,
        })
    }

    /// The address to connect to
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Camera options that reach this fake over TCP
    pub fn camera_opts(&self, name: &str) -> BcCameraOpt {
        BcCameraOpt {
            name: name.to_string(),
            channel_id: 0,
            addrs: vec![self.addr.ip()],
            port: Some(self.addr.port()),
            uid: None,
            protocol: ConnectionProtocol::Tcp,
            discovery: DiscoveryMethods::None,
            max_discovery_retries: 0,
            credentials: Credentials {
                username: self.shared.config.username.clone(),
                password: Some(self.shared.config.password.clone()),
            },
            debug: false,
        }
    }

    /// A snapshot of the counters
    pub fn stats(&self) -> FakeStats {
        self.shared.stats.lock().unwrap().clone()
    }

    /// Close every open connection, as if the camera rebooted or Wi-Fi dropped
    pub fn drop_connections(&self) {
        let _ = self.shared.events.send(Event::DropAll);
    }

    /// While unreachable, connections are accepted (and counted) then closed at once
    pub fn set_unreachable(&self, unreachable: bool) {
        self.shared.unreachable.store(unreachable, Ordering::SeqCst);
        if unreachable {
            self.drop_connections();
        }
    }

    /// Send a motion start (`true`) or stop event to every client that asked for motion
    pub fn motion(&self, active: bool) {
        let _ = self.shared.events.send(Event::Motion(active));
    }
}

impl Drop for FakeCamera {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

const NONCE: &str = "0123456789abcdef";

fn md5_hex_truncated(input: &str) -> String {
    let mut md5 = format!("{:X}", md5::compute(input));
    md5.truncate(31);
    md5
}

struct Conn {
    shared: Arc<Shared>,
    stream: TcpStream,
    context: BcContext,
    battery_reads: u32,
    motion_msg_num: Option<u16>,
    videos: BTreeMap<u16, CancellationToken>,
}

impl Conn {
    fn new(shared: Arc<Shared>, stream: TcpStream) -> Self {
        Self {
            shared,
            stream,
            context: BcContext::new_with_encryption(EncryptionProtocol::Unencrypted),
            battery_reads: 0,
            motion_msg_num: None,
            videos: BTreeMap::new(),
        }
    }

    async fn run(mut self, cancel: CancellationToken) -> std::io::Result<()> {
        let mut events = self.shared.events.subscribe();
        let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);
        let mut buf = BytesMut::new();
        let mut read = vec![0u8; 64 * 1024];
        let result = loop {
            tokio::select! {
                _ = cancel.cancelled() => break Ok(()),
                ev = events.recv() => match ev {
                    Ok(Event::DropAll) => break Ok(()),
                    Ok(Event::Motion(active)) => {
                        if let Some(msg_num) = self.motion_msg_num {
                            let bc = motion_event(msg_num, active);
                            if let Err(e) = self.write(&bc).await {
                                break Err(e);
                            }
                        }
                    }
                    Err(_) => {}
                },
                Some(bytes) = out_rx.recv() => {
                    if let Err(e) = self.stream.write_all(&bytes).await {
                        break Err(e);
                    }
                }
                n = self.stream.read(&mut read) => {
                    let n = match n {
                        Ok(0) => break Ok(()),
                        Ok(n) => n,
                        Err(e) => break Err(e),
                    };
                    buf.extend_from_slice(&read[..n]);
                    let mut failed = None;
                    while let Some(msg) = self.next_message(&mut buf) {
                        if let Err(e) = self.handle(msg, &out_tx, &cancel).await {
                            failed = Some(e);
                            break;
                        }
                    }
                    if let Some(e) = failed {
                        break Err(e);
                    }
                }
            }
        };
        for (_, v) in std::mem::take(&mut self.videos) {
            v.cancel();
        }
        result
    }

    /// Parse one client message. Legacy (class 0x6514) messages are only identified by
    /// their header; their body is skipped whatever its length.
    fn next_message(&self, buf: &mut BytesMut) -> Option<Bc> {
        if buf.len() < 20 {
            return None;
        }
        let u32_at = |i: usize| u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
        let msg_id = u32_at(4);
        let body_len = u32_at(8) as usize;
        let class = u16::from_le_bytes([buf[18], buf[19]]);
        if class == 0x6514 {
            if buf.len() < 20 + body_len {
                return None;
            }
            let meta = BcMeta {
                msg_id,
                channel_id: buf[12],
                stream_type: buf[13],
                msg_num: u16::from_le_bytes([buf[14], buf[15]]),
                response_code: u16::from_le_bytes([buf[16], buf[17]]),
                class,
            };
            buf.advance(20 + body_len);
            return Some(Bc {
                meta,
                body: BcBody::LegacyMsg(LegacyMsg::UnknownMsg),
            });
        }
        Bc::deserialize(&self.context, buf).ok()
    }

    async fn write(&mut self, bc: &Bc) -> std::io::Result<()> {
        let bytes = bc
            .serialize(vec![], &EncryptionProtocol::Unencrypted)
            .map_err(|e| std::io::Error::other(format!("{:?}", e)))?;
        self.stream.write_all(&bytes).await
    }

    async fn reply(&mut self, req: &BcMeta, code: u16, xml: Option<BcXml>) -> std::io::Result<()> {
        let meta = BcMeta {
            msg_id: req.msg_id,
            channel_id: req.channel_id,
            msg_num: req.msg_num,
            stream_type: req.stream_type,
            response_code: code,
            class: 0x0000,
        };
        let bc = match xml {
            Some(xml) => Bc::new_from_xml(meta, xml),
            None => Bc::new_from_meta(meta),
        };
        self.write(&bc).await
    }

    async fn handle(
        &mut self,
        msg: Bc,
        out: &mpsc::Sender<Vec<u8>>,
        cancel: &CancellationToken,
    ) -> std::io::Result<()> {
        let meta = &msg.meta;
        *self
            .shared
            .stats
            .lock()
            .unwrap()
            .messages
            .entry(meta.msg_id)
            .or_default() += 1;
        let xml = match &msg.body {
            BcBody::ModernMsg(ModernMsg {
                payload: Some(BcPayloads::BcXml(xml)),
                ..
            }) => Some(xml),
            _ => None,
        };
        let config = self.shared.config.clone();
        match meta.msg_id {
            MSG_ID_LOGIN if meta.class == 0x6514 => {
                // Legacy login (or the discovery probe): offer an unencrypted upgrade
                let reply = Bc::new_from_xml(
                    BcMeta {
                        msg_id: MSG_ID_LOGIN,
                        channel_id: meta.channel_id,
                        msg_num: meta.msg_num,
                        stream_type: 0,
                        response_code: 0xdd00,
                        class: 0x6614,
                    },
                    BcXml {
                        encryption: Some(Encryption {
                            version: "1.1".to_string(),
                            type_: "md5".to_string(),
                            nonce: NONCE.to_string(),
                        }),
                        ..Default::default()
                    },
                );
                self.write(&reply).await
            }
            MSG_ID_LOGIN => {
                let ok = xml.and_then(|x| x.login_user.as_ref()).is_some_and(|u| {
                    u.user_name == md5_hex_truncated(&format!("{}{}", config.username, NONCE))
                        && u.password == md5_hex_truncated(&format!("{}{}", config.password, NONCE))
                });
                if ok {
                    self.shared.stats.lock().unwrap().logins += 1;
                    let info = BcXml {
                        device_info: Some(DeviceInfo {
                            version: Some("1.1".to_string()),
                            resolution: Some(Resolution {
                                name: "3840*2160".to_string(),
                                width: 3840,
                                height: 2160,
                            }),
                        }),
                        ..Default::default()
                    };
                    self.reply(meta, 200, Some(info)).await
                } else {
                    self.shared.stats.lock().unwrap().failed_logins += 1;
                    self.reply(meta, 400, None).await
                }
            }
            MSG_ID_LOGOUT => {
                self.shared.stats.lock().unwrap().logouts += 1;
                self.reply(meta, 200, None).await
            }
            MSG_ID_ABILITY_INFO => {
                let token = |v: &str| {
                    Some(AbilityInfoToken {
                        sub_module: vec![AbilityInfoSubModule {
                            channel_id: Some(0),
                            ability_value: v.to_string(),
                        }],
                    })
                };
                let xml = BcXml {
                    ability_info: Some(AbilityInfo {
                        username: config.username.clone(),
                        system: token("general_rw, battery_ro, version_ro, uid_ro"),
                        alarm: token("motion_rw, pir_rw"),
                        video: token("preview_rw, streamTable_ro"),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                self.reply(meta, 200, Some(xml)).await
            }
            MSG_ID_PING => {
                let xml = BcXml {
                    link_type: Some(LinkType {
                        link_type: "LAN".to_string(),
                    }),
                    ..Default::default()
                };
                self.reply(meta, 200, Some(xml)).await
            }
            MSG_ID_VERSION => {
                let xml = BcXml {
                    version_info: Some(VersionInfo {
                        name: "fake".to_string(),
                        model: Some(config.model.clone()),
                        firmwareVersion: "v3.0.0.0000_00000000".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                self.reply(meta, 200, Some(xml)).await
            }
            MSG_ID_UID => {
                let xml = BcXml {
                    uid: Some(Uid {
                        version: "1.1".to_string(),
                        uid: config.uid.clone(),
                    }),
                    ..Default::default()
                };
                self.reply(meta, 200, Some(xml)).await
            }
            MSG_ID_STREAM_INFO_LIST => {
                let table = |name: &str, width, height| EncodeTable {
                    name: name.to_string(),
                    resolution: StreamResolution { width, height },
                    default_framerate: 15,
                    default_bitrate: 2048,
                    framerate_table: "15,10,5".to_string(),
                    bitrate_table: "1024,2048".to_string(),
                };
                let xml = BcXml {
                    stream_info_list: Some(StreamInfoList {
                        stream_infos: vec![StreamInfo {
                            channel_bits: 1,
                            encode_tables: vec![
                                table("mainStream", 3840, 2160),
                                table("subStream", 896, 512),
                            ],
                        }],
                    }),
                    ..Default::default()
                };
                self.reply(meta, 200, Some(xml)).await
            }
            MSG_ID_BATTERY_INFO => {
                self.battery_reads += 1;
                match config.battery_percent {
                    Some(percent) if self.battery_reads > config.battery_busy_reads => {
                        let xml = BcXml {
                            battery_info: Some(BatteryInfo {
                                channel_id: 0,
                                charge_status: "none".to_string(),
                                adapter_status: "solarPanel".to_string(),
                                voltage: 3700,
                                current: -500,
                                temperature: 21,
                                battery_percent: percent,
                                low_power: 0,
                                battery_version: 2,
                            }),
                            ..Default::default()
                        };
                        self.reply(meta, 200, Some(xml)).await
                    }
                    _ => self.reply(meta, 400, None).await,
                }
            }
            MSG_ID_GET_GENERAL => {
                let xml = BcXml {
                    system_general: Some(SystemGeneral {
                        version: "1.1".to_string(),
                        time_zone: Some(0),
                        year: Some(2026),
                        month: Some(10),
                        day: Some(11),
                        hour: Some(12),
                        minute: Some(0),
                        second: Some(0),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                self.reply(meta, 200, Some(xml)).await
            }
            MSG_ID_SET_GENERAL | MSG_ID_UDP_KEEP_ALIVE => self.reply(meta, 200, None).await,
            MSG_ID_SNAP => match config.snapshot.clone() {
                None => self.reply(meta, 400, None).await,
                Some(jpeg) => {
                    let xml = BcXml {
                        snap: Some(Snap {
                            version: "1.1".to_string(),
                            channel_id: 0,
                            file_name: Some("snap.jpg".to_string()),
                            picture_size: Some(jpeg.len() as u32),
                            ..Default::default()
                        }),
                        ..Default::default()
                    };
                    self.reply(meta, 200, Some(xml)).await?;
                    // Like the camera: the JPEG follows after a moment, on a new msg_num
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    let data_num = meta.msg_num.wrapping_add(1000);
                    let chunks: Vec<&[u8]> = jpeg.chunks(config.chunk_size.max(1)).collect();
                    for (i, chunk) in chunks.iter().enumerate() {
                        let last = i + 1 == chunks.len();
                        let bc = Bc::new(
                            BcMeta {
                                msg_id: MSG_ID_SNAP,
                                channel_id: 0,
                                msg_num: data_num,
                                stream_type: 0,
                                response_code: if last { 201 } else { 200 },
                                class: 0x0000,
                            },
                            Some(Extension {
                                version: "1.1".to_string(),
                                binary_data: Some(1),
                                ..Default::default()
                            }),
                            Some(BcPayloads::Binary(chunk.to_vec())),
                        );
                        self.write(&bc).await?;
                    }
                    self.shared.stats.lock().unwrap().snapshots += 1;
                    Ok(())
                }
            },
            MSG_ID_MOTION_REQUEST => {
                self.motion_msg_num = Some(meta.msg_num);
                self.reply(meta, 200, None).await
            }
            MSG_ID_VIDEO => {
                self.shared.stats.lock().unwrap().video_starts += 1;
                let main = xml
                    .and_then(|x| x.preview.as_ref())
                    .and_then(|p| p.stream_type.as_deref())
                    != Some("subStream");
                let media = if main {
                    config.main.clone()
                } else {
                    config.sub.clone()
                };
                self.reply(meta, 200, None).await?;
                let token = cancel.child_token();
                if let Some(old) = self.videos.insert(meta.msg_num, token.clone()) {
                    old.cancel();
                }
                let (msg_num, stream_type) = (meta.msg_num, meta.stream_type);
                let (out, shared) = (out.clone(), self.shared.clone());
                tokio::spawn(async move {
                    tokio::select! {
                        _ = token.cancelled() => {},
                        _ = replay(shared, media, msg_num, stream_type, out) => {},
                    }
                });
                Ok(())
            }
            MSG_ID_VIDEO_STOP => {
                self.shared.stats.lock().unwrap().video_stops += 1;
                // Like the camera: the stop names its stream by the start's msg_num, and
                // the other streams on the connection go on
                if let Some(v) = self.videos.remove(&meta.msg_num) {
                    v.cancel();
                }
                self.reply(meta, 200, None).await
            }
            _ => self.reply(meta, 400, None).await,
        }
    }
}

fn motion_event(msg_num: u16, active: bool) -> Bc {
    Bc::new_from_xml(
        BcMeta {
            msg_id: MSG_ID_MOTION,
            channel_id: 0,
            msg_num,
            stream_type: 0,
            response_code: 200,
            class: 0x0000,
        },
        BcXml {
            alarm_event_list: Some(AlarmEventList {
                version: "1.1".to_string(),
                alarm_events: vec![AlarmEvent {
                    version: "1.1".to_string(),
                    channel_id: 0,
                    status: if active { "MD" } else { "none" }.to_string(),
                    recording: 0,
                    ..Default::default()
                }],
            }),
            ..Default::default()
        },
    )
}

fn to_bcmedia(rec: &Record) -> Option<BcMedia> {
    let video_type = if rec.h265 {
        VideoType::H265
    } else {
        VideoType::H264
    };
    Some(match rec.kind {
        Kind::Iframe => BcMedia::Iframe(BcMediaIframe {
            video_type,
            microseconds: rec.cam_us,
            time: if rec.cam_time == 0 {
                None
            } else {
                Some(rec.cam_time)
            },
            data: rec.data.clone(),
        }),
        Kind::Pframe => BcMedia::Pframe(BcMediaPframe {
            video_type,
            microseconds: rec.cam_us,
            data: rec.data.clone(),
        }),
        Kind::Aac => BcMedia::Aac(BcMediaAac {
            data: rec.data.clone(),
        }),
        Kind::Adpcm => BcMedia::Adpcm(BcMediaAdpcm {
            data: rec.data.clone(),
        }),
        Kind::InfoV1 | Kind::InfoV2 | Kind::Restarted => return None,
    })
}

async fn replay(
    shared: Arc<Shared>,
    media: Arc<Vec<Record>>,
    msg_num: u16,
    stream_type: u8,
    out: mpsc::Sender<Vec<u8>>,
) {
    let config = &shared.config;
    // Looping like a camera that keeps streaming: each lap moves the arrival clock and the
    // camera µs clock by the media span (the arrival span is shorter: a start-up burst),
    // and audio is topped up to the video's length, so media never outruns wall time
    let period = lap_period(&media);
    let au_us = 1024 * 1_000_000 / 16_000u64;
    let last_aac = media.iter().rev().find(|r| r.kind == Kind::Aac).cloned();
    let mut audio_us = 0u64;
    let start = tokio::time::Instant::now();
    let first_us = media.first().map(|r| r.host_us).unwrap_or(0);
    let mut lap = 0u64;
    loop {
        let mut records: Vec<Record> = vec![];
        if lap > 0 {
            if let Some(a) = &last_aac {
                while audio_us + au_us / 2 < lap * period {
                    let mut pad = a.clone();
                    pad.host_us = first_us + lap * period;
                    records.push(pad);
                    audio_us += au_us;
                }
            }
        }
        for rec in media.iter() {
            let mut rec = rec.clone();
            rec.host_us += lap * period;
            rec.cam_us = rec.cam_us.wrapping_add((lap * period) as u32);
            if rec.kind == Kind::Aac {
                audio_us += au_us;
            }
            records.push(rec);
        }
        for rec in records.iter() {
            if config.speed > 0.0 {
                let at =
                    Duration::from_micros(((rec.host_us - first_us) as f64 / config.speed) as u64);
                tokio::time::sleep_until(start + at).await;
            }
            let Some(m) = to_bcmedia(rec) else { continue };
            let Ok(bytes) = m.serialize(vec![]) else {
                continue;
            };
            for chunk in bytes.chunks(config.chunk_size.max(1)) {
                let bc = Bc::new(
                    BcMeta {
                        msg_id: MSG_ID_VIDEO,
                        channel_id: 0,
                        msg_num,
                        stream_type,
                        response_code: 0,
                        class: 0x0000,
                    },
                    Some(Extension {
                        version: "1.1".to_string(),
                        binary_data: Some(1),
                        ..Default::default()
                    }),
                    Some(BcPayloads::Binary(chunk.to_vec())),
                );
                let Ok(wire) = bc.serialize(vec![], &EncryptionProtocol::Unencrypted) else {
                    return;
                };
                if out.send(wire).await.is_err() {
                    return;
                }
            }
            shared.stats.lock().unwrap().media_sent += 1;
        }
        if !config.loop_media || media.is_empty() {
            // Keep the stream open, like a camera with nothing new to send
            std::future::pending::<()>().await;
        }
        lap += 1;
    }
}

/// The media span of a recording plus one frame interval, from the video's camera clock
fn lap_period(media: &[Record]) -> u64 {
    let cams: Vec<u32> = media
        .iter()
        .filter(|r| matches!(r.kind, Kind::Iframe | Kind::Pframe))
        .map(|r| r.cam_us)
        .collect();
    if cams.len() < 2 {
        return match (media.first(), media.last()) {
            (Some(a), Some(b)) => (b.host_us - a.host_us).max(1),
            _ => 1,
        };
    }
    let span = cams[cams.len() - 1].wrapping_sub(cams[0]) as u64;
    span + span / (cams.len() as u64 - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bc_protocol::{BcCamera, StreamKind};

    fn media() -> Vec<Record> {
        let rec = |host_us, kind, cam_us, data: &[u8]| Record {
            host_us,
            kind,
            h265: true,
            cam_us,
            cam_time: 0,
            data: data.to_vec(),
        };
        vec![
            rec(0, Kind::Iframe, 1000, &[0, 0, 0, 1, 0x40, 1, 0xaa, 0xbb]),
            rec(
                10,
                Kind::Aac,
                0,
                &[0xff, 0xf1, 0x60, 0x40, 0x01, 0x7f, 0xfc, 1, 0x18, 0x20, 7],
            ),
            rec(20, Kind::Pframe, 67667, &[0, 0, 0, 1, 0x02, 1, 0xcc]),
        ]
    }

    #[tokio::test]
    async fn test_login_and_queries() {
        let cam = FakeCamera::start(FakeCameraConfig::default())
            .await
            .unwrap();
        let camera = BcCamera::new(&cam.camera_opts("t")).await.unwrap();
        camera.login().await.unwrap();
        assert_eq!(camera.uid().await.unwrap(), "FAKEUID0000000001");
        assert_eq!(camera.battery_info().await.unwrap().battery_percent, 80);
        assert_eq!(camera.get_linktype().await.unwrap().link_type, "LAN");
        let version = camera.version().await.unwrap();
        assert_eq!(version.model.as_deref(), Some("Argus PT Ultra (fake)"));
        assert_eq!(
            camera.get_stream_info().await.unwrap().stream_infos.len(),
            1
        );
        assert_eq!(
            camera.get_snapshot().await.unwrap(),
            b"\xff\xd8fake jpeg\xff\xd9"
        );
        camera.logout().await.unwrap();
        // logout() only queues the message; give it time to reach the camera before the
        // connection is shut down (PLAN 1.13 makes logout wait for the reply)
        tokio::time::sleep(Duration::from_millis(100)).await;
        camera.shutdown().await.unwrap();
        let s = cam.stats();
        // The TCP discovery probe is a connection of its own
        assert_eq!((s.connections, s.logins, s.logouts), (2, 1, 1));
    }

    #[tokio::test]
    async fn test_bad_password() {
        let cam = FakeCamera::start(FakeCameraConfig::default())
            .await
            .unwrap();
        let mut opts = cam.camera_opts("t");
        opts.credentials.password = Some("wrong".to_string());
        let camera = BcCamera::new(&opts).await.unwrap();
        assert!(camera.login().await.is_err());
        assert_eq!(cam.stats().failed_logins, 1);
    }

    #[tokio::test]
    async fn test_battery_busy_then_ok() {
        let cam = FakeCamera::start(FakeCameraConfig {
            battery_busy_reads: 1,
            ..Default::default()
        })
        .await
        .unwrap();
        let camera = BcCamera::new(&cam.camera_opts("t")).await.unwrap();
        camera.login().await.unwrap();
        assert!(camera.battery_info().await.is_err());
        assert!(camera.battery_info().await.is_ok());
    }

    #[tokio::test]
    async fn test_stream_replay() {
        let cam = FakeCamera::start(FakeCameraConfig {
            main: Arc::new(media()),
            speed: 0.0,
            chunk_size: 5,
            ..Default::default()
        })
        .await
        .unwrap();
        let camera = BcCamera::new(&cam.camera_opts("t")).await.unwrap();
        camera.login().await.unwrap();
        let mut stream = camera.start_video(StreamKind::Main, 0, true).await.unwrap();
        let mut got = vec![];
        for _ in 0..3 {
            got.push(stream.get_data().await.unwrap().unwrap());
        }
        assert!(
            matches!(&got[0], BcMedia::Iframe(f) if f.microseconds == 1000 && f.data.len() == 8)
        );
        assert!(matches!(&got[1], BcMedia::Aac(a) if a.data.len() == 11));
        assert!(matches!(&got[2], BcMedia::Pframe(f) if f.microseconds == 67667));
        stream.shutdown().await.unwrap();
        let s = cam.stats();
        assert_eq!((s.video_starts, s.video_stops, s.media_sent), (1, 1, 3));
    }

    #[tokio::test]
    async fn test_unreachable_counts_attempts() {
        let cam = FakeCamera::start(FakeCameraConfig::default())
            .await
            .unwrap();
        cam.set_unreachable(true);
        // The port check passes (connections are accepted, then closed); login fails
        let result = async {
            let camera = BcCamera::new(&cam.camera_opts("t")).await?;
            camera.login().await
        }
        .await;
        assert!(result.is_err());
        assert!(cam.stats().connections >= 1);
        assert_eq!(cam.stats().logins, 0);
    }

    #[tokio::test]
    async fn test_drop_connections() {
        let cam = FakeCamera::start(FakeCameraConfig::default())
            .await
            .unwrap();
        let camera = BcCamera::new(&cam.camera_opts("t")).await.unwrap();
        camera.login().await.unwrap();
        cam.drop_connections();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(cam.stats().open_connections, 0);
        // The client notices on its next request
        let ping = tokio::time::timeout(Duration::from_secs(5), camera.get_linktype()).await;
        assert!(
            !matches!(ping, Ok(Ok(_))),
            "ping succeeded on a dropped connection"
        );
    }
}
