use super::backoff::Backoff;
use crate::{
    config::{Config, MqttServerConfig},
    AnyResult,
};
use anyhow::{anyhow, Context, Result};
use futures::future::FutureExt;
use log::*;
use rumqttc::{
    AsyncClient, ConnectReturnCode, Event, EventLoop, Incoming, LastWill, MqttOptions, QoS,
    TlsConfiguration, Transport,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::task::JoinSet;
use tokio::{
    sync::{
        broadcast::{channel as broadcast, Sender as BroadcastSender},
        mpsc::{channel as mpsc, Receiver as MpscReceiver, Sender as MpscSender},
        oneshot::{channel as oneshot, Sender as OneshotSender},
        watch::Receiver as WatchReceiver,
        Mutex,
    },
    time::{sleep, Duration},
};
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Delays between attempts to reach the broker
const BROKER_RETRY_MIN: Duration = Duration::from_secs(1);
const BROKER_RETRY_MAX: Duration = Duration::from_secs(60);

/// The last retained payload of each topic neolink publishes. Every (re)connection to the
/// broker publishes them again, so a broker that restarted without persistence, or a
/// retained publish lost with a dropped connection, is restored, still retained
type RetainedCache = Arc<Mutex<HashMap<String, Arc<String>>>>;

pub(crate) struct Mqtt {
    cancel: CancellationToken,
    outgoing_tx: MpscSender<MqttRequest>,
    set: JoinSet<Result<()>>,
}

impl Mqtt {
    pub(crate) async fn new(config: WatchReceiver<Config>) -> Self {
        let (incoming_tx, _) = broadcast::<MqttReply>(100);
        let (outgoing_tx, mut outgoing_rx) = mpsc::<MqttRequest>(100);
        let cancel = CancellationToken::new();
        let mut set = JoinSet::<AnyResult<()>>::new();
        let retained = RetainedCache::default();

        // Thread that handles the mqttc side
        // including restarting it if the config changes
        let thread_cancel = cancel.clone();
        let mut thread_config = config;
        let thread_incoming_tx = incoming_tx;
        let thread_outgoing_tx = outgoing_tx.clone();
        set.spawn(async move {
            let mut mqtt_config = thread_config.borrow().mqtt.clone();
            // Only for a client that cannot start (an unreadable CA or client key): an
            // unreachable broker is retried by the backend itself
            let mut backoff = Backoff::new(BROKER_RETRY_MIN, BROKER_RETRY_MAX);
            let r = loop {
                break tokio::select! {
                    _ = thread_cancel.cancelled() => AnyResult::Ok(()),
                    v = thread_config.wait_for(|config| config.mqtt != mqtt_config).map(|res| res.map(|r| r.clone())) =>
                    {
                        mqtt_config = v?.mqtt.clone();
                        backoff.reset();
                        continue;
                    }
                    v = async {
                        let mut backend = MqttBackend {
                            incomming_tx: thread_incoming_tx.clone(),
                            outgoing_rx: &mut outgoing_rx,
                            outgoing_tx: thread_outgoing_tx.clone(),
                            config: mqtt_config.as_ref().unwrap(),
                            cancel: CancellationToken::new(),
                            retained: retained.clone(),
                        };
                        backend.run().await
                    }, if mqtt_config.is_some() => {
                        if let Err(e) = &v {
                            let delay = backoff.next_delay();
                            log::error!("MQTT client failed, retrying in {delay:?}: {e:?}");
                            sleep(delay).await;
                            continue;
                        }
                        v
                    },
                };
            };
            log::debug!("MQTT thread stopped: {:?}", r);
            r
        });

        Self {
            cancel,
            outgoing_tx,
            set,
            // Send the drop message on clean disconnect
        }
    }

    pub async fn subscribe<T: Into<String>>(&self, name: T) -> AnyResult<MqttInstance> {
        let (tx, rx) = oneshot();
        self.outgoing_tx
            .send(MqttRequest::Subscribe(name.into(), tx))
            .await?;
        rx.await?
    }
}

impl Drop for Mqtt {
    fn drop(&mut self) {
        log::trace!("Drop MQTT");
        let outgoing_tx = self.outgoing_tx.clone();
        let cancel = self.cancel.clone();
        let mut set = std::mem::take(&mut self.set);

        let _gt = tokio::runtime::Handle::current().enter();
        tokio::task::spawn(async move {
            let (tx, rx) = oneshot();
            let _ = outgoing_tx.send(MqttRequest::HangUp(tx)).await;
            let _ = rx.await;

            log::debug!("Mqtt::drop Cancel");
            cancel.cancel();
            while set.join_next().await.is_some() {}
            log::trace!("Dropped MQTT");
        });
    }
}

struct MqttBackend<'a> {
    incomming_tx: BroadcastSender<MqttReply>,
    outgoing_rx: &'a mut MpscReceiver<MqttRequest>,
    outgoing_tx: MpscSender<MqttRequest>,
    config: &'a MqttServerConfig,
    cancel: CancellationToken,
    retained: RetainedCache,
}

impl<'a> MqttBackend<'a> {
    /// Runs one client until the requests channel closes. An unreachable broker is no
    /// error: the client keeps what it was asked to publish and retries the broker with
    /// backoff (rumqttc reconnects on the next poll); only a client that cannot be set up
    /// is an error
    async fn run(&mut self) -> AnyResult<()> {
        log::trace!("Run MQTT Server");
        let mut mqttoptions = MqttOptions::new(
            format!("Neolink{}", Uuid::new_v4()),
            &self.config.broker_addr,
            self.config.port,
        );
        let max_size = 100 * (1024 * 1024);
        mqttoptions.set_max_packet_size(max_size, max_size);

        // Use TLS if ca path is set. A configured but unreadable CA or client key is an
        // error, never a silent fall back to plaintext (which would send the broker
        // credentials and config traffic in clear)
        if let Some(ca_path) = &self.config.ca {
            let ca = std::fs::read(ca_path).with_context(|| {
                format!("MQTT: cannot read the CA {ca_path:?} (with PUID, it must be readable by that user)")
            })?;
            let client_auth = match &self.config.client_auth {
                Some((cert_path, key_path)) => Some((
                    std::fs::read(cert_path).with_context(|| {
                        format!("MQTT: cannot read the client cert {cert_path:?}")
                    })?,
                    std::fs::read(key_path).with_context(|| {
                        format!("MQTT: cannot read the client key {key_path:?}")
                    })?,
                )),
                None => None,
            };
            let transport = Transport::Tls(TlsConfiguration::Simple {
                ca,
                alpn: None,
                client_auth,
            });
            mqttoptions.set_transport(transport);
        };

        if let Some((username, password)) = &self.config.credentials {
            mqttoptions.set_credentials(username, password);
        }

        mqttoptions.set_keep_alive(Duration::from_secs(5));

        // On unclean disconnect send this
        mqttoptions.set_last_will(LastWill::new(
            "neolink/status".to_string(),
            "offline",
            QoS::AtLeastOnce,
            true,
        ));

        let (client, connection) = AsyncClient::new(mqttoptions, 100);
        client
            .publish(
                "neolink/status".to_string(),
                QoS::AtLeastOnce,
                true,
                "connected".to_string(),
            )
            .await?;
        log::debug!("MQTT Published Startup");

        let broker = format!("{}:{}", self.config.broker_addr, self.config.port);
        // Requests are handled while the broker is unreachable too: publishes wait in the
        // client, subscriptions to the incoming messages need no broker
        let requests = handle_requests(
            self.outgoing_rx,
            &client,
            &self.outgoing_tx,
            &self.incomming_tx,
            self.config,
            &self.retained,
        );
        let connection = poll_connection(
            connection,
            &client,
            &broker,
            &self.incomming_tx,
            &self.cancel,
            &self.retained,
        );
        tokio::select! {
            v = requests => v,
            v = connection => v,
        }
    }
}

async fn handle_requests(
    outgoing_rx: &mut MpscReceiver<MqttRequest>,
    client: &AsyncClient,
    outgoing_tx: &MpscSender<MqttRequest>,
    incomming_tx: &BroadcastSender<MqttReply>,
    server_config: &MqttServerConfig,
    retained: &RetainedCache,
) -> AnyResult<()> {
    loop {
        let msg = outgoing_rx
            .recv()
            .await
            .ok_or(anyhow!("All outgoing MQTT channels closed"))?;

        // Put it on a thread so that we don't block polling. It is not cancelled when
        // this client ends: a publish then fails and is requeued for the next client, so
        // the sender still gets its reply
        let outgoing_tx = outgoing_tx.clone();
        let incomming_tx = incomming_tx.clone();
        let client = client.clone();
        let server_config = server_config.clone();
        let retained = retained.clone();
        tokio::task::spawn(async move {
            match msg {
                MqttRequest::Publish(publish) => {
                    send_publish(publish, &client, &outgoing_tx, &retained).await?;
                }
                MqttRequest::HangUp(reply) => {
                    // The same payload as the last will, which HA's
                    // availability expects
                    client
                        .publish(
                            "neolink/status".to_string(),
                            QoS::AtLeastOnce,
                            true,
                            "offline".to_string(),
                        )
                        .await?;
                    let _ = reply.send(());
                    return Err(anyhow!("Disconneting"));
                }
                MqttRequest::Subscribe(name, reply) => {
                    let instance = MqttInstance {
                        name,
                        incomming_rx: BroadcastStream::new(incomming_tx.subscribe()),
                        outgoing_tx: outgoing_tx.clone(),
                    };
                    let _ = reply.send(Ok(instance));
                }
                MqttRequest::LastWill {
                    topic,
                    message,
                    reply,
                } => {
                    let last_will = LastWillMqtt::new(&server_config, topic, message).await;
                    let _ = reply.send(last_will);
                }
            }
            AnyResult::Ok(())
        });
    }
}

/// Polls the broker connection for as long as the client runs. A lost or refused
/// connection is retried with backoff: rumqttc reconnects on the next poll and sends what
/// was published meanwhile
async fn poll_connection(
    mut connection: EventLoop,
    client: &AsyncClient,
    broker: &str,
    incomming_tx: &BroadcastSender<MqttReply>,
    cancel: &CancellationToken,
    retained: &RetainedCache,
) -> AnyResult<()> {
    let mut backoff = Backoff::new(BROKER_RETRY_MIN, BROKER_RETRY_MAX);
    let mut failing = false;
    loop {
        let notification = match connection.poll().await {
            Ok(notification) => notification,
            Err(e) => {
                let delay = backoff.next_delay();
                log::warn!("MQTT: cannot reach the broker at {broker}, retrying in {delay:?}: {e}");
                failing = true;
                sleep(delay).await;
                continue;
            }
        };
        if let Event::Incoming(Incoming::ConnAck(connected)) = &notification {
            if connected.code == ConnectReturnCode::Success {
                backoff.reset();
                if std::mem::take(&mut failing) {
                    log::info!("MQTT: connected to the broker at {broker}");
                }
            }
        }
        // Handle message on another thread so that we can keep polling
        let client = client.clone();
        let incomming_tx = incomming_tx.clone();
        let cancel = cancel.clone();
        let retained = retained.clone();
        tokio::task::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => AnyResult::Ok(()),
                v = handle_notification(notification, &client, &incomming_tx, &retained) => v,
            }
        });
    }
}

async fn handle_notification(
    notification: Event,
    client: &AsyncClient,
    incomming_tx: &BroadcastSender<MqttReply>,
    retained: &RetainedCache,
) -> AnyResult<()> {
    match notification {
        Event::Incoming(Incoming::ConnAck(connected)) => {
            if ConnectReturnCode::Success == connected.code {
                // Publish connected now that we are online
                client
                    .publish(
                        "neolink/status".to_string(),
                        QoS::AtLeastOnce,
                        true,
                        "connected",
                    )
                    .await?;
                // A restarted broker may have lost the retained topics (camera status,
                // discovery, config): publish them again
                republish_retained(client, retained).await?;
                // We succesfully logged in. Now ask for the cameras subscription.
                client
                    .subscribe("neolink/#".to_string(), QoS::AtMostOnce)
                    .await?;
            }
        }
        Event::Incoming(Incoming::Publish(published_message)) => {
            // Retained commands are replays from before we started:
            // acting on them on every broker reconnect would wake
            // battery cameras or undo settings (SPEC P14)
            let stale = published_message.retain && is_command_topic(&published_message.topic);
            if stale {
                log::warn!(
                    "Ignoring retained MQTT message on {}; publish commands without retain",
                    published_message.topic
                );
            }
            if let Some(sub_topic) = published_message
                .topic
                .strip_prefix("neolink/")
                .filter(|_| !stale)
            {
                let _ = incomming_tx.send(MqttReply {
                    topic: sub_topic.to_string(),
                    message: Arc::new(
                        String::from_utf8_lossy(published_message.payload.as_ref()).into_owned(),
                    ),
                });
            }
        }
        _ => {}
    }
    AnyResult::Ok(())
}

/// Publishes `publish` (QoS 1). A retained one is remembered first: if this client is gone
/// it is published, still retained, on the next connection. Any other is requeued for the
/// next client. The sender's reply is sent once it is queued in a client
async fn send_publish(
    publish: Publish,
    client: &AsyncClient,
    outgoing_tx: &MpscSender<MqttRequest>,
    retained: &RetainedCache,
) -> AnyResult<()> {
    let Publish { msg, retain, reply } = publish;
    let sent = if retain {
        // Held while publishing, so that a reconnection's republish cannot overtake it
        // with the previous payload
        let mut cache = retained.lock().await;
        cache.insert(msg.topic.clone(), msg.message.clone());
        client
            .publish(
                msg.topic.clone(),
                QoS::AtLeastOnce,
                true,
                (*msg.message).clone(),
            )
            .await
    } else {
        client
            .publish(
                msg.topic.clone(),
                QoS::AtLeastOnce,
                false,
                (*msg.message).clone(),
            )
            .await
    };
    match sent {
        Ok(()) => {
            let _ = reply.send(Ok(()));
        }
        // This client is gone (the config changed): the cache publishes it on the next
        // connection
        Err(_) if retain => {
            let _ = reply.send(Ok(()));
        }
        Err(_) => {
            outgoing_tx
                .send(MqttRequest::Publish(Publish { msg, retain, reply }))
                .await?;
        }
    }
    Ok(())
}

/// Publishes every remembered retained topic again, retained
async fn republish_retained(client: &AsyncClient, retained: &RetainedCache) -> AnyResult<()> {
    let cache = retained.lock().await;
    for (topic, message) in cache.iter() {
        client
            .publish(topic.clone(), QoS::AtLeastOnce, true, (**message).clone())
            .await?;
    }
    Ok(())
}

/// Topics that make neolink act: `neolink/config`, `neolink/<cam>/control/..`,
/// `neolink/<cam>/query/..`
fn is_command_topic(topic: &str) -> bool {
    let Some(sub) = topic.strip_prefix("neolink/") else {
        return false;
    };
    sub == "config"
        || sub
            .split('/')
            .nth(1)
            .is_some_and(|t| t == "control" || t == "query")
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_is_command_topic() {
        use super::is_command_topic;
        assert!(is_command_topic("neolink/config"));
        assert!(is_command_topic("neolink/argus/control/wakeup"));
        assert!(is_command_topic("neolink/argus/query/battery"));
        assert!(!is_command_topic("neolink/argus/status/battery_level"));
        assert!(!is_command_topic("neolink/status"));
        assert!(!is_command_topic("other/argus/control/x"));
    }

    #[test]
    fn test_rustls_has_a_crypto_provider() {
        // rumqttc builds the MQTT TLS config with `ClientConfig::builder()`, which panics
        // when rustls has no crypto provider compiled in (Cargo.toml: rustls with ring)
        let _ = rustls::ClientConfig::builder();
    }

    /// A config whose broker is a local port nothing listens on
    fn unreachable_broker() -> crate::config::Config {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        toml::from_str(&format!(
            "cameras = []\n[mqtt]\nbroker_addr = \"127.0.0.1\"\nport = {port}\n"
        ))
        .unwrap()
    }

    /// While the broker is down, sends wait for it instead of failing: a failed send ends
    /// `neolink mqtt` (exit 1) or restarts a camera's listener
    #[tokio::test]
    async fn test_sends_wait_for_an_unreachable_broker() {
        use super::Mqtt;
        let (_config_tx, config) = tokio::sync::watch::channel(unreachable_broker());
        let mqtt = Mqtt::new(config).await;
        let instance = std::sync::Arc::new(mqtt.subscribe("cam").await.unwrap());
        // More than the client's request channel holds (100), retained and not
        let mut set = tokio::task::JoinSet::new();
        for i in 0..300 {
            let instance = instance.clone();
            set.spawn(async move {
                instance
                    .send_message(&format!("t{i}"), "x", i % 2 == 0)
                    .await
            });
        }
        // They may wait, but none fails
        let waited = tokio::time::timeout(std::time::Duration::from_secs(8), async {
            while let Some(r) = set.join_next().await {
                r.unwrap()?;
            }
            crate::AnyResult::Ok(())
        })
        .await;
        if let Ok(r) = waited {
            r.expect("a send failed while the broker was down");
        }
    }

    /// A retained publish whose client went away is published again by the next
    /// connection, still retained and QoS 1, and never requeued as a plain publish
    #[tokio::test]
    async fn test_retained_publish_survives_a_lost_client() {
        use super::*;
        let (outgoing_tx, mut outgoing_rx) = mpsc::<MqttRequest>(10);
        let retained = RetainedCache::default();
        // A client whose event loop is gone, as when the backend restarts
        let (client, eventloop) = AsyncClient::new(MqttOptions::new("old", "127.0.0.1", 1), 10);
        drop(eventloop);
        let (reply, rx) = oneshot();
        let publish = Publish {
            msg: MqttReply {
                topic: "neolink/cam/status".to_string(),
                message: Arc::new("connected".to_string()),
            },
            retain: true,
            reply,
        };
        send_publish(publish, &client, &outgoing_tx, &retained)
            .await
            .unwrap();
        if let Ok(MqttRequest::Publish(p)) = outgoing_rx.try_recv() {
            assert!(p.retain, "a retained publish was requeued without retain");
        }
        tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .expect("no reply")
            .unwrap()
            .unwrap();

        let (client, mut eventloop) = AsyncClient::new(MqttOptions::new("new", "127.0.0.1", 1), 10);
        republish_retained(&client, &retained).await.unwrap();
        // Moves what the client queued to `pending` (there is no broker)
        eventloop.clean();
        let published: Vec<_> = eventloop
            .pending
            .iter()
            .filter_map(|r| match r {
                rumqttc::Request::Publish(p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].topic, "neolink/cam/status");
        assert_eq!(&published[0].payload[..], b"connected");
        assert!(published[0].retain, "republished without retain");
        assert_eq!(published[0].qos, QoS::AtLeastOnce);
    }
}

impl Drop for MqttBackend<'_> {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub(crate) struct MqttInstance {
    outgoing_tx: MpscSender<MqttRequest>,
    incomming_rx: BroadcastStream<MqttReply>,
    name: String,
}

impl MqttInstance {
    /// An instance for `name` without a broker. Messages sent on the returned sender arrive
    /// as if from the broker (topic without the `neolink/` prefix); what neolink publishes
    /// arrives on the receiver as (topic, message, retained). Last wills are not supported
    #[cfg(test)]
    pub(crate) fn for_test(
        name: &str,
    ) -> (
        Self,
        BroadcastSender<MqttReply>,
        MpscReceiver<(String, String, bool)>,
    ) {
        let (outgoing_tx, mut outgoing_rx) = mpsc::<MqttRequest>(100);
        let (published_tx, published_rx) = mpsc(10000);
        let (incoming_tx, _) = broadcast::<MqttReply>(1000);
        let thread_outgoing_tx = outgoing_tx.clone();
        let thread_incoming_tx = incoming_tx.clone();
        tokio::spawn(async move {
            while let Some(req) = outgoing_rx.recv().await {
                let (msg, tx, retained) = match req {
                    MqttRequest::Publish(Publish { msg, retain, reply }) => (msg, reply, retain),
                    MqttRequest::Subscribe(name, reply) => {
                        let _ = reply.send(Ok(MqttInstance {
                            name,
                            incomming_rx: BroadcastStream::new(thread_incoming_tx.subscribe()),
                            outgoing_tx: thread_outgoing_tx.clone(),
                        }));
                        continue;
                    }
                    MqttRequest::LastWill { reply, .. } => {
                        let _ = reply.send(Ok(LastWillMqtt {
                            cancel: CancellationToken::new(),
                        }));
                        continue;
                    }
                    MqttRequest::HangUp(reply) => {
                        let _ = reply.send(());
                        continue;
                    }
                };
                let _ = published_tx
                    .send((msg.topic.clone(), (*msg.message).clone(), retained))
                    .await;
                let _ = tx.send(Ok(()));
            }
        });
        (
            Self {
                incomming_rx: BroadcastStream::new(incoming_tx.subscribe()),
                outgoing_tx,
                name: name.to_string(),
            },
            incoming_tx,
            published_rx,
        )
    }

    pub(crate) fn get_name(&self) -> &str {
        &self.name
    }

    pub async fn subscribe<T: Into<String>>(&self, name: T) -> AnyResult<Self> {
        let (tx, rx) = oneshot();
        self.outgoing_tx
            .send(MqttRequest::Subscribe(name.into(), tx))
            .await?;
        rx.await?
    }

    pub async fn resubscribe(&self) -> AnyResult<Self> {
        let (tx, rx) = oneshot();
        self.outgoing_tx
            .send(MqttRequest::Subscribe(self.name.clone(), tx))
            .await?;
        rx.await?
    }

    pub async fn send_message_with_root_topic(
        &self,
        root_topic: &str,
        sub_topic: &str,
        message: &str,
        retain: bool,
    ) -> AnyResult<()> {
        let topics = [
            root_topic.to_string(),
            self.name.clone(),
            sub_topic.to_string(),
        ]
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>();
        let (tx, rx) = oneshot();
        self.outgoing_tx
            .send(MqttRequest::Publish(Publish {
                msg: MqttReply {
                    topic: topics.join("/"),
                    message: Arc::new(message.to_string()),
                },
                retain,
                reply: tx,
            }))
            .await?;
        rx.await??;
        Ok(())
    }

    pub async fn send_message(
        &self,
        sub_topic: &str,
        message: &str,
        retain: bool,
    ) -> AnyResult<()> {
        self.send_message_with_root_topic("neolink", sub_topic, message, retain)
            .await?;
        Ok(())
    }

    pub(crate) async fn recv(&mut self) -> AnyResult<MqttReply> {
        Ok(loop {
            let mut msg = self
                .incomming_rx
                .next()
                .await
                .ok_or(anyhow!("End of client data"))?
                .with_context(|| "Client stream is too far behind")?;
            // log::debug!("Got MQTT: {msg:?}");
            // log::debug!("self.name: {:?}", self.name);

            if self.name.is_empty() {
                break msg;
            } else {
                let mut topics = msg.topic.split('/');
                let sub_topic = topics.next();
                // log::debug!("topics: {:?}", msg.topic);
                // log::debug!("sub_topic: {sub_topic:?}");
                if sub_topic
                    .map(|subtopic| *subtopic == self.name)
                    .unwrap_or(false)
                {
                    msg.topic = topics.collect::<Vec<_>>().join("/");
                    // log::debug!("new topics: {:?}", msg.topic);
                    break msg;
                }
            }
        })
    }

    pub(crate) async fn last_will(&self, topic: &str, message: &str) -> AnyResult<LastWillMqtt> {
        let topic = if self.name.is_empty() {
            format!("neolink/{}", topic)
        } else {
            format!("neolink/{}/{}", self.name, topic)
        };

        let (tx, rx) = oneshot();
        self.outgoing_tx
            .send(MqttRequest::LastWill {
                topic,
                message: message.to_string(),
                reply: tx,
            })
            .await?;
        rx.await?
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MqttReply {
    pub(crate) topic: String,
    pub(crate) message: Arc<String>, // Messages can be long so avoid costly clones with an arc
}

impl MqttReply {
    pub(crate) fn as_ref(&self) -> MqttReplyRef<'_> {
        MqttReplyRef {
            topic: &self.topic,
            message: &self.message,
        }
    }
}

pub(crate) struct MqttReplyRef<'a> {
    pub(crate) topic: &'a str,
    pub(crate) message: &'a str,
}

enum MqttRequest {
    Publish(Publish),
    HangUp(OneshotSender<()>),
    Subscribe(String, OneshotSender<Result<MqttInstance>>),
    LastWill {
        topic: String,
        message: String,
        reply: OneshotSender<Result<LastWillMqtt>>,
    },
}

/// A message to publish; the reply comes once a client has queued it
struct Publish {
    msg: MqttReply,
    retain: bool,
    reply: OneshotSender<Result<()>>,
}

pub(crate) struct LastWillMqtt {
    cancel: CancellationToken,
}

impl LastWillMqtt {
    pub(crate) async fn new(
        config: &MqttServerConfig,
        topic: String,
        message: String,
    ) -> AnyResult<Self> {
        log::trace!("Run MQTT Last Will");
        let mut mqttoptions = MqttOptions::new(
            format!("NeolinkLastWill_{}_{}", topic, Uuid::new_v4()),
            &config.broker_addr,
            config.port,
        );
        let max_size = 100 * (1024 * 1024);
        mqttoptions.set_max_packet_size(max_size, max_size);

        // Use TLS if ca path is set
        if let Some(ca_path) = &config.ca {
            if let Ok(ca) = std::fs::read(ca_path) {
                // Use client_auth if they have cert and key
                let client_auth = if let Some((cert_path, key_path)) = &config.client_auth {
                    if let (Ok(cert_buf), Ok(key_buf)) =
                        (std::fs::read(cert_path), std::fs::read(key_path))
                    {
                        Some((cert_buf, key_buf))
                    } else {
                        error!("Failed to set client tls");
                        None
                    }
                } else {
                    None
                };

                let transport = Transport::Tls(TlsConfiguration::Simple {
                    ca,
                    alpn: None,
                    client_auth,
                });
                mqttoptions.set_transport(transport);
            } else {
                error!("Failed to set CA");
            }
        };

        if let Some((username, password)) = &config.credentials {
            mqttoptions.set_credentials(username, password);
        }

        mqttoptions.set_keep_alive(Duration::from_secs(5));

        // On unclean disconnect send this
        mqttoptions.set_last_will(LastWill::new(topic, message, QoS::AtLeastOnce, true));

        let (client, mut connection) = AsyncClient::new(mqttoptions, 100);
        let client = Arc::new(client);
        let cancel = CancellationToken::new();
        let thread_cancel = cancel.clone();

        tokio::task::spawn(async move {
            loop {
                let r = tokio::select! {
                    _ = thread_cancel.cancelled() => AnyResult::Ok(()),
                    v = connection.poll() =>  {
                        v?;
                        AnyResult::Ok(())
                    },
                };
                if r.is_ok() {
                    continue;
                }
                break r;
            }?;
            drop(client);
            AnyResult::Ok(())
        });

        Ok(LastWillMqtt { cancel })
    }
}

impl Drop for LastWillMqtt {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
