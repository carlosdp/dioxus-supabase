use std::{
    collections::HashMap,
    rc::Rc,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use crossbeam::queue::SegQueue;
use dioxus::hooks::use_context_provider;
use dioxus::prelude::*;
use dioxus_query::prelude::futures_util::TryStreamExt;
use futures::{select, FutureExt, SinkExt, StreamExt};
use reqwest_websocket::Message;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PostgresEvent {
    #[serde(rename = "*")]
    Any,
    Insert,
    Update,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresChanges {
    pub event: PostgresEvent,
    pub schema: String,
    pub table: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BroadcastConfig {
    #[serde(rename = "self")]
    pub self_messages: bool,
    pub ack: bool,
}

#[derive(Debug, Clone)]
pub enum SubscriptionSpec {
    PostgresChanges(PostgresChanges),
    BroadcastChannel(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broadcast: Option<BroadcastConfig>,
    pub postgres_changes: Vec<PostgresChanges>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresPayload {
    pub schema: String,
    pub table: String,
    pub commit_timestamp: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub record: serde_json::Value,
    pub old_record: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "payload")]
pub enum Events {
    #[serde(rename = "phx_join")]
    Join {
        config: SubscriptionConfig,
        access_token: Option<String>,
    },
    #[serde(rename = "phx_leave")]
    Leave {},
    #[serde(rename = "heartbeat")]
    Heartbeat {},
    #[serde(rename = "postgres_changes")]
    PostgresChanges {
        data: PostgresPayload,
        ids: Vec<i32>,
    },
    #[serde(rename = "broadcast")]
    Broadcast {
        #[serde(rename = "type")]
        ty: String,
        event: String,
        payload: serde_json::Value,
    },
    #[serde(rename = "access_token")]
    AccessToken { access_token: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMessage {
    pub topic: String,
    #[serde(flatten)]
    pub payload: Events,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub join_ref: Option<String>,
}

// Create a future that polls the SegQueue
async fn poll_queue<T>(queue: Arc<SegQueue<T>>) -> Option<T> {
    match queue.pop() {
        Some(msg) => Some(msg),
        None => {
            // If queue is empty, add a small delay to prevent busy-waiting
            gloo_timers::future::sleep(Duration::from_millis(10)).await;
            None
        }
    }
}

async fn time_heartbeat() {
    gloo_timers::future::sleep(Duration::from_secs(30)).await;
}

pub enum Command {
    Subscribe(RealtimeChannel),
    Unsubscribe(String),
}

#[derive(Debug, Clone)]
pub struct RealtimeChannel {
    pub topic: String,
    pub postgres_changes: Vec<PostgresChanges>,
    command_queue: Arc<SegQueue<Command>>,
    pub payload_queue: Arc<SegQueue<PostgresPayload>>,
    pub broadcast_queue: Arc<SegQueue<serde_json::Value>>,
    pub join_ref: Option<String>,
}

impl RealtimeChannel {
    pub fn unsubscribe(&mut self) {
        self.command_queue
            .push(Command::Unsubscribe(self.topic.clone()));
    }
}

#[derive(Clone)]
pub struct RealtimeClient {
    pub realtime_url: String,
    pub subscriptions: Arc<Mutex<HashMap<String, RealtimeChannel>>>,
    access_token: Option<String>,
    command_queue: Arc<SegQueue<Command>>,
}

impl RealtimeClient {
    pub fn new(
        supabase_url: impl Into<String>,
        supabase_anon_key: impl Into<String>,
        access_token: Option<String>,
    ) -> Self {
        let realtime_url = format!(
            "{}/realtime/v1/websocket?apikey={}&vsn=1.0.0",
            supabase_url.into().replace("http", "ws"),
            supabase_anon_key.into()
        );

        Self {
            realtime_url,
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            access_token,
            command_queue: Arc::new(SegQueue::new()),
        }
    }

    pub fn subscribe(&self, topic: impl Into<String>, spec: SubscriptionSpec) -> RealtimeChannel {
        let topic = topic.into();

        let channel = match spec {
            SubscriptionSpec::PostgresChanges(spec) => RealtimeChannel {
                topic: format!("realtime:{}", topic),
                postgres_changes: vec![spec],
                command_queue: self.command_queue.clone(),
                payload_queue: Arc::new(SegQueue::new()),
                broadcast_queue: Arc::new(SegQueue::new()),
                join_ref: None,
            },
            SubscriptionSpec::BroadcastChannel(topic) => RealtimeChannel {
                topic: format!("realtime:{}", topic),
                postgres_changes: vec![],
                command_queue: self.command_queue.clone(),
                payload_queue: Arc::new(SegQueue::new()),
                broadcast_queue: Arc::new(SegQueue::new()),
                join_ref: None,
            },
        };

        self.command_queue.push(Command::Subscribe(channel.clone()));

        channel
    }

    pub async fn process(&self) {
        let websocket = reqwest_websocket::websocket(&self.realtime_url)
            .await
            .unwrap();
        let (mut sender, mut receiver) = websocket.split();
        let mut refid: i64 = 0;

        let heartbeat_stream = time_heartbeat().fuse();

        futures::pin_mut!(heartbeat_stream);

        loop {
            let message_stream = receiver.try_next().fuse();
            let queue_stream = poll_queue(self.command_queue.clone()).fuse();

            futures::pin_mut!(message_stream, queue_stream);

            select! {
                message = message_stream => {
                    if let Ok(Some(Message::Text(message))) = message {
                        if let Ok(event_message) = serde_json::from_str::<EventMessage>(&message) {
                            match event_message.payload {
                                Events::PostgresChanges { data, .. } => {
                                    tracing::debug!("received payload for topic: {}", &event_message.topic);
                                    if let Some(channel) = self.subscriptions.lock().unwrap().get(&event_message.topic) {
                                        channel.payload_queue.push(data);
                                    } else {
                                        tracing::error!("no channel found for payload in topic: {}", &event_message.topic);
                                    }
                                },
                                Events::Broadcast { event, payload, .. } => {
                                    tracing::debug!("received broadcast event: {}", &event);
                                    tracing::debug!("payload: {}", &payload);
                                    if let Some(channel) = self.subscriptions.lock().unwrap().get(&event_message.topic) {
                                        channel.broadcast_queue.push(payload);
                                    } else {
                                        tracing::error!("no channel found for payload in topic: {}", &event_message.topic);
                                    }
                                },
                                _ => {}
                            }
                        }
                    }
                },
                queue_result = &mut queue_stream => {
                    if let Some(command) = queue_result {
                        match command {
                            Command::Subscribe(mut channel) => {
                                tracing::debug!("subscribing to topic: {}", &channel.topic);

                                if channel.join_ref.is_some() {
                                    tracing::error!("already subscribed to topic: {}", &channel.topic);
                                    continue;
                                }

                                sender.send(Message::Text(serde_json::to_string(&EventMessage {
                                    topic: channel.topic.clone(),
                                    payload: Events::Join {
                                        config: SubscriptionConfig {
                                            private: if channel.postgres_changes.len() < 1 { Some(true) } else { None },
                                            broadcast: if channel.postgres_changes.len() < 1 { Some(BroadcastConfig {
                                                self_messages: false,
                                                ack: false,
                                            }) } else { None },
                                            postgres_changes: channel.postgres_changes.clone()
                                        },
                                        access_token: self.access_token.clone(),
                                    },
                                    reference: Some(refid.to_string()),
                                    join_ref: None,
                                }).unwrap())).await.unwrap();

                                channel.join_ref = Some(refid.to_string());

                                refid += 1;

                                self.subscriptions.lock().unwrap().insert(channel.topic.clone(), channel);
                            },
                            Command::Unsubscribe(topic) => {
                                tracing::debug!("unsubscribing from topic: {}", &topic);

                                if let Some(channel) = self.subscriptions.lock().unwrap().remove(&topic) {
                                    sender.send(Message::Text(serde_json::to_string(&EventMessage {
                                        topic: topic.clone(),
                                        payload: Events::Leave {},
                                        reference: Some(refid.to_string()),
                                        join_ref: Some(channel.join_ref.unwrap()),
                                    }).unwrap())).await.unwrap();
                                }
                            },
                        }
                    }
                },
                _ = &mut heartbeat_stream => {
                    sender.send(Message::Text(serde_json::to_string(&EventMessage {
                        topic: "phoenix".to_string(),
                        payload: Events::Heartbeat {},
                        reference: Some(refid.to_string()),
                        join_ref: None,
                    }).unwrap())).await.unwrap();

                    refid += 1;

                    heartbeat_stream.set(time_heartbeat().fuse());
                }
            }
        }
    }
}

pub fn use_realtime_provider(
    supabase_url: &str,
    supabase_anon_key: &str,
    access_token: Option<String>,
) {
    let client =
        use_context_provider(|| RealtimeClient::new(supabase_url, supabase_anon_key, access_token));

    use_future(move || {
        let client = client.clone();

        async move {
            web! {
                client.process().await;
            }
        }
    });
}

#[derive(Deserialize)]
pub struct PostgresChange<T> {
    pub schema: String,
    pub table: String,
    pub commit_timestamp: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub record: T,
    pub old_record: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<String>,
}

impl<T: DeserializeOwned> From<PostgresPayload> for PostgresChange<T> {
    fn from(value: PostgresPayload) -> Self {
        PostgresChange {
            schema: value.schema,
            table: value.table,
            commit_timestamp: value.commit_timestamp,
            event_type: value.event_type,
            record: serde_json::from_value(value.record).unwrap(),
            old_record: value.old_record,
            errors: value.errors,
        }
    }
}

pub fn use_realtime_changes<T: DeserializeOwned>(
    topic: impl Into<String>,
    spec: SubscriptionSpec,
    handler: impl FnMut(PostgresChange<T>) + 'static,
) {
    let handler = Rc::new(RwLock::new(handler));
    let client = use_context::<RealtimeClient>();
    let channel = use_signal(move || client.subscribe(topic, spec));

    use_future(move || {
        let handler = handler.clone();

        async move {
            web! {
                loop {
                    if let Some(payload) = poll_queue(channel().payload_queue.clone()).await {
                        handler.write().unwrap()(payload.into());
                    }
                }
            }
        }
    });

    use_drop(move || {
        web! {
            channel().unsubscribe();
        }
    });
}

#[derive(Deserialize)]
struct BroadcastChangePayload<T> {
    record: T,
}

pub fn use_realtime_broadcast<T: DeserializeOwned>(
    topic: impl Into<String>,
    spec: SubscriptionSpec,
    handler: impl FnMut(T) + 'static,
) {
    let handler = Rc::new(RwLock::new(handler));
    let client = use_context::<RealtimeClient>();
    let channel = use_signal(move || client.subscribe(topic, spec));

    use_future(move || {
        let handler = handler.clone();

        async move {
            web! {
                loop {
                    if let Some(payload) = poll_queue(channel().broadcast_queue.clone()).await {
                        if let Ok(payload) = serde_json::from_value::<BroadcastChangePayload<T>>(payload) {
                            handler.write().unwrap()(payload.record);
                        }
                    }
                }
            }
        }
    });

    use_drop(move || {
        web! {
            channel().unsubscribe();
        }
    });
}
