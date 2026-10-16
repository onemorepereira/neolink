use crate::mqtt::Discoveries;
#[cfg(any(feature = "gstreamer", feature = "native-rtsp"))]
use neolink_core::bc_protocol::StreamKind;
use neolink_core::bc_protocol::{DiscoveryMethods, PrintFormat};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::clone::Clone;
use std::collections::HashSet;
use validator::Validate;
use validator::ValidationError;
use chrono::{DateTime, Utc};
static RE_TLS_CLIENT_AUTH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(none|request|require)$").unwrap());
static RE_RTSP_BACKEND: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(gstreamer|native)$").unwrap());
static RE_AUDIO: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(aac|none)$").unwrap());
static RE_PARAMETER_SETS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(strip|passthrough|inject)$").unwrap());
static RE_PAUSE_MODE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(black|still|test|none)$").unwrap());
static RE_MAXENC_SRC: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^([nN]one|[Aa][Ee][Ss]|[Bb][Cc][Ee][Nn][Cc][Rr][Yy][Pp][Tt])$").unwrap()
});

#[derive(Deserialize, Serialize, Validate, Clone, PartialEq)]
pub(crate) struct Config {
    #[validate(nested)]
    #[serde(default)]
    pub(crate) cameras: Vec<CameraConfig>,

    #[serde(rename = "bind", default = "default_bind_addr")]
    pub(crate) bind_addr: String,

    #[validate(range(min = 0, max = 65535, message = "Invalid port", code = "bind_port"))]
    #[serde(default = "default_bind_port")]
    pub(crate) bind_port: u16,

    #[serde(default = "default_tokio_console")]
    pub(crate) tokio_console: bool,

    #[serde(default = "default_certificate")]
    pub(crate) certificate: Option<String>,

    #[serde(default = "Default::default")]
    pub(crate) mqtt: Option<MqttServerConfig>,

    #[validate(regex(
        path = *RE_TLS_CLIENT_AUTH,
        message = "Incorrect tls auth",
        code = "tls_client_auth"
    ))]
    #[serde(default = "default_tls_client_auth")]
    pub(crate) tls_client_auth: String,

    #[validate(nested)]
    #[serde(default)]
    pub(crate) users: Vec<UserConfig>,

    /// Port of the cached snapshot endpoint; unset means bind_port + 1, 0 disables it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) snapshot_port: Option<u16>,

    /// `gstreamer` or `native`; unset: GStreamer when built in (SPEC §8)
    #[validate(regex(
        path = *RE_RTSP_BACKEND,
        message = "rtsp_backend must be \"gstreamer\" or \"native\"",
        code = "rtsp_backend"
    ))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) rtsp_backend: Option<String>,

    /// Native backend: the schemes offered, in order (`digest`, `basic`)
    #[validate(custom(function = "validate_rtsp_auth"))]
    #[serde(default = "default_rtsp_auth")]
    pub(crate) rtsp_auth: Vec<String>,

    /// Whether MQTT may replace the config (SPEC R29)
    #[serde(default = "default_true")]
    pub(crate) mqtt_config_writes: bool,

    /// Native backend: largest RTP packet
    #[validate(range(
        min = 576,
        max = 9000,
        message = "rtp_mtu must be 576-9000",
        code = "rtp_mtu"
    ))]
    #[serde(default = "default_rtp_mtu")]
    pub(crate) rtp_mtu: u16,

    /// Native backend: connection limits (SPEC R24)
    #[validate(nested)]
    #[serde(default)]
    pub(crate) limits: LimitsConfig,

    /// HTTP API listen address (only with feature `api`)
    #[serde(default = "default_api_bind")]
    pub(crate) api_bind: String,

    /// HTTP API listen port (only with feature `api`)
    #[validate(range(min = 0, max = 65535, message = "Invalid port", code = "api_port"))]
    #[serde(default = "default_api_port")]
    pub(crate) api_port: u16,
}

/// Native RTSP server limits (SPEC R24)
#[derive(Deserialize, Serialize, Validate, Clone, PartialEq, Debug)]
pub(crate) struct LimitsConfig {
    #[validate(range(min = 1, max = 4096))]
    #[serde(default = "default_max_connections")]
    pub(crate) max_connections: usize,
    /// Connections from one address (IPv4, or IPv6 /64) that have neither authenticated
    /// nor set up a session. Past it, that address's own oldest one is closed. All
    /// addresses together hold at most max_connections / 2; past that, the oldest one of
    /// the address holding the most is closed, so a flood from one address (or a few)
    /// can't lock others out. Clients that authenticated or set up a session never count, so
    /// this doesn't limit Frigate, go2rtc or Home Assistant on one address
    #[validate(range(min = 1, max = 4096))]
    #[serde(default = "default_max_unauthenticated_per_ip")]
    pub(crate) max_unauthenticated_per_ip: usize,
    #[validate(range(min = 1, max = 300))]
    #[serde(default = "default_first_request_timeout_s")]
    pub(crate) first_request_timeout_s: u64,
    #[validate(range(min = 5, max = 3600))]
    #[serde(default = "default_idle_timeout_s")]
    pub(crate) idle_timeout_s: u64,
    #[validate(range(min = 1, max = 1000))]
    #[serde(default = "default_requests_per_second")]
    pub(crate) requests_per_second: u32,
    /// Wrong credentials per (address, username) per minute; an address may fail 5 × this
    /// across all usernames. Past either, that username's (or every username's)
    /// credentials from that address are refused (401, at once, right or wrong) until a
    /// minute passes without that many failures. RTSP connections already authenticated
    /// as that user are not affected. The snapshot endpoint applies the same limits
    #[validate(range(min = 1, max = 1000))]
    #[serde(default = "default_failed_auth_per_minute")]
    pub(crate) failed_auth_per_minute: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_connections: default_max_connections(),
            max_unauthenticated_per_ip: default_max_unauthenticated_per_ip(),
            first_request_timeout_s: default_first_request_timeout_s(),
            idle_timeout_s: default_idle_timeout_s(),
            requests_per_second: default_requests_per_second(),
            failed_auth_per_minute: default_failed_auth_per_minute(),
        }
    }
}

const fn default_max_connections() -> usize {
    128
}
const fn default_max_unauthenticated_per_ip() -> usize {
    32
}
const fn default_first_request_timeout_s() -> u64 {
    10
}
const fn default_idle_timeout_s() -> u64 {
    60
}
const fn default_requests_per_second() -> u32 {
    20
}
const fn default_failed_auth_per_minute() -> usize {
    10
}
const fn default_rtp_mtu() -> u16 {
    1400
}
fn default_rtsp_auth() -> Vec<String> {
    vec!["digest".into(), "basic".into()]
}

fn validate_rtsp_auth(v: &[String]) -> Result<(), ValidationError> {
    if v.is_empty() || v.iter().any(|s| s != "digest" && s != "basic") {
        return Err(ValidationError::new("rtsp_auth")
            .with_message("rtsp_auth lists \"digest\" and/or \"basic\"".into()));
    }
    Ok(())
}

#[derive(Deserialize, Serialize, Clone, Validate, PartialEq, Eq)]
#[validate(schema(function = "validate_mqtt_server", skip_on_field_errors = true))]
pub(crate) struct MqttServerConfig {
    #[serde(alias = "server")]
    pub(crate) broker_addr: String,

    pub(crate) port: u16,

    #[serde(default, skip_serializing)]
    pub(crate) credentials: Option<(String, String)>,

    #[serde(default, skip_serializing)]
    pub(crate) ca: Option<std::path::PathBuf>,

    #[serde(default, skip_serializing)]
    pub(crate) client_auth: Option<(std::path::PathBuf, std::path::PathBuf)>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, Eq, PartialEq)]
pub(crate) enum StreamConfig {
    #[serde(alias = "none")]
    None,
    #[serde(alias = "all")]
    All,
    #[serde(alias = "both")]
    Both,
    #[serde(
        alias = "main",
        alias = "mainStream",
        alias = "mainstream",
        alias = "MainStream"
    )]
    Main,
    #[serde(
        alias = "sub",
        alias = "subStream",
        alias = "substream",
        alias = "SubStream"
    )]
    Sub,
    #[serde(
        alias = "extern",
        alias = "externStream",
        alias = "externstream",
        alias = "ExternStream"
    )]
    Extern,
}

impl StreamConfig {
    #[cfg(any(feature = "gstreamer", feature = "native-rtsp"))]
    pub(crate) fn as_stream_kinds(&self) -> Vec<StreamKind> {
        match self {
            StreamConfig::All => {
                vec![StreamKind::Main, StreamKind::Extern, StreamKind::Sub]
            }
            StreamConfig::Both => {
                vec![StreamKind::Main, StreamKind::Sub]
            }
            StreamConfig::Main => {
                vec![StreamKind::Main]
            }
            StreamConfig::Sub => {
                vec![StreamKind::Sub]
            }
            StreamConfig::Extern => {
                vec![StreamKind::Extern]
            }
            StreamConfig::None => {
                vec![]
            }
        }
    }
}

#[derive(Deserialize, Serialize, Validate, Clone, PartialEq)]
#[validate(schema(function = "validate_camera_config"))]
pub(crate) struct CameraConfig {
    pub(crate) name: String,

    #[serde(rename = "address")]
    pub(crate) camera_addr: Option<String>,

    #[serde(rename = "uid")]
    pub(crate) camera_uid: Option<String>,

    pub(crate) username: String,

    #[serde(alias = "pass", skip_serializing, default)]
    pub(crate) password: Option<String>,

    #[serde(default = "default_stream")]
    pub(crate) stream: StreamConfig,

    pub(crate) permitted_users: Option<Vec<String>>,

    #[validate(range(min = 0, max = 31, message = "Invalid channel", code = "channel_id"))]
    #[serde(default = "default_channel_id", alias = "channel")]
    pub(crate) channel_id: u8,

    #[validate(nested)]
    #[serde(default = "default_mqtt")]
    pub(crate) mqtt: MqttConfig,

    #[validate(nested)]
    #[serde(default = "default_pause")]
    pub(crate) pause: PauseConfig,

    #[serde(default = "default_discovery")]
    pub(crate) discovery: DiscoveryMethods,

    #[serde(default = "default_maxenc")]
    #[validate(regex(
        path = *RE_MAXENC_SRC,
        message = "Invalid maximum encryption method",
        code = "max_encryption"
    ))]
    pub(crate) max_encryption: String,

    #[serde(default = "default_strict")]
    /// If strict then the media stream will error in the event that the media packets are not as expected
    pub(crate) strict: bool,

    #[serde(default = "default_print", alias = "print")]
    pub(crate) print_format: PrintFormat,

    #[serde(default = "default_update_time", alias = "time")]
    pub(crate) update_time: bool,

    #[validate(range(
        min = 1,
        max = 15000,
        message = "Invalid buffer duration (it's in ms)",
        code = "buffer_duration"
    ))]
    /// Buffer duration in ms
    #[serde(
        default = "default_buffer_duration",
        alias = "duration",
        alias = "buffer"
    )]
    pub(crate) buffer_duration: u64,

    #[serde(default = "default_true", alias = "enable")]
    pub(crate) enabled: bool,

    #[serde(default = "default_false", alias = "verbose")]
    pub(crate) debug: bool,

    #[serde(default = "default_true", alias = "splash")]
    pub(crate) use_splash: bool,

    #[serde(default = "default_splash", alias = "pattern")]
    pub(crate) splash_pattern: SplashPattern,

    #[serde(
        default = "default_max_discovery_retries",
        alias = "retries",
        alias = "max_retries"
    )]
    pub(crate) max_discovery_retries: usize,

    #[serde(default = "default_true", alias = "push", alias = "push_noti")]
    pub(crate) push_notifications: bool,

    /// Disconnect when idle. Unset means: yes for battery or unknown cameras (SPEC P1)
    #[serde(
        default,
        alias = "idle",
        alias = "idle_disc",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) idle_disconnect: Option<bool>,

    /// Whether the camera runs on battery. Unset means unknown, which is treated as battery
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) battery: Option<bool>,

    /// Seconds to stay connected after the last use (SPEC P2)
    #[serde(default = "default_idle_grace_s")]
    pub(crate) idle_grace_s: u64,

    /// Allow `?fresh=1` on the snapshot endpoint, which wakes the camera (SPEC P5)
    #[serde(default = "default_false")]
    pub(crate) snapshot_fresh: bool,

    /// Longest time a motion event keeps the camera connected (SPEC P12)
    #[serde(default = "default_motion_hold_max_s")]
    pub(crate) motion_hold_max_s: u64,

    /// Native backend: `aac` or `none`
    #[validate(regex(path = *RE_AUDIO, message = "audio must be \"aac\" or \"none\"", code = "audio"))]
    #[serde(default = "default_audio")]
    pub(crate) audio: String,

    /// Native backend: pacing latency; unset: automatic (1500 ms)
    #[validate(range(
        min = 0,
        max = 3000,
        message = "pacing_latency_ms must be 0-3000",
        code = "pacing_latency_ms"
    ))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pacing_latency_ms: Option<u64>,

    /// Native backend: in-band VPS/SPS/PPS, `strip`, `passthrough` or `inject` (SPEC R10)
    #[validate(regex(
        path = *RE_PARAMETER_SETS,
        message = "parameter_sets must be strip, passthrough or inject",
        code = "parameter_sets"
    ))]
    #[serde(default = "default_parameter_sets")]
    pub(crate) parameter_sets: String,

    /// Native backend: bridge frames per second during an outage
    #[validate(range(
        min = 0.1,
        max = 2.0,
        message = "bridge_fps must be 0.1-2",
        code = "bridge_fps"
    ))]
    #[serde(default = "default_bridge_fps")]
    pub(crate) bridge_fps: f64,

    /// Native backend: longest outage bridged before the session ends
    #[validate(range(
        min = 10,
        max = 3600,
        message = "bridge_max_s must be 10-3600",
        code = "bridge_max_s"
    ))]
    #[serde(default = "default_bridge_max_s")]
    pub(crate) bridge_max_s: u64,

    /// Native backend: send RTCP sender reports
    #[serde(default = "default_true")]
    pub(crate) rtcp_sr: bool,

    /// Native backend: keep the last IRAP on disk to show while the camera wakes
    #[serde(default = "default_true")]
    pub(crate) persist_irap: bool,

    /// Human label for the camera (API-only, not in TOML config file)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) display_name: Option<String>,

    /// When the camera config was last written via the API
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) updated_at: Option<DateTime<Utc>>,
}

fn default_audio() -> String {
    "aac".into()
}
fn default_parameter_sets() -> String {
    "strip".into()
}
const fn default_bridge_fps() -> f64 {
    0.2
}
const fn default_bridge_max_s() -> u64 {
    120
}

impl CameraConfig {
    /// The camera sleeps (is only connected on demand) unless always-on was asked for,
    /// or it is known to be mains powered and `idle_disconnect` is unset
    pub(crate) fn sleeps(&self) -> bool {
        match self.idle_disconnect {
            Some(idle) => idle,
            None => self.battery != Some(false),
        }
    }

    /// The power settings, those that decide whether and for how long the camera is woken,
    /// that differ from `file`: this camera's entry in the config file, or `None` for a
    /// camera the file doesn't have, which must keep the defaults. Turning the camera off
    /// (`enabled = false`) only means fewer wakes, so it is not a change here (SPEC P14).
    pub(crate) fn power_changes(&self, file: Option<&CameraConfig>) -> Vec<&'static str> {
        let checks = [
            ("battery", self.battery != file.and_then(|f| f.battery)),
            (
                "idle_disconnect",
                self.idle_disconnect != file.and_then(|f| f.idle_disconnect),
            ),
            (
                "idle_grace_s",
                self.idle_grace_s != file.map_or(default_idle_grace_s(), |f| f.idle_grace_s),
            ),
            (
                "motion_hold_max_s",
                self.motion_hold_max_s
                    != file.map_or(default_motion_hold_max_s(), |f| f.motion_hold_max_s),
            ),
            (
                "snapshot_fresh",
                self.snapshot_fresh != file.map_or(default_false(), |f| f.snapshot_fresh),
            ),
            (
                "pause.on_motion",
                self.pause.on_motion != file.map_or(default_on_motion(), |f| f.pause.on_motion),
            ),
            ("enabled", self.enabled && file.is_some_and(|f| !f.enabled)),
        ];
        checks
            .iter()
            .filter(|(_, changed)| *changed)
            .map(|(key, _)| *key)
            .collect()
    }
}

const fn default_idle_grace_s() -> u64 {
    10
}

const fn default_motion_hold_max_s() -> u64 {
    120
}

#[derive(Deserialize, Serialize, Validate, Clone, PartialEq, Eq, Hash)]
pub(crate) struct UserConfig {
    #[validate(custom(function = "validate_username"))]
    #[serde(alias = "username")]
    pub(crate) name: String,

    #[serde(alias = "password", skip_serializing, default)]
    pub(crate) pass: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Validate, PartialEq, Eq)]
pub(crate) struct MqttConfig {
    #[serde(default = "default_true")]
    pub(crate) enable_motion: bool,
    #[serde(default = "default_true")]
    pub(crate) enable_light: bool,
    #[serde(default = "default_true")]
    pub(crate) enable_battery: bool,
    /// Update time in ms
    #[serde(default = "default_2000")]
    #[validate(range(
        min = 500,
        message = "Update ms should be > 500",
        code = "battery_update"
    ))]
    pub(crate) battery_update: u64,
    #[serde(default = "default_true")]
    pub(crate) enable_preview: bool,
    /// Update time in ms
    #[validate(range(
        min = 500,
        message = "Update ms should be > 500",
        code = "preview_update"
    ))]
    #[serde(default = "default_2000")]
    pub(crate) preview_update: u64,

    /// Enable the flood light tasks status
    /// Will not do anything if no floodlight
    /// is detected
    #[serde(default = "default_true")]
    pub(crate) enable_floodlight: bool,
    /// Update time in ms
    #[validate(range(
        min = 500,
        message = "Update ms should be > 500",
        code = "floodlight_update"
    ))]
    #[serde(default = "default_2000")]
    pub(crate) floodlight_update: u64,

    #[serde(default)]
    pub(crate) discovery: Option<MqttDiscoveryConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Validate, PartialEq, Eq)]
pub(crate) struct MqttDiscoveryConfig {
    pub(crate) topic: String,

    pub(crate) features: HashSet<Discoveries>,
}

fn validate_mqtt_server(config: &MqttServerConfig) -> Result<(), ValidationError> {
    if config.ca.is_some() && config.client_auth.is_some() {
        Err(ValidationError::new(
            "Cannot have both ca and client_auth set",
        ))
    } else {
        Ok(())
    }
}

const fn default_true() -> bool {
    true
}

const fn default_false() -> bool {
    false
}

pub(crate) fn default_mqtt() -> MqttConfig {
    MqttConfig {
        enable_motion: true,
        enable_light: true,
        enable_battery: true,
        battery_update: 2000,
        enable_preview: true,
        preview_update: 2000,
        enable_floodlight: true,
        floodlight_update: 2000,
        discovery: Default::default(),
    }
}

pub(crate) fn default_pause() -> PauseConfig {
    PauseConfig {
        on_motion: default_on_motion(),
        on_disconnect: default_on_disconnect(),
        motion_timeout: default_motion_timeout(),
        mode: default_pause_mode(),
    }
}

fn default_print() -> PrintFormat {
    PrintFormat::None
}

fn default_discovery() -> DiscoveryMethods {
    DiscoveryMethods::Relay
}

fn default_maxenc() -> String {
    "Aes".to_string()
}

#[derive(Debug, Deserialize, Serialize, Validate, Clone, PartialEq)]
pub(crate) struct PauseConfig {
    #[serde(default = "default_on_motion")]
    pub(crate) on_motion: bool,

    #[serde(default = "default_on_disconnect", alias = "on_client")]
    pub(crate) on_disconnect: bool,

    #[serde(default = "default_motion_timeout", alias = "timeout")]
    pub(crate) motion_timeout: f64,

    #[serde(default = "default_pause_mode")]
    #[validate(regex(
        path = *RE_PAUSE_MODE,
        message = "Incorrect pause mode",
        code = "mode"
    ))]
    pub(crate) mode: String,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, Eq, PartialEq)]
pub(crate) enum SplashPattern {
    #[serde(alias = "smpte")]
    Smpte,
    #[serde(alias = "snow")]
    Snow,
    #[serde(alias = "black")]
    Black,
    #[serde(alias = "white")]
    White,
    #[serde(alias = "red")]
    Red,
    #[serde(alias = "green")]
    Green,
    #[serde(alias = "blue")]
    Blue,
    #[serde(alias = "checkers-1")]
    Checkers1,
    #[serde(alias = "checkers-2")]
    Checkers2,
    #[serde(alias = "checkers-4")]
    Checkers4,
    #[serde(alias = "checkers-8")]
    Checkers8,
    #[serde(alias = "circular")]
    Circular,
    #[serde(alias = "blink")]
    Blink,
    #[serde(alias = "smpte75")]
    Smpte75,
    #[serde(alias = "zone-plate")]
    ZonePlate,
    #[serde(alias = "gamut")]
    Gamut,
    #[serde(alias = "chroma-zone-plate")]
    ChromaZonePlate,
    #[serde(alias = "solid-color")]
    SolidColor,
    #[serde(alias = "ball")]
    Ball,
    #[serde(alias = "smpte100")]
    Smpte100,
    #[serde(alias = "bar")]
    Bar,
    #[serde(alias = "pinwheel")]
    Pinwheel,
    #[serde(alias = "spokes")]
    Spokes,
    #[serde(alias = "gradient")]
    Gradient,
    #[serde(alias = "colors")]
    Colors,
    #[serde(alias = "smpte-rp-219")]
    SmpteRp219,
}

impl std::fmt::Display for SplashPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let s = match self {
            SplashPattern::Smpte => "smpte",
            SplashPattern::Snow => "snow",
            SplashPattern::Black => "black",
            SplashPattern::White => "white",
            SplashPattern::Red => "red",
            SplashPattern::Green => "green",
            SplashPattern::Blue => "blue",
            SplashPattern::Checkers1 => "checkers-1",
            SplashPattern::Checkers2 => "checkers-2",
            SplashPattern::Checkers4 => "checkers-4",
            SplashPattern::Checkers8 => "checkers-8",
            SplashPattern::Circular => "circular",
            SplashPattern::Blink => "blink",
            SplashPattern::Smpte75 => "smpte75",
            SplashPattern::ZonePlate => "zone-plate",
            SplashPattern::Gamut => "gamut",
            SplashPattern::ChromaZonePlate => "chroma-zone-plate",
            SplashPattern::SolidColor => "solid-color",
            SplashPattern::Ball => "ball",
            SplashPattern::Smpte100 => "smpte100",
            SplashPattern::Bar => "bar",
            SplashPattern::Pinwheel => "pinwheel",
            SplashPattern::Spokes => "spokes",
            SplashPattern::Gradient => "gradient",
            SplashPattern::Colors => "colors",
            SplashPattern::SmpteRp219 => "smpte-rp-219",
        }
        .to_string();
        write!(f, "{}", s)
    }
}

fn default_bind_addr() -> String {
    "0.0.0.0".to_string()
}

fn default_bind_port() -> u16 {
    8554
}

fn default_api_bind() -> String {
    "0.0.0.0".to_string()
}

fn default_api_port() -> u16 {
    8080
}

fn default_stream() -> StreamConfig {
    StreamConfig::All
}

fn default_certificate() -> Option<String> {
    None
}

fn default_tls_client_auth() -> String {
    "none".to_string()
}

fn default_tokio_console() -> bool {
    false
}

fn default_channel_id() -> u8 {
    0
}

fn default_update_time() -> bool {
    false
}

fn default_motion_timeout() -> f64 {
    1.
}

fn default_on_disconnect() -> bool {
    false
}

fn default_on_motion() -> bool {
    false
}

fn default_pause_mode() -> String {
    "none".to_string()
}

fn default_strict() -> bool {
    false
}

fn default_buffer_duration() -> u64 {
    3000
}

fn default_max_discovery_retries() -> usize {
    10
}

fn default_2000() -> u64 {
    2000
}

fn default_splash() -> SplashPattern {
    SplashPattern::Snow
}

pub(crate) static RESERVED_NAMES: &[&str] = &["anyone", "anonymous"];
fn validate_username(name: &str) -> Result<(), ValidationError> {
    if name.trim().is_empty() {
        return Err(ValidationError::new("username cannot be empty"));
    }
    if RESERVED_NAMES.contains(&name) {
        return Err(ValidationError::new("This is a reserved username"));
    }
    Ok(())
}

fn validate_camera_config(camera_config: &CameraConfig) -> Result<(), ValidationError> {
    match (&camera_config.camera_addr, &camera_config.camera_uid) {
        (None, None) => Err(ValidationError::new(
            "Either camera address or uid must be given",
        )),
        _ => Ok(()),
    }
}

/// Debug output is the serialized form, which skips passwords and credentials
/// (`skip_serializing`), so configs can be logged at any level (PLAN 1.6)
macro_rules! redacted_debug {
    ($($t:ty),*) => {$(
        impl std::fmt::Debug for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match serde_json::to_string(self) {
                    Ok(json) => write!(f, "{} {}", stringify!($t), json),
                    Err(_) => write!(f, "{} {{ .. }}", stringify!($t)),
                }
            }
        }
    )*};
}
redacted_debug!(Config, MqttServerConfig, CameraConfig, UserConfig);

#[cfg(test)]
mod redaction_tests {
    use super::*;

    #[test]
    fn test_debug_has_no_secrets() {
        let config: Config = toml::from_str(
            r#"
            [mqtt]
            broker_addr = "127.0.0.1"
            port = 1883
            credentials = ["mqttuser", "mqttsecret"]
            [[users]]
            name = "alice"
            pass = "usersecret"
            [[cameras]]
            name = "cam"
            username = "admin"
            password = "camsecret"
            address = "127.0.0.1"
            "#,
        )
        .unwrap();
        let debug = format!("{:?} {:?} {:#?}", config, config.cameras[0], config.users);
        for secret in ["mqttsecret", "usersecret", "camsecret"] {
            assert!(!debug.contains(secret), "{} leaked in {}", secret, debug);
        }
        assert!(debug.contains("\"name\":\"cam\""));
    }
}

// ---------------------------------------------------------------------------------------
// Unknown keys (PLAN 1.7)
//
// serde ignores keys it doesn't know, so a misspelt `idle_disconect = false` silently did
// nothing. Each section's names and aliases are listed here; a test checks the list
// against the structs above.

const CONFIG_KEYS: &[&str] = &[
    "cameras",
    "bind",
    "bind_port",
    "tokio_console",
    "certificate",
    "mqtt",
    "tls_client_auth",
    "users",
    "snapshot_port",
    "rtsp_backend",
    "rtsp_auth",
    "mqtt_config_writes",
    "rtp_mtu",
    "limits",
    "api_bind",
    "api_port",
];
const LIMITS_KEYS: &[&str] = &[
    "max_connections",
    "max_unauthenticated_per_ip",
    "first_request_timeout_s",
    "idle_timeout_s",
    "requests_per_second",
    "failed_auth_per_minute",
];
const MQTT_SERVER_KEYS: &[&str] = &[
    "broker_addr",
    "server",
    "port",
    "credentials",
    "ca",
    "client_auth",
];
const CAMERA_KEYS: &[&str] = &[
    "name",
    "address",
    "uid",
    "username",
    "password",
    "pass",
    "stream",
    "permitted_users",
    "channel_id",
    "channel",
    "mqtt",
    "pause",
    "discovery",
    "max_encryption",
    "strict",
    "print_format",
    "print",
    "update_time",
    "time",
    "buffer_duration",
    "duration",
    "buffer",
    "enabled",
    "enable",
    "debug",
    "verbose",
    "use_splash",
    "splash",
    "splash_pattern",
    "pattern",
    "max_discovery_retries",
    "retries",
    "max_retries",
    "push_notifications",
    "push",
    "push_noti",
    "idle_disconnect",
    "idle",
    "idle_disc",
    "battery",
    "idle_grace_s",
    "snapshot_fresh",
    "motion_hold_max_s",
    "audio",
    "pacing_latency_ms",
    "parameter_sets",
    "bridge_fps",
    "bridge_max_s",
    "rtcp_sr",
    "persist_irap",
    "display_name",
    "updated_at",
];
const USER_KEYS: &[&str] = &["name", "username", "pass", "password"];
const CAMERA_MQTT_KEYS: &[&str] = &[
    "enable_motion",
    "enable_light",
    "enable_battery",
    "battery_update",
    "enable_preview",
    "preview_update",
    "enable_floodlight",
    "floodlight_update",
    "discovery",
];
const CAMERA_MQTT_DISCOVERY_KEYS: &[&str] = &["topic", "features"];
const PAUSE_KEYS: &[&str] = &[
    "on_motion",
    "on_disconnect",
    "on_client",
    "motion_timeout",
    "timeout",
    "mode",
];

/// Settings this build can't honour, said once at startup. `text` is the config file, to
/// tell a setting the user wrote from a default.
pub(crate) fn warn_feature_mismatches(config: &Config, text: &str) {
    let tables = text.parse::<toml::Value>().ok();
    let cameras = tables
        .as_ref()
        .and_then(|t| t.get("cameras"))
        .and_then(|c| c.as_array());
    for (i, c) in config.cameras.iter().enumerate() {
        let written = cameras
            .and_then(|a| a.get(i))
            .and_then(|t| t.as_table())
            .is_some_and(|t| {
                ["push_notifications", "push", "push_noti"]
                    .iter()
                    .any(|k| t.contains_key(*k))
            });
        if c.push_notifications && written && !cfg!(feature = "pushnoti") {
            log::warn!(
                "{}: push_notifications is set but this build has no push notification support \
                 (feature pushnoti); it is ignored",
                c.name
            );
        }
        if c.pause.on_motion && c.sleeps() && !(cfg!(feature = "pushnoti") && c.push_notifications)
        {
            log::warn!(
                "{}: pause.on_motion on a camera that sleeps: motion events arrive only over an \
                 open connection, so the stream can't start on motion while it sleeps. Either \
                 keep it connected (idle_disconnect = false, costs battery) or don't use \
                 pause.on_motion with it",
                c.name
            );
        }
    }
}

static NATIVE_RTSP_ACTIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The native RTSP backend is serving: reloads it can't apply are refused (SPEC R7)
#[cfg_attr(not(feature = "native-rtsp"), allow(dead_code))]
pub(crate) fn set_native_rtsp_active(active: bool) {
    NATIVE_RTSP_ACTIVE.store(active, std::sync::atomic::Ordering::SeqCst);
}

/// Why a reloaded config must not be applied at all, if it mustn't. The whole config is
/// refused, so the previous one stays everywhere (cameras included).
pub(crate) fn reload_rejection(config: &Config) -> Option<String> {
    if !NATIVE_RTSP_ACTIVE.load(std::sync::atomic::Ordering::SeqCst) {
        return None;
    }
    if config.certificate.is_some() {
        return Some(
            "RTSPS is not yet supported by the native backend: `certificate` is not allowed".into(),
        );
    }
    if config.tls_client_auth != "none" {
        return Some("tls_client_auth is not supported by the native backend".into());
    }
    None
}

/// Keys in a config text that neolink does not know, as dotted paths
pub(crate) fn unknown_keys(text: &str) -> Vec<String> {
    let Ok(toml::Value::Table(root)) = text.parse::<toml::Value>() else {
        return vec![]; // The real parse reports syntax errors
    };
    let mut out = vec![];
    fn check(table: &toml::Table, known: &[&str], path: &str, out: &mut Vec<String>) {
        for key in table.keys() {
            if !known.contains(&key.as_str()) {
                out.push(format!("{path}{key}"));
            }
        }
    }
    check(&root, CONFIG_KEYS, "", &mut out);
    if let Some(toml::Value::Table(mqtt)) = root.get("mqtt") {
        check(mqtt, MQTT_SERVER_KEYS, "mqtt.", &mut out);
    }
    if let Some(toml::Value::Table(limits)) = root.get("limits") {
        check(limits, LIMITS_KEYS, "limits.", &mut out);
    }
    if let Some(toml::Value::Array(users)) = root.get("users") {
        for (i, user) in users.iter().enumerate() {
            if let toml::Value::Table(user) = user {
                check(user, USER_KEYS, &format!("users[{i}]."), &mut out);
            }
        }
    }
    if let Some(toml::Value::Array(cameras)) = root.get("cameras") {
        for (i, camera) in cameras.iter().enumerate() {
            let toml::Value::Table(camera) = camera else {
                continue;
            };
            let path = format!("cameras[{i}].");
            check(camera, CAMERA_KEYS, &path, &mut out);
            if let Some(toml::Value::Table(pause)) = camera.get("pause") {
                check(pause, PAUSE_KEYS, &format!("{path}pause."), &mut out);
            }
            if let Some(toml::Value::Table(mqtt)) = camera.get("mqtt") {
                check(mqtt, CAMERA_MQTT_KEYS, &format!("{path}mqtt."), &mut out);
                if let Some(toml::Value::Table(d)) = mqtt.get("discovery") {
                    check(
                        d,
                        CAMERA_MQTT_DISCOVERY_KEYS,
                        &format!("{path}mqtt.discovery."),
                        &mut out,
                    );
                }
            }
        }
    }
    out
}

/// Log a warning for each unknown key
pub(crate) fn warn_unknown_keys(text: &str, source: &str) {
    for key in unknown_keys(text) {
        log::warn!("{source}: unknown config key `{key}` is ignored; check its spelling");
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    /// The key lists match the structs' field names, renames and aliases
    #[test]
    fn test_key_lists_match_structs() {
        let src = include_str!("config.rs");
        let rename = regex::Regex::new(r#"rename = "([^"]+)""#).unwrap();
        let alias = regex::Regex::new(r#"alias = "([^"]+)""#).unwrap();
        let field = regex::Regex::new(r"^\s*pub\(crate\) (\w+):").unwrap();
        let lists: &[(&str, &[&str])] = &[
            ("Config", CONFIG_KEYS),
            ("LimitsConfig", LIMITS_KEYS),
            ("MqttServerConfig", MQTT_SERVER_KEYS),
            ("CameraConfig", CAMERA_KEYS),
            ("UserConfig", USER_KEYS),
            ("MqttConfig", CAMERA_MQTT_KEYS),
            ("MqttDiscoveryConfig", CAMERA_MQTT_DISCOVERY_KEYS),
            ("PauseConfig", PAUSE_KEYS),
        ];
        for (name, known) in lists {
            let start = src
                .find(&format!("pub(crate) struct {name} {{"))
                .unwrap_or_else(|| panic!("struct {}", name));
            let body = &src[start..start + src[start..].find("\n}").unwrap()];
            let mut keys = vec![];
            let mut attrs = String::new();
            for line in body.lines().skip(1) {
                if let Some(f) = field.captures(line) {
                    match rename.captures(&attrs) {
                        Some(r) => keys.push(r[1].to_string()),
                        None => keys.push(f[1].to_string()),
                    }
                    keys.extend(alias.captures_iter(&attrs).map(|a| a[1].to_string()));
                    attrs.clear();
                } else {
                    attrs.push_str(line);
                    attrs.push('\n');
                }
            }
            let known: Vec<String> = known.iter().map(|s| s.to_string()).collect();
            assert_eq!(keys, known, "key list for {name} is out of date");
        }
    }

    #[test]
    fn test_unknown_keys() {
        let text = r#"
            bind = "0.0.0.0"
            bnd_port = 1
            [[cameras]]
            name = "a"
            username = "admin"
            address = "1.2.3.4"
            idle_disconect = false
            [cameras.pause]
            on_moton = true
            [cameras.mqtt.discovery]
            topc = "x"
        "#;
        assert_eq!(
            unknown_keys(text),
            [
                "bnd_port",
                "cameras[0].idle_disconect",
                "cameras[0].pause.on_moton",
                "cameras[0].mqtt.discovery.topc"
            ]
        );
    }

    /// The shipped sample config and the README's examples parse, with known keys only
    #[test]
    fn test_config_corpus() {
        let mut corpus = vec![(
            "sample_config.toml".to_string(),
            include_str!("../sample_config.toml").to_string(),
        )];
        let readme = include_str!("../README.md");
        for (i, block) in readme.split("```toml").skip(1).enumerate() {
            let block = block.split("```").next().unwrap();
            corpus.push((format!("README.md toml block {i}"), block.to_string()));
        }
        // Configs from older releases that must keep working
        corpus.push((
            "legacy aliases".to_string(),
            r#"
            [[cameras]]
            name = "old"
            username = "admin"
            pass = "x"
            address = "192.168.1.2:9000"
            channel = 0
            idle = true
            time = false
            splash = false
            [cameras.pause]
            on_client = true
            timeout = 2.0
            "#
            .to_string(),
        ));
        for (name, text) in corpus {
            // README blocks are often fragments: they only need known keys
            if !name.starts_with("README") {
                let parsed: Result<Config, _> = toml::from_str(&text);
                assert!(parsed.is_ok(), "{name} does not parse: {:?}", parsed.err());
            }
            // A README snippet without a header may be top-level or camera settings
            let unknown: Vec<String> = unknown_keys(&text)
                .into_iter()
                .filter(|k| !(name.starts_with("README") && CAMERA_KEYS.contains(&k.as_str())))
                .collect();
            assert_eq!(unknown, Vec::<String>::new(), "{name}");
        }
    }

    #[test]
    fn test_api_config_defaults() {
        let config: Config = toml::from_str(
            r#"
            [[cameras]]
            name = "test"
            username = "admin"
            address = "1.2.3.4"
            "#,
        )
        .unwrap();
        assert_eq!(config.api_bind, "0.0.0.0");
        assert_eq!(config.api_port, 8080);
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cameras: vec![],
            bind_addr: default_bind_addr(),
            bind_port: default_bind_port(),
            tokio_console: default_tokio_console(),
            certificate: default_certificate(),
            mqtt: None,
            tls_client_auth: default_tls_client_auth(),
            users: vec![],
            snapshot_port: None,
            rtsp_backend: None,
            rtsp_auth: default_rtsp_auth(),
            mqtt_config_writes: default_true(),
            rtp_mtu: default_rtp_mtu(),
            limits: LimitsConfig::default(),
            api_bind: default_api_bind(),
            api_port: default_api_port(),
        }
    }
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            camera_addr: None,
            camera_uid: None,
            username: String::new(),
            password: None,
            stream: default_stream(),
            permitted_users: None,
            channel_id: default_channel_id(),
            mqtt: default_mqtt(),
            pause: default_pause(),
            discovery: default_discovery(),
            max_encryption: default_maxenc(),
            strict: default_strict(),
            print_format: default_print(),
            update_time: default_update_time(),
            buffer_duration: default_buffer_duration(),
            enabled: default_true(),
            debug: default_false(),
            use_splash: default_true(),
            splash_pattern: default_splash(),
            max_discovery_retries: default_max_discovery_retries(),
            push_notifications: default_true(),
            idle_disconnect: None,
            battery: None,
            idle_grace_s: default_idle_grace_s(),
            snapshot_fresh: default_false(),
            motion_hold_max_s: default_motion_hold_max_s(),
            audio: default_audio(),
            pacing_latency_ms: None,
            parameter_sets: default_parameter_sets(),
            bridge_fps: default_bridge_fps(),
            bridge_max_s: default_bridge_max_s(),
            rtcp_sr: default_true(),
            persist_irap: default_true(),
            display_name: None,
            updated_at: None,
        }
    }
}
