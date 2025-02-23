mod config_file_watcher;
mod config_map_watcher;
mod config_mqtt_watcher;

pub use config_file_watcher::*;
pub use config_map_watcher::*;
pub use config_mqtt_watcher::*;
use tokio::sync::mpsc;

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
            log::warn!("Task handle was already taken or not initialized.");
        }

        Ok(())
    }
}

pub enum WatcherCommand {
    Start,
    Stop,
}
