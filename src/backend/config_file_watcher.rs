use glob::Pattern;
use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::EventKind;
use notify::{RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio::task::{self};
use tracing::{debug, warn};
use walkdir::WalkDir;

use super::{spawn_backend_task, DocumentEvent, WatcherHandle};
use crate::backend::WatcherCommand;
use crate::logging::chain;
use crate::{hash_str, WatcherError};

/// Starts watching the directory for changes in a background task.
///
/// # Returns
/// A tuple containing:
/// * A `ConfigFileWatcherHandle` for the background watcher task.
/// * A `Receiver` for consuming events.
pub fn run_config_file_watcher<P: AsRef<Path>>(
    watch_path: P,
    file_pattern: impl Into<String>,
) -> Result<(WatcherHandle, tokio::sync::mpsc::Receiver<DocumentEvent>), WatcherError> {
    let (event_sender, event_receiver) = mpsc::channel(100);
    let (command_sender, mut command_receiver) = mpsc::channel(1);

    let watch_path = watch_path.as_ref().to_path_buf();
    let file_pattern = file_pattern.into();
    let log_path = watch_path.clone();

    let task = async move {
        // Wait for a start command before we begin
        match command_receiver.recv().await {
            Some(WatcherCommand::Stop) | None => {
                // Exit early if Stop command is received or channel is closed
                debug!(path = %watch_path.display(), "watcher stopped before start");
                return Ok(());
            }
            _ => {}
        }

        // Compute initial file hashes
        let mut file_hashes =
            initial_file_search(&watch_path, &file_pattern, &event_sender).await?;

        let (wh, mut rx) = AsyncWatcherHandler::new();
        let mut watcher = notify::recommended_watcher(wh)?;

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
                        debug!(path = %watch_path.display(), "watcher stop received");
                        break;
                    }
                }
            }
        }

        debug!(path = %watch_path.display(), "config file watcher stopped");

        Ok(())
    };
    let handle = spawn_backend_task(task, move |err| {
        warn!(path = %log_path.display(), error = %chain(err), "config file watcher failed");
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
///
/// A file that cannot be read is logged and skipped; it is loaded once a later change makes it
/// readable.
async fn initial_file_search<P: AsRef<Path>>(
    watch_path: P,
    file_pattern: &str,
    sender: &mpsc::Sender<DocumentEvent>,
) -> Result<HashMap<PathBuf, u64>, WatcherError> {
    let files = find_matching_files(watch_path, file_pattern).await?;

    let mut file_hashes = HashMap::new();
    for file in files {
        let content = match read_file(&file).await {
            Ok(content) => content,
            Err(err) => {
                warn!(path = %file.display(), error = %chain(&err), "config file skipped, cannot read it");
                continue;
            }
        };
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
///
/// A file that cannot be read is logged and skipped so the watcher keeps running; only a closed
/// event channel ends it.
async fn handle_fs_event(
    event: notify::Event,
    file_hashes: &mut HashMap<PathBuf, u64>,
    event_sender: &tokio::sync::mpsc::Sender<DocumentEvent>,
    watch_path: &PathBuf,
    gp: &Pattern,
) -> Result<(), WatcherError> {
    if match_path(watch_path, gp, &event) {
        match event.kind {
            EventKind::Create(CreateKind::File)
            | EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Access(AccessKind::Close(AccessMode::Write))
            | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                if let Some(path) = event.paths.first() {
                    upsert_document(path, file_hashes, event_sender).await?;
                }
            }
            EventKind::Remove(RemoveKind::File)
            | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                if let Some(path) = event.paths.first() {
                    remove_document(path, file_hashes, event_sender).await?;
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
                if let [from, to, ..] = &event.paths[..] {
                    remove_document(from, file_hashes, event_sender).await?;
                    if path_matches(watch_path, gp, to) {
                        upsert_document(to, file_hashes, event_sender).await?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Reads `path` and emits `NewDocument` or, if its content changed, `ContentChanged`.
async fn upsert_document(
    path: &Path,
    file_hashes: &mut HashMap<PathBuf, u64>,
    event_sender: &mpsc::Sender<DocumentEvent>,
) -> Result<(), WatcherError> {
    let content = match read_file(path).await {
        Ok(content) => content,
        Err(err) => {
            warn!(path = %path.display(), error = %chain(&err), "config file skipped, cannot read it");
            return Ok(());
        }
    };
    let new_hash = hash_str(&content);
    let id = path.to_string_lossy().into_owned();
    match file_hashes.insert(path.to_path_buf(), new_hash) {
        Some(old_hash) if old_hash == new_hash => Ok(()),
        Some(_) => send_event(event_sender, DocumentEvent::ContentChanged(id, content)).await,
        None => send_event(event_sender, DocumentEvent::NewDocument(id, content)).await,
    }
}

async fn remove_document(
    path: &Path,
    file_hashes: &mut HashMap<PathBuf, u64>,
    event_sender: &mpsc::Sender<DocumentEvent>,
) -> Result<(), WatcherError> {
    if file_hashes.remove(path).is_some() {
        send_event(
            event_sender,
            DocumentEvent::DocumentRemoved(path.to_string_lossy().into_owned()),
        )
        .await?;
    }
    Ok(())
}

async fn send_event(
    event_sender: &mpsc::Sender<DocumentEvent>,
    event: DocumentEvent,
) -> Result<(), WatcherError> {
    event_sender
        .send(event)
        .await
        .map_err(|_| WatcherError::Notify(notify::Error::generic("Failed to send event")))
}

pub struct AsyncWatcherHandler {
    tx: mpsc::Sender<notify::Event>,
    runtime: Runtime,
}

impl AsyncWatcherHandler {
    pub fn new() -> (Self, mpsc::Receiver<notify::Event>) {
        let (tx, rx) = mpsc::channel(100);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        (Self { tx, runtime }, rx)
    }
}

impl notify::EventHandler for AsyncWatcherHandler {
    fn handle_event(&mut self, event: notify::Result<notify::Event>) {
        match event {
            Ok(event) => self.runtime.block_on(async {
                if !self.tx.is_closed() {
                    match self.tx.send(event).await {
                        Ok(_) => {}
                        Err(err) => {
                            debug!(
                                paths = ?err.0.paths,
                                "file watcher event dropped, channel closed"
                            );
                        }
                    }
                } else {
                    debug!(paths = ?event.paths, "file watcher event dropped, channel closed");
                }
            }),
            Err(err) => {
                warn!(error = %err, "file watch error");
            }
        }
    }
}

/// Matches a path against the file pattern.
///
/// # Arguments
/// * `watch_path` - The base path to watch.
/// * `gp` - The glob pattern for filtering.
/// * `event` - The file system event to match.
fn match_path<P: AsRef<Path>>(watch_path: P, gp: &Pattern, event: &notify::Event) -> bool {
    event
        .paths
        .iter()
        .any(|path| path_matches(&watch_path, gp, path))
}

fn path_matches<P: AsRef<Path>>(watch_path: P, gp: &Pattern, path: &Path) -> bool {
    if let Ok(removed_base) = path.strip_prefix(&watch_path) {
        gp.matches(removed_base.to_str().unwrap_or_default())
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{EventKind, ModifyKind, RenameMode};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "config_watcher_fs_{}_{}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn drain(rx: &mut mpsc::Receiver<DocumentEvent>) -> Vec<DocumentEvent> {
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        events
    }

    #[tokio::test]
    async fn rename_both_reads_the_new_path() {
        let dir = temp_dir();
        let from = dir.join("a.yaml");
        let to = dir.join("b.yaml");
        std::fs::write(&to, "content").unwrap();
        let mut hashes = HashMap::from([(from.clone(), hash_str("content"))]);
        let (tx, mut rx) = mpsc::channel(10);
        let event = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(from.clone())
            .add_path(to.clone());

        handle_fs_event(
            event,
            &mut hashes,
            &tx,
            &dir,
            &Pattern::new("*.yaml").unwrap(),
        )
        .await
        .unwrap();

        let events = drain(&mut rx);
        assert!(
            matches!(&events[..], [DocumentEvent::DocumentRemoved(r), DocumentEvent::NewDocument(n, c)]
                if r == &from.to_string_lossy() && n == &to.to_string_lossy() && c == "content"),
            "{events:?}"
        );
        assert!(hashes.contains_key(&to) && !hashes.contains_key(&from));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn rename_both_from_untracked_path_emits_new_document() {
        // atomic save: editor writes a temp file and renames it over the watched one
        let dir = temp_dir();
        let from = dir.join("a.yaml.tmp");
        let to = dir.join("a.yaml");
        std::fs::write(&to, "new").unwrap();
        let mut hashes = HashMap::from([(to.clone(), hash_str("old"))]);
        let (tx, mut rx) = mpsc::channel(10);
        let event = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(from)
            .add_path(to.clone());

        handle_fs_event(
            event,
            &mut hashes,
            &tx,
            &dir,
            &Pattern::new("*.yaml").unwrap(),
        )
        .await
        .unwrap();

        let events = drain(&mut rx);
        assert!(
            matches!(&events[..], [DocumentEvent::ContentChanged(n, c)] if n == &to.to_string_lossy() && c == "new"),
            "{events:?}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn read_error_does_not_fail_event_handling() {
        let dir = temp_dir();
        let missing = dir.join("gone.yaml");
        let mut hashes = HashMap::new();
        let (tx, mut rx) = mpsc::channel(10);
        let event = notify::Event::new(EventKind::Create(CreateKind::File)).add_path(missing);

        handle_fs_event(
            event,
            &mut hashes,
            &tx,
            &dir,
            &Pattern::new("*.yaml").unwrap(),
        )
        .await
        .unwrap();

        assert!(drain(&mut rx).is_empty());
        assert!(hashes.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    async fn recv(rx: &mut mpsc::Receiver<DocumentEvent>) -> DocumentEvent {
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for document event")
            .expect("watcher channel closed")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn watcher_keeps_running_after_rename() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.yaml"), "a").unwrap();
        std::fs::write(dir.join("other.yaml"), "o").unwrap();
        let (mut handle, mut rx) = run_config_file_watcher(&dir, "*.yaml").unwrap();
        handle.start().await.unwrap();
        // two initial documents
        recv(&mut rx).await;
        recv(&mut rx).await;
        // let the notify watcher register before changing files
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        std::fs::rename(dir.join("a.yaml"), dir.join("b.yaml")).unwrap();
        let mut renamed_seen = false;
        let mut other_seen = false;
        let mut modified = false;
        while !(renamed_seen && other_seen) {
            match recv(&mut rx).await {
                DocumentEvent::NewDocument(id, _) if id.ends_with("b.yaml") => renamed_seen = true,
                DocumentEvent::ContentChanged(id, c)
                    if id.ends_with("other.yaml") && c == "changed" =>
                {
                    other_seen = true
                }
                _ => {}
            }
            if renamed_seen && !modified {
                std::fs::write(dir.join("other.yaml"), "changed").unwrap();
                modified = true;
            }
        }

        handle.stop().await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unreadable_file_at_startup_is_skipped() {
        let dir = temp_dir();
        // invalid UTF-8 cannot be read into a String
        std::fs::write(dir.join("bad.yaml"), [0xff, 0xfe, 0xfd]).unwrap();
        std::fs::write(dir.join("good.yaml"), "g").unwrap();
        let (mut handle, mut rx) = run_config_file_watcher(&dir, "*.yaml").unwrap();
        handle.start().await.unwrap();

        let first = recv(&mut rx).await;
        assert!(
            matches!(&first, DocumentEvent::NewDocument(id, c) if id.ends_with("good.yaml") && c == "g"),
            "{first:?}"
        );
        // let the notify watcher register before changing files
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        std::fs::write(dir.join("good.yaml"), "changed").unwrap();
        std::fs::write(dir.join("bad.yaml"), "fixed").unwrap();
        let mut good_changed = false;
        let mut bad_loaded = false;
        while !(good_changed && bad_loaded) {
            match recv(&mut rx).await {
                DocumentEvent::ContentChanged(id, c)
                    if id.ends_with("good.yaml") && c == "changed" =>
                {
                    good_changed = true
                }
                DocumentEvent::NewDocument(id, c) if id.ends_with("bad.yaml") && c == "fixed" => {
                    bad_loaded = true
                }
                _ => {}
            }
        }

        handle.stop().await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
