//! A config published on `neolink/config`: parsed, secrets restored, validated and checked
//! before the reactor sees it

use std::collections::HashSet;

use anyhow::Context;
use validator::Validate;

use crate::config::Config;

/// What to do with a config written over MQTT
#[derive(Debug)]
pub(super) enum ConfigWrite {
    /// Same as the running config: nothing to do, no reply
    Unchanged,
    /// Apply it, then reply with the result
    Apply(Box<Config>),
    /// Not applied: reply with this
    Refused(String),
}

/// A config parse error without the quoted source line (SPEC R25)
fn redacted_parse_error(text: &str, e: &toml::de::Error) -> String {
    let line = e
        .span()
        .map(|s| text[..s.start.min(text.len())].matches('\n').count() + 1);
    match line {
        Some(l) => format!("Failed to parse the config at line {l}: {}", e.message()),
        None => format!("Failed to parse the config: {}", e.message()),
    }
}

/// Turn the text written to `neolink/config` into what to do with it. `file` is the
/// config neolink started with, `current` the running config.
pub(super) fn prepare_config_write(text: &str, file: &Config, current: &Config) -> ConfigWrite {
    crate::config::warn_unknown_keys(text, "MQTT neolink/config");
    let mut config = match toml::from_str::<Config>(text) {
        Ok(c) => c,
        // The parse error quotes the offending line, which may hold a password: report
        // only the message and the line
        Err(e) => return ConfigWrite::Refused(redacted_parse_error(text, &e)),
    };

    // Fill in skipped passwords, but only for the same broker and the same camera address:
    // a write that points elsewhere must not receive the current secrets
    if let (Some(mqtt), Some(curr_mqtt)) = (config.mqtt.as_mut(), current.mqtt.as_ref()) {
        let same_broker = mqtt.broker_addr == curr_mqtt.broker_addr && mqtt.port == curr_mqtt.port;
        if mqtt.credentials.is_none() && same_broker {
            mqtt.credentials = curr_mqtt.credentials.clone();
        }
        if mqtt.ca.is_none() && same_broker {
            mqtt.ca = curr_mqtt.ca.clone();
        }
        if mqtt.client_auth.is_none() && same_broker {
            mqtt.client_auth = curr_mqtt.client_auth.clone();
        }
    }
    for cam in config.cameras.iter_mut() {
        if let Some(cur_cam) = current.cameras.iter().find(|c| c.name == cam.name) {
            let same_camera =
                cam.camera_addr == cur_cam.camera_addr && cam.camera_uid == cur_cam.camera_uid;
            if cam.password.is_none() && same_camera {
                cam.password = cur_cam.password.clone();
            }
        }
    }
    for user in config.users.iter_mut() {
        if let Some(cur_user) = current.users.iter().find(|c| c.name == user.name) {
            if user.pass.is_none() {
                user.pass = cur_user.pass.clone();
            }
        }
    }
    // Passwords should now be restored if they were not set

    if let Err(e) = config
        .validate()
        .with_context(|| "Failed to validate the MQTT \"config\" config file")
    {
        return ConfigWrite::Refused(format!("{:?}", e));
    }

    // A battery camera must not be turned always-on (or kept awake longer, or woken by
    // more) by whoever can publish to the broker: power settings come from the config file
    // only, and a write that changes any of them is refused whole (SPEC P14)
    let mut refused = vec![];
    for cam in config.cameras.iter() {
        let keys = cam.power_changes(file.cameras.iter().find(|f| f.name == cam.name));
        if !keys.is_empty() {
            let keys = keys.join(", ");
            log::warn!(
                "{}: refused a config write over MQTT: it changes {keys}, which only the \
                 config file may set",
                cam.name
            );
            refused.push(format!("{} ({keys})", cam.name));
        }
    }
    if !refused.is_empty() {
        return ConfigWrite::Refused(format!(
            "FAIL: power settings can only be changed in the config file, and this write \
             changes {}; nothing was applied",
            refused.join("; ")
        ));
    }

    // Nor can it grant itself access: users and each camera's permitted_users also come
    // from the config file only (SPEC R29)
    let mut refused = vec![];
    if !same_users(&config, file) {
        log::warn!("Refused a config write over MQTT: it changes users");
        refused.push("users".to_string());
    }
    for cam in config.cameras.iter() {
        let from_file = file
            .cameras
            .iter()
            .find(|f| f.name == cam.name)
            .and_then(|f| f.permitted_users.as_ref());
        if !same_names(cam.permitted_users.as_ref(), from_file) {
            log::warn!(
                "{}: refused a config write over MQTT: it changes permitted_users",
                cam.name
            );
            refused.push(format!("{} (permitted_users)", cam.name));
        }
    }
    if !refused.is_empty() {
        return ConfigWrite::Refused(format!(
            "FAIL: users and permitted_users can only be changed in the config file, and \
             this write changes {}; nothing was applied",
            refused.join("; ")
        ));
    }

    if *current == config {
        return ConfigWrite::Unchanged;
    }
    ConfigWrite::Apply(Box::new(config))
}

/// The same users with the same passwords, in any order
fn same_users(a: &Config, b: &Config) -> bool {
    a.users.iter().collect::<HashSet<_>>() == b.users.iter().collect::<HashSet<_>>()
}

/// The same `permitted_users`, in any order; absent stays absent (it means every user)
fn same_names(a: Option<&Vec<String>>, b: Option<&Vec<String>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.iter().collect::<HashSet<_>>() == b.iter().collect::<HashSet<_>>(),
        _ => false,
    }
}
