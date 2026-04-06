# signal-native-channel

Native Signal messenger channel for [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw) using [presage](https://github.com/whisperfish/presage) and [libsignal](https://github.com/signalapp/libsignal).

Communicates directly with Signal servers over WebSocket — no signal-cli, no REST API intermediary.

## Architecture

Runs as a **sidecar process** that bridges Signal (via presage) to ZeroClaw's [bridge WebSocket channel](https://github.com/zeroclaw-labs/zeroclaw/pull/2839) (shipping in the next zeroclaw release after v0.6.8).

```
Signal servers
  |  (WebSocket + Signal Protocol)
  v
signal-bridge          <-- this project
  |  (presage / libsignal)
  |
  |  Bridge WS protocol (localhost:8765/ws)
  v
ZeroClaw daemon
  |  (bridge channel, #2816)
  v
Agent loop
```

### Source layout

| File | Purpose |
|---|---|
| `src/bridge.rs` | Bridge adapter — connects presage to ZeroClaw's bridge WS channel |
| `src/channel.rs` | `SignalNativeChannel` — standalone Channel trait implementation |
| `src/linking.rs` | Secondary device linking (QR code provisioning flow) |
| `src/store.rs` | SQLite store open/init helpers |
| `src/error.rs` | Error types |
| `src/bin/signal_bridge.rs` | Main binary — runs the bridge adapter process |
| `src/bin/link.rs` | One-time device linking binary |

## Prerequisites

- **Rust >= 1.89** (edition 2024)
- **cmake** — needed to build BoringSSL (libsignal dependency)
- **protobuf** — `protoc` compiler for protobuf codegen
- **ZeroClaw** with bridge channel support (next release after v0.6.8)

```bash
brew install cmake protobuf
```

## Build

```bash
git clone <this-repo>
cd signal-native-channel
cargo build
```

This produces two binaries:

- `target/debug/link` — one-time device linking
- `target/debug/signal-bridge` — the bridge adapter (long-running sidecar)

## Quick start

### 1. Link your device (one-time)

```bash
cargo run --bin link -- --device-name ZeroClaw
```

Scan the QR code from your phone: **Signal -> Settings -> Linked Devices -> Link New Device**

### 2. Configure ZeroClaw's bridge channel

Add to `~/.zeroclaw/config.toml`:

```toml
[channels_config.bridge]
bind_host = "127.0.0.1"
bind_port = 8765
path = "/ws"
auth_token = "your-secret-token-here"
allowed_senders = ["signal"]
```

### 3. Start ZeroClaw

```bash
zeroclaw daemon
```

### 4. Start the bridge

```bash
cargo run --bin signal-bridge -- \
  --auth-token your-secret-token-here \
  --sender-id signal \
  --allowed-from '*'
```

Messages from Signal will now flow into ZeroClaw, and ZeroClaw's responses will be sent back through Signal.

## signal-bridge options

| Flag | Default | Description |
|---|---|---|
| `--bridge-url` | `ws://127.0.0.1:8765/ws` | ZeroClaw bridge WebSocket endpoint |
| `--auth-token` | (required) | Shared token matching `[channels_config.bridge].auth_token` |
| `--sender-id` | `signal` | Identity registered with the bridge |
| `--db-path` | `~/.zeroclaw/signal-native.db` | Presage SQLite database path |
| `--allowed-from` | `*` | Comma-separated Signal sender UUIDs, or `*` for all |
| `--group-filter` | (none) | `dm` for DMs only, or a hex group master key |

## Bridge protocol

The adapter speaks the bridge WebSocket protocol defined in [zeroclaw-labs/zeroclaw#2816](https://github.com/zeroclaw-labs/zeroclaw/issues/2816):

**Inbound (adapter -> ZeroClaw):**
- `{"type":"auth","token":"...","sender_id":"..."}` — first message, authenticates the connection
- `{"type":"message","id":"...","sender_id":"...","content":"..."}` — forwarded Signal message

**Outbound (ZeroClaw -> adapter):**
- `{"type":"ready","sender_id":"...","endpoint":"..."}` — auth accepted
- `{"type":"message","recipient":"...","content":"..."}` — send a message via Signal
- `{"type":"typing","recipient":"...","active":true}` — typing indicator
- `{"type":"draft",...}` — draft lifecycle events (start/update/finalize/cancel)
- `{"type":"error","code":"...","message":"..."}` — error from bridge

The adapter routes `message` and `typing` outbound events through presage. Draft and reaction events are acknowledged but have no Signal-side equivalent.

## Test

```bash
cargo test
cargo check
```

End-to-end testing requires a linked device and a running ZeroClaw bridge:

1. Link a device (`cargo run --bin link`)
2. Start ZeroClaw with bridge config
3. Start `signal-bridge`
4. Send a message to your Signal number from another account
5. Verify ZeroClaw processes it and responds

## Contributing

### What's implemented

- Receiving text messages (DM and group) and forwarding to ZeroClaw
- Sending text messages back through Signal (by UUID or group master key)
- Typing indicators
- Auth handshake with the bridge
- Automatic reconnection with exponential backoff

### Not yet implemented

- **Phone number -> UUID resolution** via CDSI contact discovery
- **Attachment send/receive** (presage supports `manager.get_attachment()` / `manager.upload_attachments()`)
- **Reactions** (bridge protocol supports them, presage can send them)
- **Read receipts**

### Code style

- `cargo fmt` before committing
- `cargo clippy` must pass
- No unwrap in library code (use `anyhow` / `thiserror`)
- Logging via `tracing` macros

### Dependency notes

presage and libsignal are not on crates.io — they are pulled as git dependencies. The `[patch.crates-io]` section in `Cargo.toml` is required for two forks:

- `curve25519-dalek` — Signal's fork
- `libsqlite3-sys` — Whisperfish's fork with `bundled-sqlcipher-custom-crypto`

Do not remove these patches or the build will fail.

## License

MIT OR Apache-2.0 (same as ZeroClaw)

presage, libsignal-service-rs, and libsignal are AGPL-3.0. Binary distribution that links these crates must comply with AGPL terms.
