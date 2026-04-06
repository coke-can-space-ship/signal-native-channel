# signal-native-channel

Native Signal messenger channel for [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw) using [presage](https://github.com/whisperfish/presage) and [libsignal](https://github.com/signalapp/libsignal).

Communicates directly with Signal servers over WebSocket — no signal-cli, no REST API intermediary.

## Architecture

```
signal-native-channel
  |
  presage              (high-level client: link, send, receive, groups, storage)
  |
  libsignal-service-rs (server protocol, protobuf, transport)
  |
  libsignal            (Signal Protocol crypto, sealed sender, zkgroup)
```

### Source layout

| File | Purpose |
|---|---|
| `src/channel.rs` | `SignalNativeChannel` — implements the ZeroClaw Channel interface |
| `src/linking.rs` | Secondary device linking (QR code provisioning flow) |
| `src/store.rs` | SQLite store open/init helpers |
| `src/error.rs` | Error types |
| `src/bin/link.rs` | Standalone binary for one-time device linking |

## Prerequisites

- **Rust >= 1.89** (edition 2024)
- **cmake** — needed to build BoringSSL (libsignal dependency)
- **protobuf** — `protoc` compiler for protobuf codegen

```bash
brew install cmake protobuf
```

## Build

```bash
git clone <this-repo>
cd signal-native-channel
cargo build
```

## Link your device

Before using the channel, you need to link it as a secondary device to your Signal account. This is a one-time operation.

```bash
cargo run --bin link -- --device-name ZeroClaw --db-path ~/.zeroclaw/signal-native.db
```

This prints a QR code to the terminal. Scan it from your phone:

**Signal app -> Settings -> Linked Devices -> Link New Device**

The `--db-path` flag controls where Signal protocol state (keys, sessions, contacts) is stored. Defaults to `~/.zeroclaw/signal-native.db` if omitted.

The `--device-name` flag sets the name shown in Signal's Linked Devices list. Defaults to `ZeroClaw` if omitted.

## Test

```bash
cargo test
cargo check
```

There are no integration tests yet (they require a linked Signal account). Unit tests cover error types and store path logic. To test end-to-end:

1. Link a device (see above)
2. Send a message to your Signal number from another account
3. Verify the channel receives and logs it

## Contributing

### Adding features

The channel currently supports:

- Receiving text messages (DM and group)
- Sending text messages (by UUID or group master key)
- Typing indicators
- Sync messages from primary device

Not yet implemented:

- **Phone number -> UUID resolution** via CDSI contact discovery (recipients must be UUIDs for now)
- **Attachment send/receive** (presage supports this via `manager.get_attachment()` / `manager.upload_attachments()`)
- **Reactions** via `add_reaction` / `remove_reaction` channel trait methods
- **Read receipts**
- **Group management** (join, leave, create)

### Code style

Follow ZeroClaw conventions from [AGENTS.md](https://github.com/zeroclaw-labs/zeroclaw/blob/main/AGENTS.md). In short:

- `cargo fmt` before committing
- `cargo clippy` must pass
- No unwrap in library code (use `anyhow` / `thiserror`)
- Logging via `tracing` macros

### Dependency notes

presage and libsignal are not on crates.io — they are pulled as git dependencies. The `[patch.crates-io]` section in `Cargo.toml` is required for two forks:

- `curve25519-dalek` — Signal's fork with custom optimizations
- `libsqlite3-sys` — Whisperfish's fork with `bundled-sqlcipher-custom-crypto` feature

Do not remove these patches or the build will fail.

## Integrating into ZeroClaw

### 1. Add the dependency

In ZeroClaw's `Cargo.toml`, add signal-native-channel as a path or git dependency and copy the `[patch.crates-io]` entries:

```toml
[dependencies]
signal-native-channel = { path = "../signal-native-channel" }
# or
signal-native-channel = { git = "<repo-url>" }

[patch.crates-io]
curve25519-dalek = { git = "https://github.com/signalapp/curve25519-dalek", tag = "signal-curve25519-4.1.3" }
libsqlite3-sys = { version = "0.36.0", git = "https://github.com/whisperfish/rusqlite", rev = "2a42b3354c9194700d08aa070f70a131a470e7dc" }
```

### 2. Replace stub types

`src/channel.rs` defines stub versions of `ChannelMessage`, `SendMessage`, and `MediaAttachment`. Replace them with real imports:

```rust
use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
use crate::channels::media_pipeline::MediaAttachment;
```

Add `#[async_trait]` and change the inherent methods to trait impl:

```rust
#[async_trait]
impl Channel for SignalNativeChannel {
    fn name(&self) -> &str { ... }
    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> { ... }
    async fn listen(&self, tx: mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> { ... }
    async fn health_check(&self) -> bool { ... }
    async fn start_typing(&self, recipient: &str) -> anyhow::Result<()> { ... }
}
```

### 3. Add config section

Add a new config struct in ZeroClaw's config module:

```toml
[channels_config.signal_native]
db_path = "~/.zeroclaw/signal-native.db"
device_name = "ZeroClaw"
allowed_from = ["+12624003837"]
group_id = "dm"
```

### 4. Register the channel

In `src/channels/mod.rs` inside `collect_configured_channels()`, add:

```rust
if let Some(ref sn) = config.channels_config.signal_native {
    channels.push(ConfiguredChannel {
        display_name: "Signal (native)",
        channel: Arc::new(SignalNativeChannel::new(
            PathBuf::from(shellexpand::tilde(&sn.db_path).to_string()),
            sn.device_name.clone(),
            sn.allowed_from.clone(),
            sn.group_id.clone(),
        )),
    });
}
```

### 5. Feature-gate (optional)

Consider gating behind a cargo feature like `channel-signal-native` to keep the default binary lean, since presage pulls in libsignal, BoringSSL, and ~300 transitive dependencies.

## License

MIT OR Apache-2.0 (same as ZeroClaw)

presage, libsignal-service-rs, and libsignal are AGPL-3.0. Binary distribution that links these crates must comply with AGPL terms.
