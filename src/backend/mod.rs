mod config_file_watcher;
mod config_map_watcher;
mod config_mqtt_watcher;

pub use config_file_watcher::*;
pub use config_map_watcher::*;
pub use config_mqtt_watcher::*;
use std::future::Future;
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::debug;

use crate::WatcherError;

#[derive(Debug)]
pub enum DocumentEvent {
    NewDocument(String, String), // New document (ID, Content) added with content
    ContentChanged(String, String), // Content of an existing document changed (ID, Content)
    DocumentRemoved(String),     // Document removed (ID)
}

pub struct WatcherHandle {
    pub(crate) command_sender: mpsc::Sender<WatcherCommand>, // Shutdown signal
    pub(crate) handle: Option<tokio::task::JoinHandle<Result<(), WatcherError>>>,
}

impl WatcherHandle {
    /// starts the watcher. Can only be used once!
    pub async fn start(&self) -> Result<(), WatcherError> {
        self.command_sender.send(WatcherCommand::Start).await?;
        Ok(())
    }

    /// Stops the watcher task.
    pub async fn stop(&mut self) -> Result<(), WatcherError> {
        self.command_sender.send(WatcherCommand::Stop).await?; // Send the shutdown signal
        if let Some(handle) = self.handle.take() {
            handle.await??;
        } else {
            debug!("watcher task handle already taken");
        }

        Ok(())
    }
}

pub enum WatcherCommand {
    Start,
    Stop,
}

/// Spawns a backend task and calls `log_error` if it ends with an error.
///
/// Such an error ends the task early and the caller can at most observe it through `stop()`,
/// so it is logged once here. This is the only place where the crate logs an error it returns.
pub(crate) fn spawn_backend_task<F>(
    task: F,
    log_error: impl FnOnce(&WatcherError) + Send + 'static,
) -> JoinHandle<Result<(), WatcherError>>
where
    F: Future<Output = Result<(), WatcherError>> + Send + 'static,
{
    tokio::spawn(async move {
        let result = task.await;
        if let Err(err) = &result {
            log_error(err);
        }
        result
    })
}
