use anyhow::{Context, Result};
use clap::Parser;
use fcm_push_listener::{new_heartbeat_ack, Message, MessageStream, Registration};
use log::*;
use std::{fs, path::PathBuf};
use tokio::io::AsyncWriteExt;
use tokio_stream::StreamExt;
use validator::Validate;

mod config;
mod opt;
mod utils;

use config::Config;
use opt::Opt;
use utils::find_and_connect;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let opt = Opt::parse();

    let conf_path = opt.config.context("Must supply --config file")?;
    let config: Config = toml::from_str(
        &fs::read_to_string(&conf_path)
            .with_context(|| format!("Failed to read {:?}", conf_path))?,
    )
    .with_context(|| format!("Failed to parse the {:?} config file", conf_path))?;

    config
        .validate()
        .with_context(|| format!("Failed to validate the {:?} config file", conf_path))?;

    let camera = find_and_connect(&config, &opt.camera).await?;

    // The Firebase app of Reolink's Android app (see README.md). fcm-push-listener >= 3
    // registers with a Firebase app instead of a sender ID (743639030586 for Android,
    // 696841269229 for iOS), which Google no longer serves.
    let firebase_app_id = "1:743639030586:android:86f60a4fb7143876";
    let firebase_project_id = "reolink-login";
    let firebase_api_key = "AIzaSyBEUIuWHnnOEwFahxWgQB4Yt4NsgOmkPyE";

    let http = reqwest::Client::new();
    // JSON: fcm-push-listener 4 reads its keys as borrowed strings, which toml cannot give
    let token_path = PathBuf::from("./token.json");
    let registration = match fs::read_to_string(&token_path) {
        Ok(text) => {
            let registration: Registration = serde_json::from_str(&text).with_context(|| {
                format!("Failed to parse {token_path:?}; delete it to register again")
            })?;
            info!("Loaded token");
            registration
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!("Registering new token");
            let registration = fcm_push_listener::register(
                &http,
                firebase_app_id,
                firebase_project_id,
                firebase_api_key,
                None,
            )
            .await?;
            let new_token = serde_json::to_string_pretty(&registration)?;
            fs::write(&token_path, new_token)
                .with_context(|| format!("Failed to write {token_path:?}"))?;
            registration
        }
        Err(e) => return Err(e).with_context(|| format!("Failed to read {token_path:?}")),
    };

    // Send registration.fcm_token to the server to allow it to send push messages to you.
    info!("registration.fcm_token: {}", registration.fcm_token);
    let uid = "6A5443E486511B0D828543445DC55A7D"; // MD5 Hash of "WHY_REOLINK"
    camera
        .send_pushinfo_android(&registration.fcm_token, uid)
        .await?;

    info!("Listening");
    let session = registration.gcm.checkin(&http).await?;
    let connection = session.new_connection(vec![]).await?;
    let mut stream = MessageStream::wrap(connection, &registration.keys);
    while let Some(message) = stream.next().await {
        match message? {
            Message::Data(data) => {
                info!("Message JSON: {}", String::from_utf8_lossy(&data.body));
                info!("Persistent ID: {:?}", data.persistent_id);
            }
            Message::HeartbeatPing => {
                // Unacknowledged heartbeats stop the pushes within the hour
                stream
                    .write_all(&new_heartbeat_ack())
                    .await
                    .context("Failed to acknowledge a heartbeat")?;
            }
            Message::Other(tag, _) => debug!("Server message with tag {tag}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use fcm_push_listener::{Registration, Session, WebPushKeys};

    #[test]
    fn test_token_json_round_trip() {
        let registration = Registration {
            fcm_token: "token".to_owned(),
            gcm: Session {
                android_id: 4_611_686_018_427_387_905,
                security_token: 18_446_744_073_709_551_615,
            },
            keys: WebPushKeys {
                public_key: vec![4, 1, 2],
                private_key: vec![9, 8],
                auth_secret: vec![0, 255],
            },
        };
        let text = serde_json::to_string_pretty(&registration).unwrap();
        let parsed: Registration = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.fcm_token, registration.fcm_token);
        assert_eq!(parsed.gcm.android_id, registration.gcm.android_id);
        assert_eq!(parsed.gcm.security_token, registration.gcm.security_token);
        assert_eq!(parsed.keys.public_key, registration.keys.public_key);
        assert_eq!(parsed.keys.private_key, registration.keys.private_key);
        assert_eq!(parsed.keys.auth_secret, registration.keys.auth_secret);
    }
}
