use glob::Pattern;
use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::RecursiveMode;
use notify::{EventKind, INotifyWatcher};
use notify_debouncer_full::new_debouncer;
use notify_debouncer_full::{self, RecommendedCache};
use notify_debouncer_full::{DebouncedEvent, Debouncer};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::sync::mpsc;
use tokio::sync::mpsc::Receiver;
use tokio::task::{self};
use walkdir::WalkDir;

use super::{DocumentEvent, WatcherHandle};
use crate::backend::WatcherCommand;
use crate::{hash_str, WatcherError};

pub type AsyncWatcherResult = notify::Result<(
    Debouncer<INotifyWatcher, RecommendedCache>,
    Receiver<Result<Vec<notify_debouncer_full::DebouncedEvent>, Vec<notify::Error>>>,
)>;

/// Starts watching the directory for changes in a background task.
///
/// # Returns
/// A tuple containing:
/// * A `ConfigFileWatcherHandle` for the background watcher task.
/// * A `Receiver` for consuming events.
pub fn run_config_file_watcher<P: AsRef<Path>>(
    watch_path: P,
    file_pattern: impl Into<String>,
    debounce: Duration,
) -> Result<(WatcherHandle, tokio::sync::mpsc::Receiver<DocumentEvent>), WatcherError> {
    let (event_sender, event_receiver) = mpsc::channel(100);
    let (command_sender, mut command_receiver) = mpsc::channel(1);

    let watch_path = watch_path.as_ref().to_path_buf();
    let file_pattern = file_pattern.into();

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

        // Compute initial file hashes
        let mut file_hashes =
            initial_file_search(&watch_path, &file_pattern, &event_sender).await?;

        let (mut watcher, mut rx) = create_async_watcher(debounce)?;
        watcher.watch(&watch_path, RecursiveMode::Recursive)?;
        let gp = Pattern::new(&file_pattern)?;

        loop {
            tokio::select! {
                // Process file system events
                Some(res) = rx.recv() => {
                    handle_fs_event(res, &mut file_hashes, &event_sender, &watch_path, &gp).await?;
                }

                // Check for control commands
                Some(command) = command_receiver.recv() => {
                    if let WatcherCommand::Stop = command {
                        log::info!("Watcher received stop command");
                        break;
                    }
                }
            }
        }

        log::debug!("Exiting ConfigFileWatcher loop");

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

/// Recursively walks the specified path and collects files matching the specified pattern.
///
/// # Arguments
/// * `watch_path` - The path to search for files.
/// * `file_pattern` - The glob pattern for matching files.
///
/// # Returns
/// A list of paths matching the given criteria.
async fn find_matching_files<P: AsRef<Path>>(
    watch_path: P,
    file_pattern: &str,
) -> Result<Vec<PathBuf>, WatcherError> {
    let watch_path = watch_path.as_ref().to_path_buf();
    let gp = Pattern::new(file_pattern)?;

    task::spawn_blocking(move || {
        let mut matching_files = Vec::new();
        for entry in WalkDir::new(&watch_path).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_file() {
                if let Ok(Some(file_name)) = path.strip_prefix(&watch_path).map(|f| f.to_str()) {
                    if gp.matches(file_name) {
                        matching_files.push(path.to_path_buf());
                    }
                }
            }
        }
        Ok(matching_files)
    })
    .await
    .unwrap_or(Ok(vec![]))
}

/// Computes a hash for each file matching the given pattern in the specified path.
///
/// # Arguments
/// * `watch_path` - The path to search for files.
/// * `file_pattern` - The glob pattern for matching files.
/// * `sender` - Sender channel to notify about found files
///
/// # Returns
/// A `HashMap` where the keys are file paths and the values are their respective hashes.
async fn initial_file_search<P: AsRef<Path>>(
    watch_path: P,
    file_pattern: &str,
    sender: &mpsc::Sender<DocumentEvent>,
) -> Result<HashMap<PathBuf, u64>, WatcherError> {
    let files = find_matching_files(watch_path, file_pattern).await?;

    let mut file_hashes = HashMap::new();
    for file in files {
        let content = read_file(&file).await?;
        file_hashes.insert(file.clone(), hash_str(&content));
        sender
            .send(DocumentEvent::NewDocument(
                file.to_string_lossy().into_owned(),
                content,
            ))
            .await
            .map_err(|_| WatcherError::Notify(notify::Error::generic("Failed to send event")))?;
    }

    Ok(file_hashes)
}

async fn read_file(path: &Path) -> Result<String, WatcherError> {
    let file = File::open(path)
        .await
        .map_err(|e| WatcherError::FileReadError(path.to_path_buf(), e))?;

    let mut reader = BufReader::new(file);

    let mut content = String::new();

    reader
        .read_to_string(&mut content)
        .await
        .map_err(|e| WatcherError::FileReadError(path.to_path_buf(), e))?;

    Ok(content)
}

/// Processes file system events.
async fn handle_fs_event(
    res: Result<Vec<DebouncedEvent>, Vec<notify::Error>>,
    file_hashes: &mut HashMap<PathBuf, u64>,
    event_sender: &tokio::sync::mpsc::Sender<DocumentEvent>,
    watch_path: &PathBuf,
    gp: &Pattern,
) -> Result<(), WatcherError> {
    match res {
        Ok(events) => {
            for event in events {
                if match_path(watch_path, gp, &event) {
                    match event.kind {
                        EventKind::Create(CreateKind::File)
                        | EventKind::Modify(ModifyKind::Data(_))
                        | EventKind::Access(AccessKind::Close(AccessMode::Write)) => {
                            if let Some(path) = event.paths.first() {
                                let content = read_file(path).await?;
                                // Compute the new hash for the file
                                let new_hash = hash_str(&content);

                                if let Some(existing_hash) = file_hashes.get(path) {
                                    // File exists: Check if the hash has changed
                                    if existing_hash != &new_hash {
                                        // Content changed: Update the hash and emit `ContentChanged`
                                        file_hashes.insert(path.to_path_buf(), new_hash);
                                        event_sender
                                            .send(DocumentEvent::ContentChanged(
                                                path.to_string_lossy().into_owned(),
                                                content,
                                            ))
                                            .await
                                            .unwrap();
                                    }
                                } else {
                                    // File does not exist in `file_hashes`: It's a new file
                                    file_hashes.insert(path.to_path_buf(), new_hash);
                                    event_sender
                                        .send(DocumentEvent::NewDocument(
                                            path.to_string_lossy().into_owned(),
                                            content,
                                        ))
                                        .await
                                        .unwrap();
                                }
                            }
                        }
                        EventKind::Remove(RemoveKind::File) => {
                            if let Some(path) = event.paths.first() {
                                if file_hashes.remove(path).is_some() {
                                    event_sender
                                        .send(DocumentEvent::DocumentRemoved(
                                            path.to_string_lossy().into_owned(),
                                        ))
                                        .await
                                        .unwrap();
                                }
                            }
                        }
                        EventKind::Modify(ModifyKind::Name(mode)) => {
                            match mode {
                                RenameMode::To => {
                                    if let Some(path) = event.paths.first() {
                                        let content = read_file(path).await?;
                                        // Compute the new hash for the file
                                        let new_hash = hash_str(&content);

                                        if let Some(existing_hash) = file_hashes.get(path) {
                                            // File exists: Check if the hash has changed
                                            if existing_hash != &new_hash {
                                                // Content changed: Update the hash and emit `ContentChanged`
                                                file_hashes.insert(path.to_path_buf(), new_hash);
                                                event_sender
                                                    .send(DocumentEvent::ContentChanged(
                                                        path.to_string_lossy().into_owned(),
                                                        content,
                                                    ))
                                                    .await
                                                    .unwrap();
                                            }
                                        } else {
                                            // File does not exist in `file_hashes`: It's a new file
                                            file_hashes.insert(path.to_path_buf(), new_hash);
                                            event_sender
                                                .send(DocumentEvent::NewDocument(
                                                    path.to_string_lossy().into_owned(),
                                                    content,
                                                ))
                                                .await
                                                .unwrap();
                                        }
                                    }
                                }
                                RenameMode::From => {
                                    if let Some(path) = event.paths.first() {
                                        if file_hashes.remove(path).is_some() {
                                            event_sender
                                                .send(DocumentEvent::DocumentRemoved(
                                                    path.to_string_lossy().into_owned(),
                                                ))
                                                .await
                                                .unwrap();
                                        }
                                    }
                                }
                                RenameMode::Both => {
                                    if let [from, to, ..] = &event.paths[..] {
                                        // Remove the hash for the `from` file
                                        if file_hashes.remove(from).is_some() {
                                            event_sender
                                                .send(DocumentEvent::DocumentRemoved(
                                                    from.to_string_lossy().into_owned(),
                                                ))
                                                .await
                                                .unwrap();

                                            // Compute the hash for the `to` file to check for changes
                                            let content = read_file(from).await?;

                                            // Compute the new hash for the file
                                            let new_hash = hash_str(&content);

                                            file_hashes.insert(to.to_path_buf(), new_hash);
                                            event_sender
                                                .send(DocumentEvent::NewDocument(
                                                    to.to_string_lossy().into_owned(),
                                                    content,
                                                ))
                                                .await
                                                .unwrap();
                                        }
                                    }
                                }
                                _ => {
                                    // log::debug!("Unhandled Rename Event: {:?}", event);
                                }
                            }
                        }
                        _ => {
                            // log::debug!("Unhandled Event: {:?}", event);
                        }
                    }
                }
            }
            // log::debug!("Files: {:#?}", file_hashes);
            // log::debug!("");
        }
        Err(e) => log::error!("Watch error: {:?}", e),
    }
    Ok(())
}

/// Creates an async file watcher.
///
/// This function sets up a debouncer for watching file system changes.
fn create_async_watcher(debounce: Duration) -> AsyncWatcherResult {
    let (tx, rx) = mpsc::channel(100);
    let runtime = tokio::runtime::Runtime::new().unwrap();

    let watcher = new_debouncer(debounce, None, move |res| {
        runtime.block_on(async {
            if !tx.is_closed() {
                match tx.send(res).await {
                    Ok(_) => {}
                    Err(err) => {
                        log::error!("Error sending file watcher event: {}", err);
                    }
                }
            }
        })
    })?;

    Ok((watcher, rx))
}

/// Matches a path against the file pattern.
///
/// # Arguments
/// * `watch_path` - The base path to watch.
/// * `gp` - The glob pattern for filtering.
/// * `event` - The file system event to match.
fn match_path<P: AsRef<Path>>(watch_path: P, gp: &Pattern, event: &DebouncedEvent) -> bool {
    event.paths.iter().any(|path| {
        if let Ok(removed_base) = path.strip_prefix(&watch_path) {
            gp.matches(removed_base.to_str().unwrap_or_default())
        } else {
            false
        }
    })
}
