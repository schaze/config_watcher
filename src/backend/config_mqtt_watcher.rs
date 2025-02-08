use super::{DocumentEvent, WatcherHandle};
use crate::{hash_str, WatcherError};
use rumqttc::{AsyncClient, ConnectionError, QoS};
use std::{collections::HashMap, time::Duration};
use tokio::sync::{mpsc, watch};

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

    let (mqtt_client, mut eventloop) = AsyncClient::new(mqttoptions, channel_size);
    let (stop_sender, mut stop_receiver) = watch::channel(false);

    let config_topic = format!("{}/#", config_topic.trim_end_matches('/'));

    let handle = tokio::task::spawn(async move {
        let mut hashes: HashMap<String, u64> = HashMap::new();

        loop {
            let poll_res = tokio::select! {
                poll_res = eventloop.poll() => poll_res,
                _exit = stop_receiver.changed() => {
                        log::debug!("Before Borrow");
                    if *stop_receiver.borrow() {
                        log::debug!("Received stop signal. Exiting...");
                        break;
                    }
                    continue;
                }
            };
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
                                    log::warn!(
                                    "Cannot parse mqtt payload for topic [{}] to string. Error: {}",
                                    topic,
                                    err
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
                        log::debug!("HOMIE: Connected");
                        // subscribe to config topic
                        mqtt_client
                            .subscribe(&config_topic, rumqttc::QoS::ExactlyOnce)
                            .await?;
                    }
                    rumqttc::Event::Outgoing(rumqttc::Outgoing::Disconnect) => {
                        log::debug!("HOMIE: Connection closed from our side.",);
                        break;
                    }
                    _ => {}
                },

                Err(err) => {
                    log::error!("Error connecting mqtt. {:#?}", err);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            };
        }
        log::debug!("Exiting mqtt config watcher eventloop...");
        Ok(())
    });
    Ok((
        WatcherHandle {
            handle,
            stop_sender,
        },
        receiver,
    ))
}
