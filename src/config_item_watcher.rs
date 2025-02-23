use crate::backend::{DocumentEvent, WatcherHandle};
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
            log::warn!("Task handle was already taken or not initialized.");
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
    E: Send + Sync + std::fmt::Debug + 'static,
{
    let (watcher_backend_handle, mut receiver) = make_watcher_backend()?;
    let (event_tx, event_rx) = mpsc::channel(100);
    let (stop_sender, mut stop_receiver) = watch::channel(false);

    let mut item_hashes = HashSet::new();

    let handle = tokio::spawn({
        let event_tx = event_tx.clone();

        async move {
            loop {
                // log::warn!("waiting for file events: {}", fp);
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
                                log::warn!("Shutdown sender dropped. Exiting watcher.");
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

            log::debug!("Exiting Watcher loop");
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
    E: Send + Sync + std::fmt::Debug,
{
    match event {
        DocumentEvent::NewDocument(filename, content) => {
            log::debug!("Processing document: {:?}", filename);

            let mut events =
                match process_file(&filename, content, item_hashes, tokenizer, deserialize).await {
                    Ok(events) => events,
                    Err(err) => {
                        log::error!("Failed to process document {:?}: {:?}", filename, err);
                        vec![] // Skip this event
                    }
                };
            events.insert(
                0,
                ConfigItemEvent::NewDocument(hash_str(&filename), filename),
            );
            events
        }
        DocumentEvent::ContentChanged(filename, content) => {
            log::debug!("Processing document: {:?}", filename);
            match process_file(&filename, content, item_hashes, tokenizer, deserialize).await {
                Ok(events) => events,
                Err(err) => {
                    log::error!("Failed to process document {:?}: {:?}", filename, err);
                    vec![] // Skip this event
                }
            }
        }
        // Handle file removal
        DocumentEvent::DocumentRemoved(filename) => {
            log::debug!("Document removed: {:?}", filename);

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
) -> Result<Vec<ConfigItemEvent<T>>, WatcherError>
where
    T: Send + Sync,
    E: Send + Sync + std::fmt::Debug,
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
                log::error!(
                    "Failed to deserialize document in file {:?}:\n{}\n{:?}",
                    filename,
                    doc,
                    err
                );
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

    Ok(events)
}
