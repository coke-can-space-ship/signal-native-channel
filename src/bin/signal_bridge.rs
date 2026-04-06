//! Bridge adapter: runs as a sidecar process connecting Signal (via presage)
//! to ZeroClaw's bridge WebSocket channel.
//!
//! Usage:
//!   signal-bridge [options]
//!
//! Options:
//!   --bridge-url <url>       Bridge WS endpoint (default: ws://127.0.0.1:8765/ws)
//!   --auth-token <token>     Shared auth token (required)
//!   --sender-id <id>         Sender identity (default: signal)
//!   --db-path <path>         Presage SQLite DB (default: ~/.zeroclaw/signal-native.db)
//!   --allowed-from <list>    Comma-separated allowed sender UUIDs, or * (default: *)
//!   --group-filter <filter>  Group filter: "dm", or hex group master key
//!
//! Example:
//!   signal-bridge --auth-token my-secret --sender-id signal --allowed-from '*'

use std::path::PathBuf;

use signal_native_channel::bridge::{self, BridgeAdapterConfig};
use signal_native_channel::store::default_db_path;

fn get_arg(flag: &str) -> Option<String> {
    std::env::args()
        .position(|a| a == flag)
        .and_then(|i| std::env::args().nth(i + 1))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let auth_token = get_arg("--auth-token")
        .ok_or_else(|| anyhow::anyhow!("--auth-token is required"))?;

    let config = BridgeAdapterConfig {
        bridge_url: get_arg("--bridge-url")
            .unwrap_or_else(|| "ws://127.0.0.1:8765/ws".to_string()),
        auth_token,
        sender_id: get_arg("--sender-id").unwrap_or_else(|| "signal".to_string()),
        db_path: get_arg("--db-path")
            .map(PathBuf::from)
            .unwrap_or_else(default_db_path),
        allowed_from: get_arg("--allowed-from")
            .map(|s| s.split(',').map(|s| s.trim().to_string()).collect())
            .unwrap_or_else(|| vec!["*".to_string()]),
        group_filter: get_arg("--group-filter"),
    };

    eprintln!("signal-bridge starting");
    eprintln!("  bridge: {}", config.bridge_url);
    eprintln!("  sender: {}", config.sender_id);
    eprintln!("  db:     {}", config.db_path.display());

    bridge::run(config).await
}
