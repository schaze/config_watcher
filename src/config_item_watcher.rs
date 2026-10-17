use crate::backend::{DocumentEvent, WatcherHandle};
use crate::logging::{truncate, PAYLOAD_MAX};
use crate::{hash_str, Tokenizer, WatcherError};
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
};
use tokio::{
    sync::{
        mpsc::{self, Receiver},
        watch,
    },
    task::JoinHandle,
};
use tracing::debug;

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub struct ConfigItemHash(u64, u64);

impl ConfigItemHash {
    pub fn filename_hash(&self) -> u64 {
        self.0
    }

    pub fn item_hash(&self) -> u64 {
        self.1
    }
}

impl Display for ConfigItemHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}-{}", self.0, self.1))
    }
}

// Event types for configuration items
#[derive(Debug)]
pub enum ConfigItemEvent<T> {
    NewDocument(u64, String),
    RemoveDocument(u64),
    New(ConfigItemHash, T),  // Hash and Item
    Removed(ConfigItemHash), // Hash of the removed item
    /// An item of the document (ID and name as in `NewDocument`) failed to deserialize and was
    /// skipped; `error` is the deserializer's message.
    Invalid {
        document: u64,
        name: String,
        error: String,
    },
}

pub struct ConfigItemWatcherHandle {
    task_handle: Option<JoinHandle<Result<(), WatcherError>>>,
    watcher_backend_handle: WatcherHandle,
    stop_sender: watch::Sender<bool>, // Shutdown signal
}

impl ConfigItemWatcherHandle {
    /// starts the watcher. Can only be used once!
    pub async fn start(&self) -> Result<(), WatcherError> {
        self.watcher_backend_handle.start().await?;
        Ok(())
    }

    /// Stops the watcher task.
    pub async fn stop(&mut self) -> Result<(), WatcherError> {
        self.watcher_backend_handle.stop().await?;
        let _ = self.stop_sender.send(true); // Send the shutdown signal

        if let Some(handle) = self.task_handle.take() {
            handle.await??;
        } else {
            debug!("watcher task handle already taken");
        }
        Ok(())
    }
}

// Watcher function
pub fn run_config_item_watcher<T, E>(
    make_watcher_backend: impl Fn() -> std::result::Result<
        (WatcherHandle, tokio::sync::mpsc::Receiver<DocumentEvent>),
        WatcherError,
    >,
    tokenizer: &'static dyn Tokenizer,
    deserialize: impl Fn(&str) -> std::result::Result<T, E> + Send + Sync + 'static,
) -> Result<(ConfigItemWatcherHandle, Receiver<ConfigItemEvent<T>>), WatcherError>
where
    T: Send + Sync + 'static,
    E: Send + Sync + Display + 'static,
{
    let (watcher_backend_handle, mut receiver) = make_watcher_backend()?;
    let (event_tx, event_rx) = mpsc::channel(100);
    let (stop_sender, mut stop_receiver) = watch::channel(false);

    let mut item_hashes = HashSet::new();

    let handle = tokio::spawn({
        let event_tx = event_tx.clone();

        async move {
            loop {
                let events = tokio::select! {
                    // Wait for file events
                    Some(event) = receiver.recv() => {
                        handle_config_file_event(event, &mut item_hashes, tokenizer, &deserialize).await
                    }
                    // Check for shutdown signal
                    result = stop_receiver.changed() => {
                        match result {
                            Ok(_) => if *stop_receiver.borrow(){
                                break;
                            } else {
                                continue;
                            },
                            Err(_) => {
                                debug!("watcher stop sender dropped");
                                break;
                            }
                        }
                    }
                };

                // Send events for new or changed items
                for event in events {
                    event_tx.send(event).await.unwrap();
                }
            }

            debug!("config item watcher stopped");
            Ok(())
        }
    });

    Ok((
        ConfigItemWatcherHandle {
            task_handle: Some(handle),
            watcher_backend_handle,
            stop_sender,
        },
        event_rx,
    ))
}

async fn handle_config_file_event<T, E>(
    event: DocumentEvent,
    item_hashes: &mut HashSet<ConfigItemHash>,
    tokenizer: &dyn Tokenizer,
    deserialize: &(impl Fn(&str) -> std::result::Result<T, E> + Send + Sync),
) -> Vec<ConfigItemEvent<T>>
where
    T: Send + Sync,
    E: Send + Sync + Display,
{
    match event {
        DocumentEvent::NewDocument(filename, content) => {
            debug!(document = %filename, "document added");

            let mut events =
                process_file(&filename, content, item_hashes, tokenizer, deserialize).await;
            events.insert(
                0,
                ConfigItemEvent::NewDocument(hash_str(&filename), filename),
            );
            events
        }
        DocumentEvent::ContentChanged(filename, content) => {
            debug!(document = %filename, "document changed");
            process_file(&filename, content, item_hashes, tokenizer, deserialize).await
        }
        // Handle file removal
        DocumentEvent::DocumentRemoved(filename) => {
            debug!(document = %filename, "document removed");

            let mut events = file_removed(&filename, item_hashes);
            events.push(ConfigItemEvent::RemoveDocument(hash_str(&filename)));
            events
        }
    }
}

fn file_removed<T>(
    filename: &str,
    item_hashes: &mut HashSet<ConfigItemHash>,
) -> Vec<ConfigItemEvent<T>>
where
    T: Send + Sync,
{
    let mut events = Vec::new();

    let filepath_hash = hash_str(filename);

    item_hashes.retain(|hash| {
        if hash.0 == filepath_hash {
            events.push(ConfigItemEvent::Removed(*hash));
            false
        } else {
            true
        }
    });
    events
}

async fn process_file<T, E>(
    filename: &str,
    content: String,
    item_hashes: &mut HashSet<ConfigItemHash>,
    tokenizer: &dyn Tokenizer,
    deserialize: &impl Fn(&str) -> std::result::Result<T, E>,
) -> Vec<ConfigItemEvent<T>>
where
    T: Send + Sync,
    E: Send + Sync + Display,
{
    let mut events = Vec::new();

    // Parse the file into new items and their hashes

    let filename_hash = hash_str(filename);
    let new_items: HashMap<u64, T> = tokenizer
        .tokenize(&content)
        .map(|doc| doc.trim())
        .filter(|doc| !doc.is_empty())
        .filter_map(|doc| match deserialize(doc) {
            Ok(item) => Some((hash_str(doc), item)),
            Err(err) => {
                let error = err.to_string();
                debug!(
                    document = %filename,
                    error = %error,
                    content = %truncate(doc, PAYLOAD_MAX),
                    "config item deserialization failed"
                );
                events.push(ConfigItemEvent::Invalid {
                    document: filename_hash,
                    name: filename.to_owned(),
                    error,
                });
                None
            }
        })
        .collect();

    // Filter and detect removals
    item_hashes.retain(|hash| {
        // if the hash does not belong to the current file, we keep it
        if hash.0 != filename_hash {
            return true;
        }
        // if the hash belongs to the current file but its content hash is not found in the
        // new_hashes, the item was removed
        if !new_items.contains_key(&hash.1) {
            events.push(ConfigItemEvent::Removed(*hash));
            false // Remove this item
        } else {
            true // Keep this item
        }
    });

    // Detect changes and additions
    for (new_hash, new_item) in new_items.into_iter() {
        let hash = ConfigItemHash(filename_hash, new_hash);
        if item_hashes.insert(hash) {
            // New item
            events.push(ConfigItemEvent::New(hash, new_item));
        }
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::YamlTokenizer;

    const FILE: &str = "a.yaml";

    fn parse(doc: &str) -> Result<u32, String> {
        doc.parse().map_err(|_| format!("not a number: {doc}"))
    }

    async fn load(
        event: DocumentEvent,
        hashes: &mut HashSet<ConfigItemHash>,
    ) -> Vec<ConfigItemEvent<u32>> {
        handle_config_file_event(event, hashes, &YamlTokenizer, &parse).await
    }

    #[tokio::test]
    async fn invalid_item_is_reported_after_new_document() {
        let mut hashes = HashSet::new();
        let event = DocumentEvent::NewDocument(FILE.into(), "1\n---\nx".into());

        let events = load(event, &mut hashes).await;

        let document = hash_str(FILE);
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], ConfigItemEvent::NewDocument(id, _) if *id == document));
        assert!(matches!(
            &events[1],
            ConfigItemEvent::Invalid { document: id, name, error }
                if *id == document && name == FILE && error == "not a number: x"
        ));
        assert!(matches!(&events[2], ConfigItemEvent::New(_, 1)));
    }

    #[tokio::test]
    async fn item_that_becomes_invalid_is_removed_and_reported() {
        let mut hashes = HashSet::new();
        load(
            DocumentEvent::NewDocument(FILE.into(), "1".into()),
            &mut hashes,
        )
        .await;

        let events = load(
            DocumentEvent::ContentChanged(FILE.into(), "y".into()),
            &mut hashes,
        )
        .await;

        let document = hash_str(FILE);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            ConfigItemEvent::Invalid { document: id, .. } if *id == document
        ));
        assert!(matches!(
            &events[1],
            ConfigItemEvent::Removed(hash) if hash.filename_hash() == document
        ));
        assert!(hashes.is_empty());
    }
}
