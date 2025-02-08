mod config_file_watcher;
mod config_map_watcher;
mod config_mqtt_watcher;

pub use config_file_watcher::*;
pub use config_map_watcher::*;
pub use config_mqtt_watcher::*;
use tokio::sync::watch;

use crate::WatcherError;

#[derive(Debug)]
pub enum DocumentEvent {
    NewDocument(String, String), // New document (ID, Content) added with content
    ContentChanged(String, String), // Content of an existing document changed (ID, Content)
    DocumentRemoved(String),     // Document removed (ID)
}

pub struct WatcherHandle {
    pub(crate) stop_sender: watch::Sender<bool>, // Shutdown signal
    pub(crate) handle: tokio::task::JoinHandle<Result<(), WatcherError>>,
}

impl WatcherHandle {
    /// Stops the watcher task.
    pub async fn stop(self) -> Result<(), WatcherError> {
        let _ = self.stop_sender.send(true); // Send the shutdown signal
        self.handle.await??;
        Ok(())
    }
}
