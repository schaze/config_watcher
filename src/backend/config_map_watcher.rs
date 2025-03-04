use futures::{StreamExt, TryStreamExt};
use k8s_openapi::api::core::v1::ConfigMap;
use kube::{api::Api, runtime::watcher, Client};
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    time::Duration,
};
use tokio::sync::mpsc;

use super::{DocumentEvent, WatcherCommand, WatcherHandle};
use crate::{hash_str, WatcherError};

/// Starts watching a ConfigMap in the given namespace.
///
/// # Returns
/// - A `ConfigMapWatcherHandle` for controlling the watcher.
/// - A `Receiver` that streams file-like events.
pub fn run_configmap_watcher(
    configmap_name: String,
    namespace: String,
) -> Result<(WatcherHandle, mpsc::Receiver<DocumentEvent>), WatcherError> {
    let (event_sender, event_receiver) = mpsc::channel(100);
    let (command_sender, mut command_receiver) = mpsc::channel(1);

    let handle = tokio::spawn(async move {
        // Wait for a start command before we begin
        match command_receiver.recv().await {
            Some(WatcherCommand::Stop) | None => {
                // Exit early if Stop command is received or channel is closed
                log::info!("Watcher received stop command before starting or channel closed");
                return Ok(());
            }
            _ => {}
        }
        let Ok(client) = Client::try_default().await else {
            log::error!("Cannot create kubernetes client. Configmap watcher will exit!");
            return Ok(());
        };
        let api: Api<ConfigMap> = Api::namespaced(client, &namespace);
        let config =
            watcher::Config::default().fields(format!("metadata.name={}", configmap_name).as_str());
        let mut file_hashes: HashMap<String, u64> = HashMap::new();

        let mut stream = watcher(api, config).boxed();
        loop {
            tokio::select! {
               event = stream.try_next() =>
                    {
                        match event {
                            Ok(Some(watcher::Event::Apply(cm))) | Ok(Some(watcher::Event::InitApply(cm))) => {
                                if cm.metadata.name.as_deref() == Some(&configmap_name) {
                                    handle_configmap_update(
                                        combine_configmap_data(&cm),
                                        &mut file_hashes,
                                        &event_sender,
                                    )
                                    .await;
                                }
                            }
                            Ok(Some(watcher::Event::Delete(cm))) => {
                                if cm.metadata.name.as_deref() == Some(&configmap_name) {
                                    for key in file_hashes.keys() {
                                        event_sender
                                            .send(DocumentEvent::DocumentRemoved(key.clone()))
                                            .await
                                            .ok();
                                    }
                                    file_hashes.clear();
                                }
                            }
                            Ok(None) => {
                                log::warn!("==> Kubernetes ConfigMap Watcher stream has ended. There will not be any more config updates.");
                                break;
                            }
                            Err(err) => {
                                log::error!("==> Error in Kubernetes ConfigMap Watcher: {}", err);
                                // wait for 3 seconds before retrying
                                tokio::time::sleep(Duration::from_secs(3)).await;
                            }
                            _ => {}
                        }
                    },
                // Check for control commands
                Some(command) = command_receiver.recv() => {
                    if let WatcherCommand::Stop = command {
                        log::info!("Watcher received stop command");
                        break;
                    }
                }
            }
        }
        Ok(())
    });

    Ok((
        WatcherHandle {
            command_sender,
            handle: Some(handle),
        },
        event_receiver,
    ))
}

fn combine_configmap_data(cm: &ConfigMap) -> BTreeMap<String, Cow<str>> {
    let mut result = BTreeMap::new();

    if let Some(data) = &cm.data {
        for (key, value) in data {
            result.insert(key.clone(), Cow::Borrowed(value.as_str()));
        }
    }

    if let Some(binary_data) = &cm.binary_data {
        for (key, value) in binary_data {
            match std::str::from_utf8(&value.0) {
                Ok(as_str) => {
                    result.insert(key.clone(), Cow::Borrowed(as_str));
                }
                Err(e) => {
                    log::error!("Cannot utf8 decode value to string: {:?}", e);
                }
            }
        }
    }

    result
}

/// Handles updates to the ConfigMap, detecting per-field changes.
async fn handle_configmap_update(
    new_data: BTreeMap<String, Cow<'_, str>>,
    file_hashes: &mut HashMap<String, u64>,
    event_sender: &mpsc::Sender<DocumentEvent>,
) {
    let mut new_hashes: HashMap<String, u64> = HashMap::new();

    // Detect new files and content changes
    for (key, value) in &new_data {
        let new_hash = hash_str(value);
        new_hashes.insert(key.clone(), new_hash);

        match file_hashes.get(key) {
            Some(&existing_hash) if existing_hash != new_hash => {
                event_sender
                    .send(DocumentEvent::ContentChanged(
                        key.clone(),
                        value.to_string(),
                    ))
                    .await
                    .ok();
            }
            None => {
                event_sender
                    .send(DocumentEvent::NewDocument(key.clone(), value.to_string()))
                    .await
                    .ok();
            }
            _ => {}
        }
    }

    // Detect removed files
    for key in file_hashes.keys() {
        if !new_data.contains_key(key) {
            event_sender
                .send(DocumentEvent::DocumentRemoved(key.clone()))
                .await
                .ok();
        }
    }

    *file_hashes = new_hashes; // Update stored hashes
}
