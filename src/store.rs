use std::path::{Path, PathBuf};

use presage::model::identity::OnNewIdentity;
use presage_store_sqlite::SqliteStore;

/// Default database path under the zeroclaw config directory.
pub fn default_db_path() -> PathBuf {
    let base = dirs_fallback();
    base.join("signal-native.db")
}

/// Open (or create) the SQLite store at the given path.
///
/// Uses `OnNewIdentity::Trust` so that new contacts are accepted without
/// manual approval — appropriate for a bot/assistant use case.
pub async fn open_store(db_path: &Path) -> anyhow::Result<SqliteStore> {
    if let Some(parent) = db_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let store = SqliteStore::open(
        db_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("db path is not valid UTF-8"))?,
        OnNewIdentity::Trust,
    )
    .await
    .map_err(|e| anyhow::anyhow!("failed to open signal store: {e}"))?;
    Ok(store)
}

fn dirs_fallback() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".zeroclaw")
    } else {
        PathBuf::from(".zeroclaw")
    }
}
