use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use presage::libsignal_service::content::ContentBody;
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::manager::Registered;
use presage::model::messages::Received;
use presage::Manager;
use presage_store_sqlite::SqliteStore;
use tokio::sync::{mpsc, Mutex};

use crate::error::SignalNativeError;
use crate::linking;

const CHANNEL_NAME: &str = "signal-native";
const GROUP_TARGET_PREFIX: &str = "group:";
const GROUP_MASTER_KEY_LEN: usize = 32;

// ---------------------------------------------------------------------------
// Zeroclaw Channel trait types (mirrored from zeroclaw::channels::traits)
//
// When integrating into the zeroclaw tree, replace these with the real imports:
//   use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
//   use crate::channels::media_pipeline::MediaAttachment;
// ---------------------------------------------------------------------------

/// Incoming message from a channel.
#[derive(Debug, Clone)]
pub struct ChannelMessage {
    pub id: String,
    pub sender: String,
    pub reply_target: String,
    pub content: String,
    pub channel: String,
    pub timestamp: u64,
    pub thread_ts: Option<String>,
    pub interruption_scope_id: Option<String>,
    pub attachments: Vec<MediaAttachment>,
}

/// Outgoing message to a channel.
#[derive(Debug, Clone)]
pub struct SendMessage {
    pub content: String,
    pub recipient: String,
    pub subject: Option<String>,
    pub thread_ts: Option<String>,
    pub attachments: Vec<MediaAttachment>,
}

/// Stub for media attachments — replace with the real type on integration.
#[derive(Debug, Clone)]
pub struct MediaAttachment {
    pub file_name: String,
    pub data: Vec<u8>,
    pub mime_type: Option<String>,
}

// ---------------------------------------------------------------------------
// Channel implementation
// ---------------------------------------------------------------------------

/// Native Signal channel backed by presage (libsignal).
///
/// Communicates directly with Signal servers over WebSocket — no signal-cli
/// or REST API intermediary.
pub struct SignalNativeChannel {
    /// Path to the SQLite database that holds Signal protocol state.
    db_path: PathBuf,
    /// Phone numbers (E.164) or `"*"` for wildcard allowed senders.
    allowed_from: Vec<String>,
    /// If set, only accept messages from this group. `"dm"` = DMs only.
    group_filter: Option<String>,
    /// Lazily initialised, shared manager. Protected by a mutex because
    /// `Manager::receive_messages` takes `&mut self`.
    manager: Arc<Mutex<Option<Manager<SqliteStore, Registered>>>>,
}

impl SignalNativeChannel {
    pub fn new(
        db_path: PathBuf,
        allowed_from: Vec<String>,
        group_filter: Option<String>,
    ) -> Self {
        Self {
            db_path,
            allowed_from,
            group_filter,
            manager: Arc::new(Mutex::new(None)),
        }
    }

    /// Load an already-linked manager from the store.
    ///
    /// This will NOT initiate device linking. If the device has not been linked
    /// (via the `link` binary), this returns an error.
    async fn ensure_manager(&self) -> anyhow::Result<()> {
        let mut guard = self.manager.lock().await;
        if guard.is_some() {
            return Ok(());
        }

        let mgr = linking::load_registered(&self.db_path).await.map_err(|e| {
            anyhow::anyhow!(
                "signal-native: no linked device found at {}. \
                 Run the `link` binary first to link this device. \
                 Original error: {e}",
                self.db_path.display()
            )
        })?;
        tracing::info!("signal-native: loaded existing linked device");
        *guard = Some(mgr);
        Ok(())
    }

    fn is_sender_allowed(&self, sender_uuid: &str) -> bool {
        if self.allowed_from.iter().any(|s| s == "*") {
            return true;
        }
        self.allowed_from.iter().any(|s| s == sender_uuid)
    }

    /// Parse a recipient string into a ServiceId (ACI UUID) or group master key bytes.
    fn parse_recipient(recipient: &str) -> Result<RecipientTarget, SignalNativeError> {
        if let Some(group_key_hex) = recipient.strip_prefix(GROUP_TARGET_PREFIX) {
            let bytes = hex::decode(group_key_hex).map_err(|e| {
                SignalNativeError::InvalidRecipient(format!("bad group key hex: {e}"))
            })?;
            if bytes.len() != GROUP_MASTER_KEY_LEN {
                return Err(SignalNativeError::InvalidRecipient(format!(
                    "group master key must be {GROUP_MASTER_KEY_LEN} bytes, got {}",
                    bytes.len()
                )));
            }
            return Ok(RecipientTarget::Group(bytes));
        }

        // Try UUID (Signal's preferred addressing).
        if let Ok(uuid) = Uuid::parse_str(recipient) {
            return Ok(RecipientTarget::Direct(ServiceId::Aci(uuid.into())));
        }

        // E.164 phone numbers can't be directly resolved to a ServiceId without
        // a contact discovery lookup. For now, require UUID addressing.
        // TODO: implement CDSI contact discovery for phone number -> UUID resolution.
        Err(SignalNativeError::InvalidRecipient(
            "recipient must be a UUID or group:<hex_master_key>".to_string(),
        ))
    }

    fn now_millis() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

enum RecipientTarget {
    Direct(ServiceId),
    Group(Vec<u8>),
}

// ---------------------------------------------------------------------------
// Channel trait implementation
//
// This mirrors zeroclaw's Channel trait. When integrating into the tree,
// add `#[async_trait]` and `impl Channel for SignalNativeChannel`.
// ---------------------------------------------------------------------------

impl SignalNativeChannel {
    pub fn name(&self) -> &str {
        CHANNEL_NAME
    }

    /// Send a message to a recipient (UUID) or group (group:<hex_master_key>).
    pub async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        self.ensure_manager().await?;
        let mut guard = self.manager.lock().await;
        let mgr = guard.as_mut().ok_or(SignalNativeError::NotLinked)?;

        let timestamp = Self::now_millis();
        let data_message = presage::libsignal_service::content::DataMessage {
            body: Some(message.content.clone()),
            timestamp: Some(timestamp),
            ..Default::default()
        };

        match Self::parse_recipient(&message.recipient)? {
            RecipientTarget::Direct(service_id) => {
                mgr.send_message(service_id, data_message, timestamp)
                    .await
                    .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;
            }
            RecipientTarget::Group(master_key_bytes) => {
                mgr.send_message_to_group(&master_key_bytes, data_message, timestamp)
                    .await
                    .map_err(|e| anyhow::anyhow!("group send failed: {e}"))?;
            }
        }

        Ok(())
    }

    /// Listen for incoming messages. Long-running — feeds messages into `tx`.
    pub async fn listen(&self, tx: mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        self.ensure_manager().await?;

        let mut retry_delay = tokio::time::Duration::from_secs(2);
        let max_delay = tokio::time::Duration::from_secs(60);

        loop {
            let receive_result = {
                let mut guard = self.manager.lock().await;
                let mgr = guard.as_mut().ok_or(SignalNativeError::NotLinked)?;
                mgr.receive_messages().await
            };

            let messages = match receive_result {
                Ok(stream) => {
                    retry_delay = tokio::time::Duration::from_secs(2);
                    stream
                }
                Err(e) => {
                    tracing::warn!(
                        "signal-native: receive_messages error: {e}, retrying in {retry_delay:?}"
                    );
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = (retry_delay * 2).min(max_delay);
                    continue;
                }
            };

            futures_util::pin_mut!(messages);

            while let Some(item) = messages.next().await {
                match item {
                    Received::QueueEmpty => {
                        tracing::debug!("signal-native: initial message queue drained");
                    }
                    Received::Contacts => {
                        tracing::debug!("signal-native: contacts synced from primary");
                    }
                    Received::Content(content) => {
                        if let Some(msg) = self.process_content(&content) {
                            if tx.send(msg).await.is_err() {
                                tracing::info!("signal-native: receiver dropped, exiting");
                                return Ok(());
                            }
                        }
                    }
                }
            }

            tracing::debug!("signal-native: message stream ended, reconnecting...");
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        }
    }

    /// Health check — verifies the manager is loaded and can reach Signal servers.
    pub async fn health_check(&self) -> bool {
        let guard = self.manager.lock().await;
        guard.is_some()
    }

    /// Send a typing indicator.
    pub async fn start_typing(&self, recipient: &str) -> anyhow::Result<()> {
        self.ensure_manager().await?;
        let mut guard = self.manager.lock().await;
        let mgr = guard.as_mut().ok_or(SignalNativeError::NotLinked)?;

        let timestamp = Self::now_millis();

        if let Ok(RecipientTarget::Direct(service_id)) = Self::parse_recipient(recipient) {
            let typing = presage::libsignal_service::content::ContentBody::TypingMessage(
                presage::libsignal_service::proto::TypingMessage {
                    timestamp: Some(timestamp),
                    action: Some(
                        presage::libsignal_service::proto::typing_message::Action::Started.into(),
                    ),
                    ..Default::default()
                },
            );
            mgr.send_message(service_id, typing, timestamp)
                .await
                .map_err(|e| anyhow::anyhow!("typing indicator failed: {e}"))?;
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Extract a ChannelMessage from a decrypted Content, applying filters.
    ///
    /// Sync messages (messages sent from the primary device) are skipped to
    /// avoid the bot responding to its own outbound messages.
    fn process_content(
        &self,
        content: &presage::libsignal_service::content::Content,
    ) -> Option<ChannelMessage> {
        let body = match &content.body {
            ContentBody::DataMessage(dm) => dm,
            // Skip sync messages — these are our own outbound messages echoed
            // back from the primary device. Processing them would cause loops.
            ContentBody::SynchronizeMessage(_) => return None,
            _ => return None,
        };

        let text = body.body.as_deref().filter(|t| !t.is_empty())?;

        // Sender identification
        let sender_uuid = content.metadata.sender.raw_uuid().to_string();

        if !self.is_sender_allowed(&sender_uuid) {
            return None;
        }

        // Group filtering
        let group_master_key = body
            .group_v2
            .as_ref()
            .and_then(|g| g.master_key.as_deref())
            .map(hex::encode);

        let reply_target = if let Some(ref gk) = group_master_key {
            format!("{GROUP_TARGET_PREFIX}{gk}")
        } else {
            sender_uuid.clone()
        };

        if let Some(ref filter) = self.group_filter {
            if filter.eq_ignore_ascii_case("dm") {
                // DMs only — skip group messages
                if group_master_key.is_some() {
                    return None;
                }
            } else {
                // Specific group — skip non-matching
                match &group_master_key {
                    Some(gk) if gk == filter => {}
                    _ => return None,
                }
            }
        }

        let timestamp_ms = content.metadata.timestamp;
        let timestamp_secs = timestamp_ms / 1000;

        Some(ChannelMessage {
            id: format!(
                "signative_{timestamp_ms}_{sender}",
                sender = &sender_uuid[..8]
            ),
            sender: sender_uuid,
            reply_target,
            content: text.to_string(),
            channel: CHANNEL_NAME.to_string(),
            timestamp: timestamp_secs,
            thread_ts: None,
            interruption_scope_id: None,
            attachments: vec![],
        })
    }
}

// ---------------------------------------------------------------------------
// Hex encoding helper (lightweight, avoids extra dep)
// ---------------------------------------------------------------------------

mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn decode(s: &str) -> Result<Vec<u8>, String> {
        if s.len() % 2 != 0 {
            return Err("odd-length hex string".to_string());
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
            .collect()
    }
}
