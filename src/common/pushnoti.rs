//! This thread handles push notifications
//! the last notification is pushed into a watcher
//! as is, which comes fromt the json structure
//!

use anyhow::{anyhow, Context};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use fcm_push_listener::{
    new_heartbeat_ack, DataMessage, Message, MessageStream, Registration, Session, WebPushKeys,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::path::{Path, PathBuf};
use std::{fs, sync::Arc};
use tokio::{
    io::AsyncWriteExt,
    sync::{
        mpsc::{Receiver as MpscReceiver, Sender as MpscSender},
        oneshot::Sender as OneshotSender,
        watch::{channel as watch, Receiver as WatchReceiver, Sender as WatchSender},
        RwLock,
    },
    time::{sleep, timeout, Duration},
};
use tokio_stream::StreamExt;

use super::NeoInstance;
use crate::AnyResult;

// The Firebase app of Reolink's Android app (its google-services values, which every copy
// of the app carries; see crates/pushnoti/README.md). Cameras push through Reolink's
// servers to FCM tokens of this app.
const FIREBASE_APP_ID: &str = "1:743639030586:android:86f60a4fb7143876";
const FIREBASE_PROJECT_ID: &str = "reolink-login";
const FIREBASE_API_KEY: &str = "AIzaSyBEUIuWHnnOEwFahxWgQB4Yt4NsgOmkPyE";

/// The value of `scheme` in the saved registration for a Firebase installation
/// registration (fcm-push-listener >= 3). Files written before have no `scheme`: their
/// sender-ID registration is one Google no longer serves, so they are registered again.
const SCHEME_FIREBASE: &str = "firebase";

/// How long one connection to the push server is kept before reconnecting
const CONNECTION_LIFETIME: Duration = Duration::from_secs(60 * 5);

pub(crate) struct PushNotiThread {
    pn_watcher: Arc<WatchSender<Option<PushNoti>>>,
    registed_cameras: HashMap<String, NeoInstance>,
    received_ids: Arc<RwLock<HashSet<String>>>,
}

// The push notification
#[derive(Clone, Eq, PartialEq, Debug)]
pub(crate) struct PushNoti {
    pub(crate) message: String,
    pub(crate) id: Option<String>,
}

impl TryFrom<DataMessage> for PushNoti {
    type Error = std::string::FromUtf8Error;

    /// The decrypted body is the JSON of the push
    fn try_from(data: DataMessage) -> Result<Self, Self::Error> {
        Ok(PushNoti {
            message: String::from_utf8(data.body)?,
            id: data.persistent_id,
        })
    }
}

pub(crate) enum PnRequest {
    Get {
        sender: OneshotSender<WatchReceiver<Option<PushNoti>>>,
    },
    Activate {
        instance: NeoInstance,
        sender: OneshotSender<AnyResult<()>>,
    },
    AddPushID {
        id: String,
    },
}

/// The registration as saved in `neolink_token.toml`. The layout is the one
/// fcm-push-listener 2 wrote (numbers as strings, keys in URL-safe base64 without
/// padding), plus `scheme`.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct SavedRegistration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scheme: Option<String>,
    fcm_token: String,
    gcm: SavedGcm,
    keys: SavedKeys,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct SavedGcm {
    android_id: String,
    security_token: String,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct SavedKeys {
    public_key: String,
    private_key: String,
    auth_secret: String,
}

impl From<&Registration> for SavedRegistration {
    fn from(registration: &Registration) -> Self {
        SavedRegistration {
            scheme: Some(SCHEME_FIREBASE.to_owned()),
            fcm_token: registration.fcm_token.clone(),
            gcm: SavedGcm {
                android_id: registration.gcm.android_id.to_string(),
                security_token: registration.gcm.security_token.to_string(),
            },
            keys: SavedKeys {
                public_key: URL_SAFE_NO_PAD.encode(&registration.keys.public_key),
                private_key: URL_SAFE_NO_PAD.encode(&registration.keys.private_key),
                auth_secret: URL_SAFE_NO_PAD.encode(&registration.keys.auth_secret),
            },
        }
    }
}

impl SavedRegistration {
    /// The registration, or `None` for one from before the Firebase scheme
    fn into_registration(self) -> AnyResult<Option<Registration>> {
        match self.scheme.as_deref() {
            Some(SCHEME_FIREBASE) => {}
            None => return Ok(None),
            Some(other) => return Err(anyhow!("Unknown registration scheme {other:?}")),
        }
        let decode = |name: &str, value: &str| {
            URL_SAFE_NO_PAD
                .decode(value)
                .with_context(|| format!("Invalid {name}"))
        };
        Ok(Some(Registration {
            fcm_token: self.fcm_token,
            gcm: Session {
                android_id: self.gcm.android_id.parse().context("Invalid android_id")?,
                security_token: self
                    .gcm
                    .security_token
                    .parse()
                    .context("Invalid security_token")?,
            },
            keys: WebPushKeys {
                public_key: decode("public_key", &self.keys.public_key)?,
                private_key: decode("private_key", &self.keys.private_key)?,
                auth_secret: decode("auth_secret", &self.keys.auth_secret)?,
            },
        }))
    }
}

/// Parses a saved registration: `None` when it must be registered again
fn parse_registration(text: &str) -> AnyResult<Option<Registration>> {
    toml::from_str::<SavedRegistration>(text)
        .context("Unable to parse the saved push notification registration")?
        .into_registration()
}

fn serialise_registration(registration: &Registration) -> AnyResult<String> {
    toml::to_string(&SavedRegistration::from(registration))
        .context("Unable to serialise the push notification registration")
}

/// Loads the saved registration, if there is a usable one
fn load_registration(token_path: &Path) -> Option<Registration> {
    let text = match fs::read_to_string(token_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            log::warn!("Unable to read the push notification registration {token_path:?}: {e}");
            return None;
        }
    };
    if text.trim().is_empty() {
        // Cleared after a listener error
        return None;
    }
    match parse_registration(&text) {
        Ok(Some(registration)) => Some(registration),
        Ok(None) => {
            log::info!(
                "The push notification registration in {token_path:?} uses the old sender ID scheme, which Google no longer serves. Registering again"
            );
            None
        }
        Err(e) => {
            log::warn!("{e:#} ({token_path:?}). Registering again");
            None
        }
    }
}

fn save_registration(token_path: Option<&Path>, registration: &Registration) {
    let Some(token_path) = token_path else {
        return;
    };
    let result = serialise_registration(registration)
        .and_then(|text| fs::write(token_path, text).map_err(anyhow::Error::from));
    if let Err(e) = result {
        log::warn!("Unable to save the push notification registration to {token_path:?}: {e:#}");
    }
}

/// `<config dir>/neolink/neolink_token.toml`, or `None` (registration kept in memory
/// only) when there is no config directory or it can't be created
fn token_path() -> Option<PathBuf> {
    let Some(mut dir) = dirs::config_dir() else {
        log::warn!("No config directory: the push notification registration is not saved");
        return None;
    };
    dir.push("neolink");
    if let Err(e) = fs::create_dir_all(&dir) {
        log::warn!(
            "Unable to create {dir:?} for the push notification registration, which is not saved: {e}"
        );
        return None;
    }
    dir.push("neolink_token.toml");
    Some(dir)
}

/// What to do after the push listener stops with an error
#[derive(Debug, PartialEq, Eq)]
enum ListenerErrorAction {
    /// A message could not be decoded or decrypted with the saved keys: register again
    ClearRegistration,
    /// A network failure: reconnect after the usual short wait
    Reconnect,
    /// Anything else: wait a little longer before reconnecting
    Wait,
}

fn listener_error_action(e: &fcm_push_listener::Error) -> ListenerErrorAction {
    use fcm_push_listener::Error::*;
    match e {
        EmptyPayload
        | MissingCryptoMetadata(_)
        | ProtobufDecode(_, _)
        | Base64Decode(_, _)
        | Crypto(_, _) => ListenerErrorAction::ClearRegistration,
        Request(_, e) | Response(_, e) if e.is_request() || e.is_connect() || e.is_timeout() => {
            ListenerErrorAction::Reconnect
        }
        _ => ListenerErrorAction::Wait,
    }
}

impl PushNotiThread {
    pub(crate) async fn new() -> AnyResult<Self> {
        let (pn_watcher, _) = watch(None);

        Ok(PushNotiThread {
            pn_watcher: Arc::new(pn_watcher),
            registed_cameras: Default::default(),
            received_ids: Arc::new(RwLock::new(Default::default())),
        })
    }

    pub(crate) async fn run(
        &mut self,
        sender: &MpscSender<PnRequest>,
        pn_request_rx: &mut MpscReceiver<PnRequest>,
    ) -> AnyResult<()> {
        let http = reqwest::Client::new();
        loop {
            // Short wait on start/retry
            sleep(Duration::from_secs(3)).await;

            let token_path = token_path();
            log::debug!("Push notification details are saved to {:?}", token_path);

            let mut registration =
                if let Some(registration) = token_path.as_deref().and_then(load_registration) {
                    log::debug!("Loaded push notification token");
                    registration
                } else {
                    log::debug!("Registering new push notification token");
                    match fcm_push_listener::register(
                        &http,
                        FIREBASE_APP_ID,
                        FIREBASE_PROJECT_ID,
                        FIREBASE_API_KEY,
                        None,
                    )
                    .await
                    {
                        Ok(registration) => {
                            save_registration(token_path.as_deref(), &registration);
                            registration
                        }
                        Err(e) => {
                            log::warn!("Issue connecting to push notifications server: {e}");
                            sleep(Duration::from_secs(30)).await;
                            continue;
                        }
                    }
                };

            // Send registration.fcm_token to the server to allow it to send push messages to you.
            log::debug!("registration.fcm_token: {}", registration.fcm_token);
            let md5ed = md5::compute(format!("WHY_REOLINK_{:?}", registration.fcm_token));
            let uid = format!("{:X}", md5ed);
            let fcm_token = registration.fcm_token.clone();
            log::debug!("push notification UID: {}", uid);

            log::debug!("Push notification Listening");
            let thread_pn_watcher = self.pn_watcher.clone();

            for (_, instance) in self.registed_cameras.iter() {
                let uid = uid.clone();
                let fcm_token = fcm_token.clone();
                let instance = instance.clone();
                // Passive: registration rides on the next wake instead of causing one
                // (SPEC P1, P10)
                tokio::task::spawn(async move {
                    let _ = instance
                        .run_passive_task(|camera| {
                            let fcm_token = fcm_token.clone();
                            let uid = uid.clone();
                            Box::pin(async move {
                                let r = camera.send_pushinfo_android(&fcm_token, &uid).await;
                                log::debug!(
                                    "Registered {} for push notifications: {:?}",
                                    camera.uid().await?,
                                    r
                                );
                                r?;
                                AnyResult::Ok(())
                            })
                        })
                        .await;
                });
            }

            let received_ids = self.received_ids.clone();
            tokio::select! {
                v = async {
                    loop {
                        let received: Vec<String> = received_ids.read().await.iter().cloned().collect();
                        let r = timeout(
                            CONNECTION_LIFETIME,
                            listen(&http, &mut registration, token_path.as_deref(), received, |data| {
                                log::debug!("Got FCM Message: {:?}", String::from_utf8_lossy(&data.body));
                                let push = match PushNoti::try_from(data) {
                                    Ok(push) => push,
                                    Err(e) => {
                                        log::warn!("Push notification is not UTF-8, ignored: {e}");
                                        return;
                                    }
                                };
                                if let Some(id) = push.id.clone() {
                                    // Don't worry if queue is full we will just not register as received yet
                                    let _ = sender.try_send(PnRequest::AddPushID { id });
                                }
                                thread_pn_watcher.send_replace(Some(push));
                            }),
                        )
                        .await;
                        match &r {
                            Ok(Ok(_)) => {
                                log::debug!("Push notification listener reported normal shutdown");
                            }
                            Ok(Err(e)) => match listener_error_action(e) {
                                ListenerErrorAction::ClearRegistration => {
                                    // Wipe data so next call is a new token
                                    if let Some(token_path) = token_path.as_deref() {
                                        if let Err(e) = fs::write(token_path, "") {
                                            log::warn!("Unable to clear the push notification registration {token_path:?}: {e}");
                                        }
                                    }
                                    log::debug!("Error on push notification listener: {:?}. Clearing token", e);
                                }
                                ListenerErrorAction::Reconnect => {
                                    log::debug!("Error on push notification listener: {:?}", e);
                                }
                                ListenerErrorAction::Wait => {
                                    log::debug!("Error on push notification listener: {:?}", e);
                                    // This sort of error can be a network error
                                    // Wait for a little longer
                                    sleep(Duration::from_secs(30)).await;
                                }
                            },
                            Err(_) => {
                                // timeout
                                continue;
                            }
                        };
                        break;
                    }
                } => v,
                v = async {
                    while let Some(msg) = pn_request_rx.recv().await {
                        match msg {
                            PnRequest::Get{sender} => {
                                let _ = sender.send(self.pn_watcher.subscribe());
                            }
                            PnRequest::Activate{instance, sender} => {
                                let uid = uid.clone();
                                let fcm_token = fcm_token.clone();
                                self.registed_cameras.insert(uid.clone(), instance.clone());
                                tokio::task::spawn(async move {
                                    let r = instance.run_passive_task(|camera| {
                                        let fcm_token = fcm_token.clone();
                                        let uid = uid.clone();
                                        Box::pin(async move {
                                            let r = camera.send_pushinfo_android(&fcm_token, &uid).await;
                                            log::debug!(
                                                "Registered {} for push notifications: {:?}",
                                                camera.uid().await?,
                                                r
                                            );
                                            r?;
                                            AnyResult::Ok(())
                                        })
                                    }).await;
                                    let _ = sender.send(r);
                                });
                            }
                            PnRequest::AddPushID{id} => {
                                log::trace!("Recived Push Notifcation of ID: {id}");
                                received_ids.write().await.insert(id);
                            }
                        }
                    }
                } => {
                    // These are critical errors
                    log::debug!("Push Notification thread ended {:?}", v);
                    break AnyResult::Ok(());
                },
            };
        }
    }
}

/// One connection to the push server: checks in (saving a renewed session), logs in
/// acknowledging `received_ids`, then passes each data message to `on_message` and acks
/// the server's heartbeats until the server closes the connection or an error
async fn listen(
    http: &reqwest::Client,
    registration: &mut Registration,
    token_path: Option<&Path>,
    received_ids: Vec<String>,
    mut on_message: impl FnMut(DataMessage),
) -> Result<(), fcm_push_listener::Error> {
    let session = registration.gcm.checkin(http).await?;
    if session.changed(&registration.gcm) {
        log::debug!("Push notification session renewed");
        registration.gcm = (*session).clone();
        save_registration(token_path, registration);
    }
    let connection = session.new_connection(received_ids).await?;
    let mut stream = MessageStream::wrap(connection, &registration.keys);
    while let Some(message) = stream.next().await {
        match message? {
            Message::Data(data) => on_message(data),
            Message::HeartbeatPing => {
                log::trace!("Push notification heartbeat");
                stream
                    .write_all(&new_heartbeat_ack())
                    .await
                    .map_err(fcm_push_listener::Error::Socket)?;
            }
            Message::Other(tag, _) => {
                log::trace!("Push notification server message with tag {tag}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registration() -> Registration {
        Registration {
            fcm_token: "token:abc".to_owned(),
            gcm: Session {
                // Beyond f64 precision: must survive as a string
                android_id: 4_611_686_018_427_387_905,
                security_token: 18_446_744_073_709_551_615,
            },
            keys: WebPushKeys {
                public_key: vec![4, 1, 2, 3, 250],
                private_key: vec![9, 8, 7],
                auth_secret: vec![0, 255, 16],
            },
        }
    }

    fn assert_same(a: &Registration, b: &Registration) {
        assert_eq!(a.fcm_token, b.fcm_token);
        assert_eq!(a.gcm.android_id, b.gcm.android_id);
        assert_eq!(a.gcm.security_token, b.gcm.security_token);
        assert_eq!(a.keys.public_key, b.keys.public_key);
        assert_eq!(a.keys.private_key, b.keys.private_key);
        assert_eq!(a.keys.auth_secret, b.keys.auth_secret);
    }

    #[test]
    fn test_registration_round_trip() {
        let text = serialise_registration(&registration()).unwrap();
        let parsed = parse_registration(&text).unwrap().unwrap();
        assert_same(&parsed, &registration());
    }

    #[test]
    fn test_registration_layout() {
        // fcm-push-listener 2's layout plus `scheme`
        let text = serialise_registration(&registration()).unwrap();
        let expected = r#"scheme = "firebase"
fcm_token = "token:abc"

[gcm]
android_id = "4611686018427387905"
security_token = "18446744073709551615"

[keys]
public_key = "BAECA_o"
private_key = "CQgH"
auth_secret = "AP8Q"
"#;
        assert_eq!(text, expected);
    }

    #[test]
    fn test_registration_layout_matches_fcm_push_listener() {
        // The fields carry the same values as fcm-push-listener's own serde form
        let ours: serde_json::Value =
            serde_json::to_value(SavedRegistration::from(&registration())).unwrap();
        let theirs = serde_json::to_value(registration()).unwrap();
        for (table, key) in [
            ("gcm", "android_id"),
            ("gcm", "security_token"),
            ("keys", "public_key"),
            ("keys", "private_key"),
            ("keys", "auth_secret"),
        ] {
            assert_eq!(ours[table][key], theirs[table][key], "{table}.{key}");
        }
        assert_eq!(ours["fcm_token"], theirs["fcm_token"]);
    }

    #[test]
    fn test_registration_from_fcm_push_listener_2_is_registered_again() {
        let text = r#"fcm_token = "old"

[gcm]
android_id = "1"
security_token = "2"

[keys]
public_key = "AQ"
private_key = "Ag"
auth_secret = "Aw"
"#;
        assert!(parse_registration(text).unwrap().is_none());
    }

    #[test]
    fn test_registration_errors() {
        // Cleared file, truncated file, bad base64, bad number, unknown scheme
        assert!(parse_registration("").is_err());
        assert!(parse_registration("scheme = \"firebase\"\nfcm_token = \"a\"").is_err());
        let good = serialise_registration(&registration()).unwrap();
        assert!(parse_registration(&good.replace("AP8Q", "A+/=")).is_err());
        assert!(parse_registration(&good.replace("\"4611686018427387905\"", "\"x\"")).is_err());
        assert!(parse_registration(&good.replace("firebase", "sender-id")).is_err());
    }

    #[test]
    fn test_push_noti_from_data_message() {
        let push = PushNoti::try_from(DataMessage {
            body: br#"{"data":{"ALMTYPE":"MD"}}"#.to_vec(),
            persistent_id: Some("0:1%abc".to_owned()),
        })
        .unwrap();
        assert_eq!(
            push,
            PushNoti {
                message: r#"{"data":{"ALMTYPE":"MD"}}"#.to_owned(),
                id: Some("0:1%abc".to_owned()),
            }
        );
        assert!(PushNoti::try_from(DataMessage {
            body: vec![0xff, 0xfe],
            persistent_id: None,
        })
        .is_err());
    }

    #[test]
    fn test_listener_error_action() {
        use fcm_push_listener::Error;
        assert_eq!(
            listener_error_action(&Error::EmptyPayload),
            ListenerErrorAction::ClearRegistration
        );
        assert_eq!(
            listener_error_action(&Error::MissingCryptoMetadata("crypto-key")),
            ListenerErrorAction::ClearRegistration
        );
        let b64 = URL_SAFE_NO_PAD.decode("!").unwrap_err();
        assert_eq!(
            listener_error_action(&Error::Base64Decode("x", b64)),
            ListenerErrorAction::ClearRegistration
        );
        assert_eq!(
            listener_error_action(&Error::Socket(std::io::Error::other("reset"))),
            ListenerErrorAction::Wait
        );
        assert_eq!(
            listener_error_action(&Error::DependencyFailure("GCM checkin", "no id")),
            ListenerErrorAction::Wait
        );
    }

    #[tokio::test]
    async fn test_listener_error_action_network() {
        // A connection refused by a local port nothing listens on
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let e = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap_err();
        assert_eq!(
            listener_error_action(&fcm_push_listener::Error::Request("x", e)),
            ListenerErrorAction::Reconnect
        );
    }
}
