use super::{spawn_backend_task, DocumentEvent, WatcherHandle};
use crate::{backend::WatcherCommand, hash_str, logging::chain, WatcherError};
use rumqttc::{AsyncClient, ConnectionError, QoS};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tracing::{debug, warn};

#[derive(Clone, Debug)]
pub struct MqttPublishEvent {
    pub topic: String,
    pub payload: String,
    pub duplicate: bool,
    pub retain: bool,
    pub qos: QoS,
}

#[derive(Debug)]
pub enum MqttClientEvent {
    Connect,
    Disconnect,
    Stop,
    PublishMessage(MqttPublishEvent),
    Error(ConnectionError),
}

pub fn run_mqtt_watcher(
    mqttoptions: rumqttc::MqttOptions,
    config_topic: &str,
    channel_size: usize,
) -> Result<(WatcherHandle, mpsc::Receiver<DocumentEvent>), WatcherError> {
    let (event_sender, receiver) = mpsc::channel(channel_size);

    let (host, port) = mqttoptions.broker_address();
    let broker = format!("{host}:{port}");
    let (mqtt_client, mut eventloop) = AsyncClient::new(mqttoptions, channel_size);
    let (command_sender, mut command_receiver) = mpsc::channel(1);

    let config_topic = format!("{}/#", config_topic.trim_end_matches('/'));
    let log_topic = config_topic.clone();

    let task = async move {
        // Wait for a start command before we begin
        match command_receiver.recv().await {
            Some(WatcherCommand::Stop) | None => {
                // Exit early if Stop command is received or channel is closed
                debug!(topic = %config_topic, "watcher stopped before start");
                return Ok(());
            }
            _ => {}
        }
        let mut hashes: HashMap<String, u64> = HashMap::new();
        // Start of the current connection outage and the number of failed attempts in it
        let mut outage: Option<(Instant, u32)> = None;

        loop {
            tokio::select! {
                poll_res = eventloop.poll() => {
                    match poll_res {
                        Ok(event) => match event {
                            rumqttc::Event::Incoming(rumqttc::Packet::Publish(p)) => {
                                let topic = p.topic;
                                if p.payload.is_empty() {
                                    // deleted topic
                                    if hashes.remove(&topic).is_some() {
                                        event_sender
                                            .send(DocumentEvent::DocumentRemoved(topic))
                                            .await
                                            .unwrap();
                                    }
                                } else {
                                    // published new or updated content
                                    let content = match String::from_utf8(p.payload.to_vec()) {
                                        Ok(payload) => payload,
                                        Err(err) => {
                                            warn!(
                                                topic = %topic,
                                                error = %err,
                                                "MQTT config payload is not UTF-8"
                                            );
                                            continue;
                                        }
                                    };

                                    let new_hash = hash_str(&content);
                                    if let Some(existing_hash) = hashes.get(&topic) {
                                        // File exists: Check if the hash has changed
                                        if existing_hash != &new_hash {
                                            // Content changed: Update the hash and emit `ContentChanged`
                                            hashes.insert(topic.clone(), new_hash);
                                            event_sender
                                                .send(DocumentEvent::ContentChanged(topic, content))
                                                .await
                                                .unwrap();
                                        }
                                    } else {
                                        // File does not exist in `file_hashes`: It's a new file
                                        hashes.insert(topic.clone(), new_hash);
                                        event_sender
                                            .send(DocumentEvent::NewDocument(topic, content))
                                            .await
                                            .unwrap();
                                    }
                                }
                            }
                            rumqttc::Event::Incoming(rumqttc::Incoming::ConnAck(_)) => {
                                if let Some((since, attempts)) = outage.take() {
                                    debug!(
                                        topic = %config_topic,
                                        broker = %broker,
                                        outage_secs = since.elapsed().as_secs(),
                                        attempts,
                                        "MQTT connection restored"
                                    );
                                } else {
                                    debug!(
                                        topic = %config_topic,
                                        broker = %broker,
                                        "MQTT connected"
                                    );
                                }
                                // subscribe to config topic
                                mqtt_client
                                    .subscribe(&config_topic, rumqttc::QoS::ExactlyOnce)
                                    .await?;
                            }
                            rumqttc::Event::Outgoing(rumqttc::Outgoing::Disconnect) => {
                                debug!(
                                    topic = %config_topic,
                                    broker = %broker,
                                    "MQTT disconnected"
                                );
                                break;
                            }
                            _ => {}
                        },

                        Err(err) => {
                            if let Some((_, attempts)) = &mut outage {
                                *attempts += 1;
                                debug!(
                                    topic = %config_topic,
                                    broker = %broker,
                                    attempt = *attempts,
                                    error = %err,
                                    "MQTT reconnect failed"
                                );
                            } else {
                                warn!(
                                    topic = %config_topic,
                                    broker = %broker,
                                    error = %err,
                                    "MQTT connection lost"
                                );
                                outage = Some((Instant::now(), 1));
                            }
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    };

                },
                // Check for control commands
                Some(command) = command_receiver.recv() => {
                    if let WatcherCommand::Stop = command {
                        debug!(topic = %config_topic, "watcher stop received");
                        break;
                    }

                }
            };
        }
        debug!(topic = %config_topic, "MQTT config watcher stopped");
        Ok(())
    };
    let handle = spawn_backend_task(task, move |err| {
        warn!(topic = %log_topic, error = %chain(err), "MQTT config watcher failed");
    });
    Ok((
        WatcherHandle {
            handle: Some(handle),
            command_sender,
        },
        receiver,
    ))
}
