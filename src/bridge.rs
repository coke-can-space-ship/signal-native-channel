//! Bridge adapter: connects presage (Signal) to ZeroClaw's bridge WebSocket channel.
//!
//! Speaks the bridge protocol defined in zeroclaw-labs/zeroclaw#2816:
//! - Connects to the bridge WS endpoint
//! - Authenticates with `{"type":"auth","token":"...","sender_id":"..."}`
//! - Forwards inbound Signal messages as `{"type":"message",...}`
//! - Receives outbound events from ZeroClaw and routes them through presage

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use presage::libsignal_service::content::ContentBody;
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::manager::Registered;
use presage::model::messages::Received;
use presage::Manager;
use presage_store_sqlite::SqliteStore;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::linking;

const GROUP_TARGET_PREFIX: &str = "group:";
const GROUP_MASTER_KEY_LEN: usize = 32;

// ---------------------------------------------------------------------------
// Bridge protocol types (matching zeroclaw bridge.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
enum BridgeInbound {
    Auth {
        token: String,
        sender_id: String,
    },
    Message {
        id: String,
        sender_id: String,
        content: String,
    },
    Ping {
        nonce: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
enum BridgeOutbound {
    Ready {
        sender_id: String,
        endpoint: String,
    },
    Error {
        code: String,
        message: String,
    },
    Message {
        id: String,
        recipient: String,
        content: String,
        #[serde(default)]
        subject: Option<String>,
        #[serde(default)]
        thread_ts: Option<String>,
    },
    Typing {
        recipient: String,
        active: bool,
    },
    #[serde(rename = "draft")]
    Draft {
        recipient: String,
        message_id: String,
        event: String,
        #[serde(default)]
        text: Option<String>,
    },
    Reaction {
        action: String,
        channel_id: String,
        message_id: String,
        emoji: String,
    },
    Ack {
        id: String,
    },
    Pong {
        #[serde(default)]
        nonce: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Bridge adapter configuration
// ---------------------------------------------------------------------------

/// Configuration for the bridge adapter process.
pub struct BridgeAdapterConfig {
    /// WebSocket URL of the zeroclaw bridge endpoint (e.g. `ws://127.0.0.1:8765/ws`).
    pub bridge_url: String,
    /// Shared auth token matching `[channels_config.bridge].auth_token` in zeroclaw.
    pub auth_token: String,
    /// Sender ID to register with. This is the identity for all Signal messages
    /// forwarded through this adapter.
    pub sender_id: String,
    /// Path to the presage SQLite database.
    pub db_path: PathBuf,
    /// Phone numbers (E.164), UUIDs, or `"*"` for wildcard allowed senders.
    pub allowed_from: Vec<String>,
    /// Optional group filter (`"dm"` for DMs only, or a hex group master key).
    pub group_filter: Option<String>,
    /// Path to the zeroclaw session JSONL file to watch for `sessions_send` messages.
    /// When set, the adapter watches for new entries and delivers them to Signal.
    pub session_file: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Bridge adapter runtime
// ---------------------------------------------------------------------------

/// Run the bridge adapter. This is the main entrypoint for the `signal-bridge` binary.
///
/// Connects to the zeroclaw bridge WebSocket, authenticates, then:
/// - Spawns a presage listener that forwards Signal messages to zeroclaw
/// - Reads outbound events from zeroclaw and sends them via presage
pub async fn run(config: BridgeAdapterConfig) -> anyhow::Result<()> {
    let manager = linking::load_registered(&config.db_path).await.map_err(|e| {
        anyhow::anyhow!(
            "no linked device found at {}. Run `signal-bridge link` first. Error: {e}",
            config.db_path.display()
        )
    })?;
    tracing::info!("signal-native: loaded linked device");

    let manager = Arc::new(Mutex::new(manager));

    let mut retry_delay = Duration::from_secs(2);
    let max_delay = Duration::from_secs(60);

    loop {
        match run_session(&config, Arc::clone(&manager)).await {
            Ok(()) => {
                tracing::info!("bridge session ended cleanly, reconnecting...");
                retry_delay = Duration::from_secs(2);
            }
            Err(e) => {
                tracing::warn!("bridge session error: {e}, reconnecting in {retry_delay:?}");
            }
        }
        tokio::time::sleep(retry_delay).await;
        retry_delay = (retry_delay * 2).min(max_delay);
    }
}

async fn run_session(
    config: &BridgeAdapterConfig,
    manager: Arc<Mutex<Manager<SqliteStore, Registered>>>,
) -> anyhow::Result<()> {
    // Connect to bridge
    let (ws_stream, _) = tokio_tungstenite::connect_async(&config.bridge_url).await?;
    let (mut ws_sink, mut ws_source) = ws_stream.split();
    tracing::info!("connected to bridge at {}", config.bridge_url);

    // Authenticate
    let auth = BridgeInbound::Auth {
        token: config.auth_token.clone(),
        sender_id: config.sender_id.clone(),
    };
    ws_sink
        .send(WsMessage::Text(serde_json::to_string(&auth)?.into()))
        .await?;

    // Wait for Ready or Error
    let ready_msg = tokio::time::timeout(Duration::from_secs(15), ws_source.next())
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for bridge auth response"))?
        .ok_or_else(|| anyhow::anyhow!("bridge closed before auth response"))??;

    let ready_text = match ready_msg {
        WsMessage::Text(t) => t,
        other => anyhow::bail!("unexpected message type during auth: {other:?}"),
    };

    match serde_json::from_str::<BridgeOutbound>(&ready_text)? {
        BridgeOutbound::Ready {
            sender_id,
            endpoint,
        } => {
            tracing::info!("bridge authenticated as {sender_id} on {endpoint}");
        }
        BridgeOutbound::Error { code, message } => {
            anyhow::bail!("bridge auth rejected: [{code}] {message}");
        }
        _ => {
            anyhow::bail!("unexpected bridge response during auth");
        }
    }

    // Use mpsc channels to decouple the non-Send presage stream from tokio::spawn.
    // Signal listener runs on the current task (not spawned) because presage's
    // receive_messages() stream is !Send.
    let (signal_tx, mut signal_rx) = mpsc::channel::<String>(64);
    let (bridge_event_tx, mut bridge_event_rx) = mpsc::channel::<BridgeOutbound>(64);

    // Track the last Signal sender so we can route replies back.
    // Key: bridge sender_id ("signal"), Value: Signal UUID or group target.
    let mut last_signal_sender: Option<String> = None;

    // Channel for messages from the session file watcher.
    let (session_msg_tx, mut session_msg_rx) = mpsc::channel::<String>(16);

    // Shared set of message contents we forwarded FROM Signal, so the session
    // watcher can skip them (avoid echo loop).
    let sent_from_signal: Arc<Mutex<std::collections::VecDeque<String>>> =
        Arc::new(Mutex::new(std::collections::VecDeque::new()));

    // Task: watch the session JSONL file for `sessions_send` injections.
    if let Some(ref session_file) = config.session_file {
        let session_file = session_file.clone();
        let tx = session_msg_tx.clone();
        let sent_from_signal = Arc::clone(&sent_from_signal);
        tokio::task::spawn_local(async move {
            if let Err(e) = watch_session_file(&session_file, tx, sent_from_signal).await {
                tracing::warn!("session file watcher error: {e}");
            }
        });
    }
    drop(session_msg_tx); // drop our copy so the channel closes when the watcher stops

    // Task: read from bridge WS, forward parsed events via mpsc
    let bridge_reader = tokio::task::spawn_local(async move {
        while let Some(msg) = ws_source.next().await {
            let text = match msg {
                Ok(WsMessage::Text(t)) => t,
                Ok(WsMessage::Ping(_) | WsMessage::Pong(_)) => continue,
                Ok(WsMessage::Close(_)) => {
                    tracing::info!("bridge sent close frame");
                    break;
                }
                Ok(_) => continue,
                Err(e) => {
                    tracing::warn!("bridge ws read error: {e}");
                    break;
                }
            };
            tracing::info!("bridge ws frame: {}", &text[..text.len().min(200)]);
            match serde_json::from_str::<BridgeOutbound>(&text) {
                Ok(event) => {
                    if bridge_event_tx.send(event).await.is_err() {
                        break;
                    }
                }
                Err(e) => tracing::warn!("failed to parse bridge event: {e} | raw: {}", &text[..text.len().min(300)]),
            }
        }
    });

    // Task: write to bridge WS (drains signal_rx mpsc)
    let bridge_writer = tokio::task::spawn_local(async move {
        while let Some(payload) = signal_rx.recv().await {
            if ws_sink
                .send(WsMessage::Text(payload.into()))
                .await
                .is_err()
            {
                tracing::warn!("bridge ws write failed");
                break;
            }
        }
    });

    // Current task: run the Signal listener (non-Send stream stays here)
    // and also poll bridge events in a select loop.
    // The presage stream can die independently of the bridge WS, so we
    // reconnect it in a loop without tearing down the bridge connection.
    let mut signal_retry_delay = Duration::from_secs(2);
    let signal_max_delay = Duration::from_secs(60);

    'outer: loop {
        let messages = {
            let mut guard = manager.lock().await;
            match guard.receive_messages().await {
                Ok(stream) => {
                    signal_retry_delay = Duration::from_secs(2);
                    stream
                }
                Err(e) => {
                    tracing::warn!(
                        "signal: receive_messages failed: {e}, retrying in {signal_retry_delay:?}"
                    );
                    tokio::time::sleep(signal_retry_delay).await;
                    signal_retry_delay = (signal_retry_delay * 2).min(signal_max_delay);
                    continue;
                }
            }
        };
        futures_util::pin_mut!(messages);

        loop {
            tokio::select! {
                signal_item = messages.next() => {
                    let Some(item) = signal_item else {
                        tracing::info!("signal: message stream ended, reconnecting presage...");
                        break; // break inner loop, reconnect presage in outer loop
                    };
                    match item {
                        Received::QueueEmpty => {
                            tracing::debug!("signal: initial queue drained");
                        }
                        Received::Contacts => {
                            tracing::debug!("signal: contacts synced");
                        }
                        Received::Content(ref content) => {
                            if let Some((bridge_msg, signal_reply_addr)) = content_to_bridge_message(
                                content,
                                &config.allowed_from,
                                config.group_filter.as_deref(),
                                &config.sender_id,
                            ) {
                                // Track this content so session watcher skips it (avoid echo)
                                if let BridgeInbound::Message { content: ref msg_content, .. } = bridge_msg {
                                    let mut guard = sent_from_signal.lock().await;
                                    guard.push_back(msg_content.clone());
                                    if guard.len() > 50 { guard.pop_front(); }
                                }
                                last_signal_sender = Some(signal_reply_addr);
                                let payload = serde_json::to_string(&bridge_msg)?;
                                if signal_tx.send(payload).await.is_err() {
                                    tracing::info!("bridge writer closed");
                                    break 'outer;
                                }
                            }
                        }
                    }
                }
                bridge_event = bridge_event_rx.recv() => {
                    let Some(event) = bridge_event else {
                        tracing::info!("bridge event channel closed");
                        break 'outer;
                    };
                    tracing::info!("received bridge event: {event:?}");
                    if let Err(e) = handle_outbound_event(&manager, event, last_signal_sender.as_deref()).await {
                        tracing::warn!("failed to handle bridge event: {e}");
                    }
                }
                session_msg = session_msg_rx.recv() => {
                    if let Some(content) = session_msg {
                        if let Some(ref recipient) = last_signal_sender {
                            tracing::info!("session_send detected, delivering to Signal: {}", &content[..content.len().min(100)]);
                            if let Err(e) = send_signal_message(&manager, recipient, content).await {
                                tracing::warn!("failed to deliver session_send message: {e}");
                            }
                        } else {
                            tracing::warn!("session_send message but no Signal sender known, dropping");
                        }
                    }
                }
            }
        }
    }

    drop(signal_tx);
    drop(bridge_event_rx);
    let _ = bridge_reader.await;
    let _ = bridge_writer.await;

    Ok(())
}

/// Convert a presage Content into a bridge inbound message event.
///
/// The `sender_id` in the bridge message is set to the Signal sender's UUID
/// (or `group:<hex_master_key>` for group messages) so that zeroclaw's reply
/// routes back to the correct recipient via `parse_recipient`.
/// Returns `(bridge_message, signal_reply_address)`.
///
/// The bridge message's `sender_id` is set to `bridge_sender_id` (the auth
/// identity) to satisfy the bridge's sender_mismatch check. The actual Signal
/// UUID is returned separately for reply routing.
fn content_to_bridge_message(
    content: &presage::libsignal_service::content::Content,
    allowed_from: &[String],
    group_filter: Option<&str>,
    bridge_sender_id: &str,
) -> Option<(BridgeInbound, String)> {
    let body = match &content.body {
        ContentBody::DataMessage(dm) => dm,
        ContentBody::SynchronizeMessage(_) => return None,
        _ => return None,
    };

    let from_uuid = content.metadata.sender.raw_uuid().to_string();

    // Allowlist check
    if !allowed_from.iter().any(|s| s == "*" || s == &from_uuid) {
        return None;
    }

    // Group filter
    let group_master_key = body
        .group_v2
        .as_ref()
        .and_then(|g| g.master_key.as_deref())
        .map(hex_encode);

    if let Some(filter) = group_filter {
        if filter.eq_ignore_ascii_case("dm") {
            if group_master_key.is_some() {
                return None;
            }
        } else {
            match &group_master_key {
                Some(gk) if gk == filter => {}
                _ => return None,
            }
        }
    }

    let timestamp_ms = content.metadata.timestamp;
    let reply_address = if let Some(ref gk) = group_master_key {
        format!("{GROUP_TARGET_PREFIX}{gk}")
    } else {
        from_uuid.clone()
    };

    // Handle reactions (emoji on a message)
    if let Some(ref reaction) = body.reaction {
        let emoji = reaction.emoji.as_deref().unwrap_or("?");
        let is_remove = reaction.remove.unwrap_or(false);
        let target_ts = reaction.target_sent_timestamp.unwrap_or(0);

        let action = if is_remove { "removed" } else { "reacted" };
        let message_text = format!(
            "[{action} {emoji} on message signative_{target_ts}]"
        );
        tracing::info!("signal reaction: {from_uuid} {action} {emoji} on ts {target_ts}");

        return Some((
            BridgeInbound::Message {
                id: format!("signative_{timestamp_ms}_{}", &from_uuid[..8]),
                sender_id: bridge_sender_id.to_string(),
                content: message_text,
            },
            reply_address,
        ));
    }

    let text = body.body.as_deref().filter(|t| !t.is_empty())?;

    // Handle quoted replies (replying to a specific message)
    let message_text = if let Some(ref quote) = body.quote {
        let quoted_text = quote.text.as_deref().unwrap_or("");
        let quoted_ts = quote.id.unwrap_or(0);
        // Include the quote context so zeroclaw knows what the user is replying to
        let snippet = if quoted_text.len() > 100 {
            format!("{}...", &quoted_text[..100])
        } else {
            quoted_text.to_string()
        };
        format!(
            "[replying to signative_{quoted_ts}: \"{snippet}\"]\n\n{text}"
        )
    } else {
        text.to_string()
    };

    Some((
        BridgeInbound::Message {
            id: format!("signative_{timestamp_ms}_{}", &from_uuid[..8]),
            sender_id: bridge_sender_id.to_string(),
            content: message_text,
        },
        reply_address,
    ))
}

/// Watch a zeroclaw sessions directory for `sessions_send` messages.
///
/// `sessions_send` appends `{"role":"user","content":"..."}` entries to
/// session JSONL files. The target session may be any `bridge_*.jsonl` file
/// in the directory — not necessarily the main conversation session.
///
/// We scan all matching files for new entries and forward their content.
async fn watch_session_file(
    path: &std::path::Path,
    tx: mpsc::Sender<String>,
    sent_from_signal: Arc<Mutex<VecDeque<String>>>,
) -> anyhow::Result<()> {
    // `path` points to a specific JSONL file. Watch its parent directory
    // for ALL bridge_*.jsonl files.
    let sessions_dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("session file has no parent directory"))?
        .to_path_buf();

    tracing::info!("session watcher started: watching {}", sessions_dir.display());

    // Track file sizes to detect new content.
    let mut file_sizes: std::collections::HashMap<PathBuf, u64> = std::collections::HashMap::new();

    // Initialize: record current sizes of all matching files.
    if let Ok(mut entries) = tokio::fs::read_dir(&sessions_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let p = entry.path();
            if is_bridge_session_file(&p) {
                if let Ok(meta) = tokio::fs::metadata(&p).await {
                    file_sizes.insert(p, meta.len());
                }
            }
        }
    }

    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;

        let mut entries = match tokio::fs::read_dir(&sessions_dir).await {
            Ok(e) => e,
            Err(_) => continue,
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let p = entry.path();
            if !is_bridge_session_file(&p) {
                continue;
            }

            let current_size = match tokio::fs::metadata(&p).await {
                Ok(meta) => meta.len(),
                Err(_) => continue,
            };

            let prev_size = file_sizes.get(&p).copied().unwrap_or(0);
            if current_size <= prev_size {
                continue;
            }

            // File grew — read the new bytes.
            file_sizes.insert(p.clone(), current_size);

            if let Ok(data) = tokio::fs::read(&p).await {
                // Only look at bytes after prev_size.
                let new_bytes = &data[prev_size as usize..];
                let new_text = String::from_utf8_lossy(new_bytes);

                for line in new_text.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
                        let role = entry.get("role").and_then(|v| v.as_str()).unwrap_or("");
                        let content = entry.get("content").and_then(|v| v.as_str()).unwrap_or("");

                        if role == "user" && !content.is_empty() {
                            let is_echo = {
                                let mut guard = sent_from_signal.lock().await;
                                if let Some(pos) = guard.iter().position(|s| s == content) {
                                    guard.remove(pos);
                                    true
                                } else {
                                    false
                                }
                            };
                            if !is_echo {
                                tracing::info!(
                                    "session watcher: sessions_send detected in {}",
                                    p.file_name().unwrap_or_default().to_string_lossy()
                                );
                                if tx.send(content.to_string()).await.is_err() {
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn is_bridge_session_file(path: &std::path::Path) -> bool {
    path.extension().is_some_and(|ext| ext == "jsonl")
        && path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("bridge"))
}

/// Resolve a bridge recipient to the actual Signal address.
fn resolve_recipient(recipient: &str, last_signal_sender: Option<&str>) -> anyhow::Result<String> {
    match parse_recipient(recipient) {
        Ok(_) => Ok(recipient.to_string()),
        Err(_) => last_signal_sender
            .ok_or_else(|| {
                anyhow::anyhow!("cannot route reply to '{recipient}': no Signal sender known yet")
            })
            .map(String::from),
    }
}

/// Send a text message with optional file attachments via presage.
///
/// Scans the message text for file paths (lines matching existing files).
/// Found files are uploaded as Signal attachments alongside the text.
async fn send_signal_message(
    manager: &Arc<Mutex<Manager<SqliteStore, Registered>>>,
    recipient: &str,
    text: String,
) -> anyhow::Result<()> {
    let target = parse_recipient(recipient)?;
    let timestamp = now_millis();

    // Detect file paths in the message text and upload as attachments.
    let (file_paths, clean_text) = extract_file_paths(&text);
    let mut attachment_pointers = Vec::new();

    if !file_paths.is_empty() {
        let mut guard = manager.lock().await;
        for path in &file_paths {
            match upload_file_attachment(&mut *guard, path).await {
                Ok(pointer) => {
                    tracing::info!("uploaded attachment: {}", path.display());
                    attachment_pointers.push(pointer);
                }
                Err(e) => {
                    tracing::warn!("failed to upload attachment {}: {e}", path.display());
                }
            }
        }
        drop(guard);
    }

    let body_text = if attachment_pointers.is_empty() {
        text
    } else {
        clean_text
    };

    let data_message = presage::libsignal_service::content::DataMessage {
        body: Some(body_text),
        timestamp: Some(timestamp),
        attachments: attachment_pointers,
        ..Default::default()
    };

    let mut guard = manager.lock().await;
    match target {
        RecipientTarget::Direct(service_id) => {
            guard
                .send_message(service_id, data_message, timestamp)
                .await
                .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;
        }
        RecipientTarget::Group(key_bytes) => {
            guard
                .send_message_to_group(&key_bytes, data_message, timestamp)
                .await
                .map_err(|e| anyhow::anyhow!("group send failed: {e}"))?;
        }
    }

    Ok(())
}

/// Upload a local file as a Signal attachment.
async fn upload_file_attachment(
    manager: &mut Manager<SqliteStore, Registered>,
    path: &std::path::Path,
) -> anyhow::Result<presage::proto::AttachmentPointer> {
    use presage::libsignal_service::sender::AttachmentSpec;

    let data = tokio::fs::read(path).await?;
    let content_type = mime_guess::from_path(path)
        .first()
        .map(|m| m.to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string());

    let spec = AttachmentSpec {
        content_type,
        length: data.len(),
        file_name,
        preview: None,
        voice_note: None,
        borderless: None,
        width: None,
        height: None,
        caption: None,
        blur_hash: None,
    };

    let result = manager
        .upload_attachment(spec, data)
        .await
        .map_err(|e| anyhow::anyhow!("upload transport error: {e}"))?
        .map_err(|e| anyhow::anyhow!("upload error: {e:?}"))?;

    Ok(result)
}

/// Extract file paths from message text.
///
/// Looks for lines or inline references to existing files. Returns the
/// found paths and a cleaned version of the text with the raw paths removed.
fn extract_file_paths(text: &str) -> (Vec<std::path::PathBuf>, String) {
    let mut paths = Vec::new();

    // Scan the entire text for substrings that look like file paths.
    // Match /absolute/paths and ~/home/paths, allowing them to be wrapped
    // in backticks, quotes, bold markers, or markdown list items.
    for word in split_path_candidates(text) {
        let expanded = if word.starts_with("~/") {
            if let Some(home) = std::env::var_os("HOME") {
                std::path::PathBuf::from(home).join(&word[2..])
            } else {
                continue;
            }
        } else {
            std::path::PathBuf::from(&word)
        };

        if expanded.is_absolute() && expanded.exists() && expanded.is_file() {
            paths.push(expanded);
        }
    }

    paths.sort();
    paths.dedup();

    (paths, text.to_string())
}

/// Extract candidate file paths from text. Finds substrings starting with
/// `/` or `~/` and ending at whitespace or common delimiters.
fn split_path_candidates(text: &str) -> Vec<String> {
    let mut results = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Look for path start: / or ~/
        let is_path_start = chars[i] == '/'
            || (chars[i] == '~' && i + 1 < len && chars[i + 1] == '/');

        if is_path_start {
            let start = i;
            // Consume until whitespace or end-of-path delimiter
            while i < len && !is_path_end(chars[i]) {
                i += 1;
            }
            let mut candidate: String = chars[start..i].iter().collect();
            // Strip trailing punctuation that's likely not part of the path
            while candidate.ends_with(|c: char| "`*\"')],;:".contains(c)) {
                candidate.pop();
            }
            if candidate.len() > 1 {
                results.push(candidate);
            }
        } else {
            i += 1;
        }
    }

    results
}

fn is_path_end(c: char) -> bool {
    c.is_whitespace() || c == '\n' || c == '\r'
}

/// Handle an outbound event from zeroclaw by routing it through presage.
///
/// The `recipient` from zeroclaw is the bridge sender_id (e.g. "signal").
/// We resolve it to the actual Signal address using `last_signal_sender`.
async fn handle_outbound_event(
    manager: &Arc<Mutex<Manager<SqliteStore, Registered>>>,
    event: BridgeOutbound,
    last_signal_sender: Option<&str>,
) -> anyhow::Result<()> {
    match event {
        BridgeOutbound::Message {
            recipient, content, ..
        } => {
            let effective = resolve_recipient(&recipient, last_signal_sender)?;
            tracing::info!("sending reply to Signal recipient: {effective}");
            send_signal_message(manager, &effective, content).await?;
        }
        BridgeOutbound::Typing {
            recipient, active, ..
        } => {
            if active {
                let effective = resolve_recipient(&recipient, last_signal_sender)
                    .unwrap_or_default();
                if let Ok(RecipientTarget::Direct(service_id)) = parse_recipient(&effective) {
                    let timestamp = now_millis();
                    let typing = ContentBody::TypingMessage(
                        presage::libsignal_service::proto::TypingMessage {
                            timestamp: Some(timestamp),
                            action: Some(
                                presage::libsignal_service::proto::typing_message::Action::Started
                                    .into(),
                            ),
                            ..Default::default()
                        },
                    );
                    let mut guard = manager.lock().await;
                    guard
                        .send_message(service_id, typing, timestamp)
                        .await
                        .map_err(|e| anyhow::anyhow!("typing failed: {e}"))?;
                }
            }
        }
        // Draft finalize = the complete response. Send it as a Signal message.
        BridgeOutbound::Draft { event, text, .. } => {
            if event == "finalize" {
                if let Some(text) = text {
                    let effective = resolve_recipient("signal", last_signal_sender)?;
                    tracing::info!("sending finalized draft to Signal recipient: {effective}");
                    send_signal_message(manager, &effective, text).await?;
                }
            }
        }
        // Reaction, Ack, Pong — no Signal-side action needed
        BridgeOutbound::Pong { .. } | BridgeOutbound::Ack { .. } => {}
        BridgeOutbound::Reaction { .. } => {}
        BridgeOutbound::Ready { .. } | BridgeOutbound::Error { .. } => {}
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

enum RecipientTarget {
    Direct(ServiceId),
    Group(Vec<u8>),
}

fn parse_recipient(recipient: &str) -> anyhow::Result<RecipientTarget> {
    if let Some(group_key_hex) = recipient.strip_prefix(GROUP_TARGET_PREFIX) {
        let bytes = hex_decode(group_key_hex)
            .map_err(|e| anyhow::anyhow!("bad group key hex: {e}"))?;
        if bytes.len() != GROUP_MASTER_KEY_LEN {
            anyhow::bail!(
                "group master key must be {GROUP_MASTER_KEY_LEN} bytes, got {}",
                bytes.len()
            );
        }
        return Ok(RecipientTarget::Group(bytes));
    }

    if let Ok(uuid) = Uuid::parse_str(recipient) {
        return Ok(RecipientTarget::Direct(ServiceId::Aci(uuid.into())));
    }

    anyhow::bail!("recipient must be a UUID or group:<hex_master_key>")
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err("odd-length hex string".to_string());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}
