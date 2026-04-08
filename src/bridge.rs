//! Bridge adapter: connects presage (Signal) to ZeroClaw's bridge WebSocket channel.
//!
//! Speaks the bridge protocol defined in zeroclaw-labs/zeroclaw#2816:
//! - Connects to the bridge WS endpoint
//! - Authenticates with `{"type":"auth","token":"...","sender_id":"..."}`
//! - Forwards inbound Signal messages as `{"type":"message",...}`
//! - Receives outbound events from ZeroClaw and routes them through presage

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

    // Task: read from bridge WS, forward parsed events via mpsc
    let bridge_reader = tokio::spawn(async move {
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
            match serde_json::from_str::<BridgeOutbound>(&text) {
                Ok(event) => {
                    if bridge_event_tx.send(event).await.is_err() {
                        break;
                    }
                }
                Err(e) => tracing::debug!("ignoring unparseable bridge event: {e}"),
            }
        }
    });

    // Task: write to bridge WS (drains signal_rx mpsc)
    let bridge_writer = tokio::spawn(async move {
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
    let messages = {
        let mut guard = manager.lock().await;
        guard
            .receive_messages()
            .await
            .map_err(|e| anyhow::anyhow!("receive_messages failed: {e}"))?
    };
    futures_util::pin_mut!(messages);

    loop {
        tokio::select! {
            signal_item = messages.next() => {
                let Some(item) = signal_item else { break };
                match item {
                    Received::QueueEmpty => {
                        tracing::debug!("signal: initial queue drained");
                    }
                    Received::Contacts => {
                        tracing::debug!("signal: contacts synced");
                    }
                    Received::Content(content) => {
                        if let Some((bridge_msg, signal_reply_addr)) = content_to_bridge_message(
                            &content,
                            &config.allowed_from,
                            config.group_filter.as_deref(),
                            &config.sender_id,
                        ) {
                            last_signal_sender = Some(signal_reply_addr);
                            let payload = serde_json::to_string(&bridge_msg)?;
                            if signal_tx.send(payload).await.is_err() {
                                tracing::info!("bridge writer closed");
                                break;
                            }
                        }
                    }
                }
            }
            bridge_event = bridge_event_rx.recv() => {
                let Some(event) = bridge_event else {
                    tracing::info!("bridge event channel closed");
                    break;
                };
                tracing::info!("received bridge event: {event:?}");
                if let Err(e) = handle_outbound_event(&manager, event, last_signal_sender.as_deref()).await {
                    tracing::warn!("failed to handle bridge event: {e}");
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

    let text = body.body.as_deref().filter(|t| !t.is_empty())?;
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

    // Use the Signal UUID (or group key) as sender_id so zeroclaw's reply
    // is addressed to a value that parse_recipient can resolve.
    let reply_address = if let Some(ref gk) = group_master_key {
        format!("{GROUP_TARGET_PREFIX}{gk}")
    } else {
        from_uuid.clone()
    };

    Some((
        BridgeInbound::Message {
            id: format!("signative_{timestamp_ms}_{}", &from_uuid[..8]),
            sender_id: bridge_sender_id.to_string(),
            content: text.to_string(),
        },
        reply_address,
    ))
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
            // Try parsing the recipient directly (UUID or group key).
            // If that fails, fall back to the last known Signal sender.
            let effective_recipient = match parse_recipient(&recipient) {
                Ok(_) => recipient.clone(),
                Err(_) => last_signal_sender
                    .ok_or_else(|| anyhow::anyhow!(
                        "cannot route reply to '{recipient}': no Signal sender known yet"
                    ))?
                    .to_string(),
            };
            tracing::info!("sending reply to Signal recipient: {effective_recipient}");
            let target = parse_recipient(&effective_recipient)?;
            let timestamp = now_millis();
            let data_message = presage::libsignal_service::content::DataMessage {
                body: Some(content),
                timestamp: Some(timestamp),
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
        }
        BridgeOutbound::Typing {
            recipient, active, ..
        } => {
            if active {
                let effective = match parse_recipient(&recipient) {
                    Ok(_) => recipient.clone(),
                    Err(_) => last_signal_sender.unwrap_or_default().to_string(),
                };
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
        BridgeOutbound::Draft {
            event, text, ..
        } => {
            if event == "finalize" {
                if let Some(text) = text {
                    let effective_recipient = last_signal_sender
                        .ok_or_else(|| anyhow::anyhow!("draft finalize but no Signal sender known"))?
                        .to_string();
                    tracing::info!("sending finalized draft to Signal recipient: {effective_recipient}");
                    let target = parse_recipient(&effective_recipient)?;
                    let timestamp = now_millis();
                    let data_message = presage::libsignal_service::content::DataMessage {
                        body: Some(text),
                        timestamp: Some(timestamp),
                        ..Default::default()
                    };
                    let mut guard = manager.lock().await;
                    match target {
                        RecipientTarget::Direct(service_id) => {
                            guard.send_message(service_id, data_message, timestamp)
                                .await
                                .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;
                        }
                        RecipientTarget::Group(key_bytes) => {
                            guard.send_message_to_group(&key_bytes, data_message, timestamp)
                                .await
                                .map_err(|e| anyhow::anyhow!("group send failed: {e}"))?;
                        }
                    }
                }
            }
            // Ignore draft start/update — only finalize triggers a Signal send
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
