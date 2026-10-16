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
/// All fields except `name` are optional: on POST, omitted fields get defaults;
/// on PUT, omitted fields are left unchanged (partial update). `username` and
/// `password` are accepted for internal storage but never disclosed.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_fresh: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_grace_s: Option<u64>,
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
    /// The stream kind this request asks for (`None` means "not specified").
    pub(crate) fn stream_kind(&self) -> Option<StreamConfig> {
        self.stream.as_ref().map(|s| match s.to_lowercase().as_str() {
            "none" => StreamConfig::None,
            "all" => StreamConfig::All,
            "both" => StreamConfig::Both,
            "sub" => StreamConfig::Sub,
            "extern" => StreamConfig::Extern,
            _ => StreamConfig::Main,
        })
    }

    /// The discovery method this request asks for (`None` means "not specified").
    fn discovery_kind(&self) -> Option<neolink_core::bc_protocol::DiscoveryMethods> {
        self.discovery.as_ref().map(|s| match s.to_lowercase().as_str() {
            "none" => neolink_core::bc_protocol::DiscoveryMethods::None,
            "local" => neolink_core::bc_protocol::DiscoveryMethods::Local,
            "remote" => neolink_core::bc_protocol::DiscoveryMethods::Remote,
            "map" => neolink_core::bc_protocol::DiscoveryMethods::Map,
            "cellular" => neolink_core::bc_protocol::DiscoveryMethods::Cellular,
            _ => neolink_core::bc_protocol::DiscoveryMethods::Relay,
        })
    }

    /// Resolve into a full `CameraConfig` for POST (create).
    /// Applies defaults for any field that is `None`.
    /// `username` must be provided either in the request or as `supplied_username`.
    pub(crate) fn resolve_for_create(
        &self,
        supplied_username: Option<&str>,
    ) -> Result<CameraConfig> {
        let username = self
            .username
            .as_deref()
            .or(supplied_username)
            .ok_or_else(|| anyhow::anyhow!("username is required"))?;

        if self.address.is_none() && self.uid.is_none() {
            anyhow::bail!("either address or uid is required");
        }

        Ok(CameraConfig {
            name: self.name.clone(),
            camera_addr: self.address.clone(),
            camera_uid: self.uid.clone(),
            username: username.to_string(),
            password: self.password.clone(),
            stream: self.stream_kind().unwrap_or(StreamConfig::Main),
            permitted_users: None,
            channel_id: 0,
            mqtt: crate::config::default_mqtt(),
            pause: crate::config::default_pause(),
            discovery: self.discovery_kind()
                .unwrap_or(neolink_core::bc_protocol::DiscoveryMethods::Relay),
            max_encryption: "Aes".to_string(),
            strict: false,
            print_format: neolink_core::bc_protocol::PrintFormat::None,
            update_time: false,
            buffer_duration: 3000,
            enabled: self.enabled.unwrap_or(true),
            debug: false,
            use_splash: true,
            splash_pattern: crate::config::SplashPattern::Snow,
            max_discovery_retries: 10,
            push_notifications: true,
            idle_disconnect: None,
            battery: Some(self.battery.unwrap_or(true)),
            idle_grace_s: self.idle_grace_s.unwrap_or(10),
            snapshot_fresh: self.snapshot_fresh.unwrap_or(false),
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

    /// Apply this request as a partial update to an existing `CameraConfig` (PUT).
    /// Only fields that are `Some` are changed; everything else is left as-is.
    pub(crate) fn apply_to(&self, existing: &mut CameraConfig) {
        if let Some(display_name) = &self.display_name {
            existing.display_name = Some(display_name.clone());
        }
        if let Some(address) = &self.address {
            existing.camera_addr = Some(address.clone());
        }
        if let Some(uid) = &self.uid {
            existing.camera_uid = Some(uid.clone());
        }
        if let Some(username) = &self.username {
            existing.username = username.clone();
        }
        if let Some(password) = &self.password {
            existing.password = Some(password.clone());
        }
        if let Some(battery) = self.battery {
            existing.battery = Some(battery);
        }
        if let Some(enabled) = self.enabled {
            existing.enabled = enabled;
        }
        if let Some(stream) = self.stream_kind() {
            existing.stream = stream;
        }
        if let Some(discovery) = self.discovery_kind() {
            existing.discovery = discovery;
        }
        if let Some(snapshot_fresh) = self.snapshot_fresh {
            existing.snapshot_fresh = snapshot_fresh;
        }
        if let Some(idle_grace_s) = self.idle_grace_s {
            existing.idle_grace_s = idle_grace_s;
        }
        existing.updated_at = Some(Utc::now());
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
            battery: None,
            enabled: None,
            stream: None,
            discovery: None,
            snapshot_fresh: None,
            idle_grace_s: None,
        };
        assert!(req.resolve_for_create(None).is_err());
        assert!(req.resolve_for_create(Some("admin")).is_ok());
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
            battery: None,
            enabled: None,
            stream: None,
            discovery: None,
            snapshot_fresh: None,
            idle_grace_s: None,
        };
        assert!(req.resolve_for_create(Some("admin")).is_err());
    }
}
