use rumqttc::ClientError;
use std::hash::Hasher;
use std::io;
use std::path::PathBuf;
use thiserror::Error;
use tokio::sync::mpsc::error::SendError;
use tokio::task::JoinError;
use twox_hash::XxHash64;

use crate::backend::WatcherCommand;

/// Each `Display` is one line without the cause; the cause is available through `source()`.
#[derive(Debug, Error)]
pub enum WatcherError {
    #[error("file watcher error")]
    Notify(#[from] notify::Error),
    #[error("invalid glob pattern")]
    Pattern(#[from] glob::PatternError),
    #[error("cannot hash file {0}")]
    HashError(PathBuf, #[source] io::Error),
    #[error("watcher task failed")]
    JoinError(#[from] JoinError),
    #[error("cannot read file {0}")]
    FileReadError(PathBuf, #[source] io::Error),
    #[error("Kubernetes API error")]
    KubeError(#[from] kube::Error),
    #[error("Kubernetes watcher error")]
    WatcherError(#[from] kube::runtime::watcher::Error),
    #[error("MQTT client error")]
    MqttClient(#[from] ClientError),
    #[error("cannot send command to watcher")]
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
