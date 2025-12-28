# Config Watcher

Config Watcher is a Rust library that provides a unified way to read and track configuration items from various sources, including files, Kubernetes ConfigMaps, and MQTT topics. It allows applications to receive real-time updates on configuration changes, ensuring dynamic and reactive behavior. The library supports structured data formats such as YAML and JSON, regardless of the backend source.

## Features

- **Multi-Backend Configuration Monitoring**: Supports file-based configurations, Kubernetes ConfigMaps, and MQTT topics.
- **Event-Based Updates**: Emits events on a per-item basis when configuration changes occur.
- **Customizable Tokenization and Deserialization**: Allows users to define how configuration data is parsed and structured.
- **Config Item Watcher Interface**: Provides a single API to interact with different backends uniformly.
- **Supports Multiple Configuration Sources**: Monitors directories of configuration files, Kubernetes namespaces, or MQTT topics and aggregates their contents.

## Installation

Install via cargo:

```bash
cargo add config_watcher
```

## How It Works

Config Watcher reads structured configuration data from supported backends and emits events when configuration items change. Users can define:

- **Backends**: Where the data comes from (files, Kubernetes, MQTT), all managed under `ConfigItemWatcher`.
- **Tokenization**: How to split configuration documents into meaningful items.
- **Deserialization**: How raw data should be converted into structured objects.

## Config Item Watcher and Backends

Each backend has its own parameters that must be provided when initializing a watcher:

### 1. File System Watcher

Monitors local files for changes and updates the configuration dynamically.

```rust
use config_watcher::backend::run_config_file_watcher;
use std::time::Duration;

let watcher = run_config_file_watcher("/config", "*.yaml", Duration::from_secs(1));
```

**Parameters:**

- `watch_path: impl AsRef<Path>` – The directory or file path to watch.
- `file_pattern: impl Into<String>` – The glob pattern to match files (e.g., `*.yaml`).
- `debounce: Duration` – The debounce interval for reducing redundant events.

### 2. Kubernetes ConfigMap Watcher

Tracks Kubernetes ConfigMaps and provides live updates when the configuration changes.

```rust
use config_watcher::backend::run_configmap_watcher;

let watcher = run_configmap_watcher("config-map-name".to_string(), "namespace".to_string());
```

**Parameters:**

- `configmap_name: String` – Name of the ConfigMap.
- `namespace: String` – Kubernetes namespace containing the ConfigMap.

### 3. MQTT Watcher

Subscribes to an MQTT topic and listens for configuration updates. It uses `rumqttc::MqttOptions` to configure the MQTT connection.

```rust
use config_watcher::backend::run_mqtt_watcher;
use rumqttc::MqttOptions;

let mut mqtt_options = MqttOptions::new("config-watcher-client", "mqtt-broker-host", 1883);
mqtt_options.set_credentials("user", "password");

let watcher = run_mqtt_watcher(mqtt_options, "config/topic", 1024).expect("Failed to start MQTT watcher");
```

**Parameters:**

- `mqttoptions: MqttOptions` – MQTT connection options.
- `config_topic: &str` – MQTT topic to subscribe to.
- `channel_size: usize` – Size of the message channel.

## Event Handling

Config Watcher uses content-based hashing to track configuration changes. Because of this, it does not provide traditional "update" events. Instead, when an item changes, it is reported as a **removal** followed by an **addition** with the updated content. This ensures that even minor changes are properly detected and processed.

Config Watcher provides real-time updates for configuration items by leveraging `ConfigItemWatcher`, which manages backends and ensures a consistent interface for receiving updates.

### Explanation of `ConfigItemEvent` Variants

- **NewDocument(u64, String)**: Represents a completely new document being added. The `u64` is an internal identifier used to track the document, and the `String` represents the document path (filename in the filesystem, attribute in a ConfigMap, or topic in MQTT). This allows applications to map document IDs to paths and display relevant information.
- **RemoveDocument(u64)**: Indicates that a document was removed. The `u64` identifier allows the system to properly correlate the deletion with previous content.
- **New(ConfigItemHash, T)**: Represents a new configuration item being introduced inside an existing document. The `ConfigItemHash` is a hash-based identifier ensuring unique tracking, and `T` is the deserialized configuration object.
- **Removed(ConfigItemHash)**: Signifies that a specific configuration item has been removed. The hash ensures that only the affected item is processed without interfering with unrelated configurations.

### How to Use `run_config_item_watcher`

The `run_config_item_watcher` function is responsible for managing configuration watchers. To use it, you need to:

1. **Choose a backend** – Specify whether the configuration source is a file system, Kubernetes ConfigMap, or MQTT topic.
2. **Provide a tokenizer** – Define how the document is split into configuration items.
3. **Define a deserializer** – Convert raw configuration data into structured objects.

When executed, the function returns a handle to manage the watcher and a receiver that emits events when configurations change. The application can then react to these events dynamically.

### Example Usage:

```rust
// Initialize the configuration watcher
let (watcher_handle, mut receiver) = run_config_item_watcher(|| {
    // Choose the backend: in this case, watching a directory for YAML files
    backend::run_config_file_watcher("/config", "*.yaml", Duration::from_secs(1))
}, &YamlTokenizer, deserialize_my_config)?;

watcher_handle.start().await.unwrap();

// Continuously listen for configuration updates
while let Some(event) = receiver.recv().await {
    match event {
        // Handle newly added documents
        ConfigItemEvent::NewDocument(id, path) => {
            // `id` is an internal identifier for tracking the document
            // `path` represents the document path (filename, attribute in a ConfigMap, or topic in MQTT)
            // This allows the application to map file IDs to filenames and display relevant paths to users
            println!("New document detected: {}", id);
            // Process the new document
        },
        // Handle removed documents
        ConfigItemEvent::RemoveDocument(id) => {
            println!("Document removed: {}", id);
            // Perform any necessary cleanup
        },
        // Handle newly added configuration items
        ConfigItemEvent::New(hash, item) => {
            println!("New item detected: {:?}", hash);
            // Process the new configuration item
        },
        // Handle removed configuration items
        ConfigItemEvent::Removed(hash) => {
            println!("Configuration item removed: {:?}", hash);
            // Perform any necessary cleanup
        },
    }
}

```

### TLS Crypto Provides

`config_watcher` will use `aws-lc-rs` as default TLS backend for kube (which will install it globally).
If you prefer to use another provider manually in your application set default features to false:

```toml
[dependencies]
config_watcher = { version = "0.10.1", default-features = false }
```

Please note that you will need to install the provider manually in your application in this case, otherwise your application will panic at runtime!

## Contributing

Contributions are welcome! Please submit a pull request or open an issue for discussion.

## License

This project is licensed under the MIT License. See [LICENSE](LICENSE) for details.
