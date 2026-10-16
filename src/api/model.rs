//! JSON request/response types for the camera API, and conversion to/from the
//! internal `CameraConfig` / `Config`.
//!
//! GET never returns `username` or `password`. POST/PUT accept them for internal
//! storage but they are never echoed back.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{CameraConfig, StreamConfig};

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn default_stream() -> String {
    "main".to_string()
}

fn default_discovery() -> String {
    "relay".to_string()
}

fn default_idle_grace_s() -> u64 {
    10
}

/// A camera as accepted on POST/PUT.
///
/// `username` and `password` are accepted for internal storage but never disclosed.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub(crate) struct CameraRequest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    #[serde(default = "default_true")]
    pub battery: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_stream")]
    pub stream: String,
    #[serde(default = "default_discovery")]
    pub discovery: String,
    #[serde(default = "default_false")]
    pub snapshot_fresh: bool,
    #[serde(default = "default_idle_grace_s")]
    pub idle_grace_s: u64,
}

/// A camera as returned on GET.
///
/// `username` is always `None` (serialized as null) and never disclosed.
/// `password` is not present at all.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub(crate) struct CameraResponse {
    pub name: String,
    /// Never disclosed: always serialized as `null`, never the actual value.
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    #[serde(default = "default_true")]
    pub battery: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub stream: String,
    pub discovery: String,
    #[serde(default = "default_false")]
    pub snapshot_fresh: bool,
    #[serde(default = "default_idle_grace_s")]
    pub idle_grace_s: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

impl CameraRequest {
    /// The stream kind this request asks for.
    pub(crate) fn stream_kind(&self) -> StreamConfig {
        match self.stream.to_lowercase().as_str() {
            "none" => StreamConfig::None,
            "all" => StreamConfig::All,
            "both" => StreamConfig::Both,
            "sub" => StreamConfig::Sub,
            "extern" => StreamConfig::Extern,
            _ => StreamConfig::Main,
        }
    }

    /// Convert into an internal `CameraConfig`.
    ///
    /// `username` is required: if the request doesn't carry one, `supplied_username`
    /// must be `Some` (the caller resolves it, e.g. from the existing config on PUT).
    pub(crate) fn into_camera_config(
        &self,
        supplied_username: Option<&str>,
    ) -> Result<CameraConfig> {
        let username = self
            .username
            .as_deref()
            .or(supplied_username)
            .ok_or_else(|| anyhow::anyhow!("username is required"))?;

        // A camera needs an address or a uid (existing validation rule).
        if self.address.is_none() && self.uid.is_none() {
            anyhow::bail!("either address or uid is required");
        }

        Ok(CameraConfig {
            name: self.name.clone(),
            camera_addr: self.address.clone(),
            camera_uid: self.uid.clone(),
            username: username.to_string(),
            password: self.password.clone(),
            stream: self.stream_kind(),
            permitted_users: None,
            channel_id: 0,
            mqtt: crate::config::default_mqtt(),
            pause: crate::config::default_pause(),
            discovery: match self.discovery.to_lowercase().as_str() {
                "none" => neolink_core::bc_protocol::DiscoveryMethods::None,
                "local" => neolink_core::bc_protocol::DiscoveryMethods::Local,
                "remote" => neolink_core::bc_protocol::DiscoveryMethods::Remote,
                "map" => neolink_core::bc_protocol::DiscoveryMethods::Map,
                "cellular" => neolink_core::bc_protocol::DiscoveryMethods::Cellular,
                _ => neolink_core::bc_protocol::DiscoveryMethods::Relay,
            },
            max_encryption: "Aes".to_string(),
            strict: false,
            print_format: neolink_core::bc_protocol::PrintFormat::None,
            update_time: false,
            buffer_duration: 3000,
            enabled: self.enabled,
            debug: false,
            use_splash: true,
            splash_pattern: crate::config::SplashPattern::Snow,
            max_discovery_retries: 10,
            push_notifications: true,
            idle_disconnect: None,
            battery: Some(self.battery),
            idle_grace_s: self.idle_grace_s,
            snapshot_fresh: self.snapshot_fresh,
            motion_hold_max_s: 120,
            audio: "aac".to_string(),
            pacing_latency_ms: None,
            parameter_sets: "strip".to_string(),
            bridge_fps: 0.2,
            bridge_max_s: 120,
            rtcp_sr: true,
            persist_irap: true,
            display_name: self.display_name.clone(),
            updated_at: Some(Utc::now()),
        })
    }
}

impl From<&CameraConfig> for CameraResponse {
    fn from(config: &CameraConfig) -> Self {
        Self {
            name: config.name.clone(),
            username: None, // never disclosed
            display_name: config.display_name.clone(),
            address: config.camera_addr.clone(),
            uid: config.camera_uid.clone(),
            battery: config.battery.unwrap_or(true),
            enabled: config.enabled,
            stream: match config.stream {
                StreamConfig::None => "none",
                StreamConfig::All => "all",
                StreamConfig::Both => "both",
                StreamConfig::Main => "main",
                StreamConfig::Sub => "sub",
                StreamConfig::Extern => "extern",
            }
            .to_string(),
            discovery: match config.discovery {
                neolink_core::bc_protocol::DiscoveryMethods::None => "none",
                neolink_core::bc_protocol::DiscoveryMethods::Local => "local",
                neolink_core::bc_protocol::DiscoveryMethods::Remote => "remote",
                neolink_core::bc_protocol::DiscoveryMethods::Map => "map",
                neolink_core::bc_protocol::DiscoveryMethods::Relay => "relay",
                neolink_core::bc_protocol::DiscoveryMethods::Cellular => "cellular",
                neolink_core::bc_protocol::DiscoveryMethods::Debug => "debug",
            }
            .to_string(),
            snapshot_fresh: config.snapshot_fresh,
            idle_grace_s: config.idle_grace_s,
            updated_at: config.updated_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_camera_response_omits_secrets() {
        let config = test_camera_with_secrets();
        let response = CameraResponse::from(&config);
        let json = serde_json::to_string(&response).unwrap();
        assert!(!json.contains("admin"), "username leaked: {json}");
        assert!(!json.contains("secret"), "password leaked: {json}");
        assert!(!json.contains("password"), "password key present: {json}");
        assert!(json.contains("\"username\":null"), "username not null: {json}");
        assert!(json.contains("\"name\":\"cam\""));
    }

    fn test_camera_with_secrets() -> CameraConfig {
        CameraConfig {
            name: "cam".to_string(),
            camera_addr: Some("1.2.3.4".into()),
            camera_uid: None,
            username: "admin".to_string(),
            password: Some("secret".into()),
            stream: crate::config::StreamConfig::Main,
            permitted_users: None,
            channel_id: 0,
            mqtt: crate::config::default_mqtt(),
            pause: crate::config::default_pause(),
            discovery: neolink_core::bc_protocol::DiscoveryMethods::Relay,
            max_encryption: "Aes".to_string(),
            strict: false,
            print_format: neolink_core::bc_protocol::PrintFormat::None,
            update_time: false,
            buffer_duration: 3000,
            enabled: true,
            debug: false,
            use_splash: true,
            splash_pattern: crate::config::SplashPattern::Snow,
            max_discovery_retries: 10,
            push_notifications: true,
            idle_disconnect: None,
            battery: Some(true),
            idle_grace_s: 10,
            snapshot_fresh: false,
            motion_hold_max_s: 120,
            audio: "aac".to_string(),
            pacing_latency_ms: None,
            parameter_sets: "strip".to_string(),
            bridge_fps: 0.2,
            bridge_max_s: 120,
            rtcp_sr: true,
            persist_irap: true,
            display_name: None,
            updated_at: None,
        }
    }

    #[test]
    fn test_request_into_config_requires_username() {
        let req = CameraRequest {
            name: "cam".to_string(),
            username: None,
            password: None,
            display_name: None,
            address: Some("1.2.3.4".to_string()),
            uid: None,
            battery: true,
            enabled: true,
            stream: "main".to_string(),
            discovery: "relay".to_string(),
            snapshot_fresh: false,
            idle_grace_s: 10,
        };
        assert!(req.into_camera_config(None).is_err());
        assert!(req.into_camera_config(Some("admin")).is_ok());
    }

    #[test]
    fn test_request_into_config_requires_address_or_uid() {
        let req = CameraRequest {
            name: "cam".to_string(),
            username: Some("admin".to_string()),
            password: None,
            display_name: None,
            address: None,
            uid: None,
            battery: true,
            enabled: true,
            stream: "main".to_string(),
            discovery: "relay".to_string(),
            snapshot_fresh: false,
            idle_grace_s: 10,
        };
        assert!(req.into_camera_config(Some("admin")).is_err());
    }
}
