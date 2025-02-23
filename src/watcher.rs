use rumqttc::ClientError;
use std::hash::Hasher;
use std::io;
use std::path::PathBuf;
use thiserror::Error;
use tokio::sync::mpsc::error::SendError;
use tokio::task::JoinError;
use twox_hash::XxHash64;

use crate::backend::WatcherCommand;

#[derive(Debug, Error)]
pub enum WatcherError {
    #[error("Error watching files {:#?}", .0)]
    Notify(#[from] notify::Error),
    #[error("Error matching glob pattern {:#?}", .0)]
    Pattern(#[from] glob::PatternError),
    #[error("Error hashing file [{0}]: {1:?}")]
    HashError(PathBuf, io::Error),
    #[error("Error waiting for watch task to complete: {0} -- {0:#?}")]
    JoinError(#[from] JoinError),
    #[error("Error reading file [{0}]: {1:?}")]
    FileReadError(PathBuf, io::Error),
    #[error("Kubernetes API error: {0}")]
    KubeError(#[from] kube::Error),
    #[error("Kubernetes watcher API error: {0}")]
    WatcherError(#[from] kube::runtime::watcher::Error),
    #[error("Mqtt Client error: {0}")]
    MqttClient(#[from] ClientError),
    #[error("Error sending command to watcher {0}")]
    SendError(#[from] SendError<WatcherCommand>),
}

pub fn hash_str(data: &str) -> u64 {
    let mut hasher = XxHash64::default();
    hasher.write(data.as_bytes());
    hasher.finish()
}

pub trait Tokenizer: Send + Sync {
    fn tokenize<'a>(&self, content: &'a str) -> Box<dyn Iterator<Item = &'a str> + 'a>;
}

pub struct YamlTokenizer;

impl Tokenizer for YamlTokenizer {
    fn tokenize<'a>(&self, content: &'a str) -> Box<dyn Iterator<Item = &'a str> + 'a> {
        Box::new(
            content
                .split("\n---")
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        )
    }
}

pub struct JsonTokenizer;

impl Tokenizer for JsonTokenizer {
    fn tokenize<'a>(&self, content: &'a str) -> Box<dyn Iterator<Item = &'a str> + 'a> {
        Box::new(
            content
                .split("\n}\n{")
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        )
    }
}
