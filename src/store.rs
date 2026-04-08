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
/// Uses `OnNewIdentity::Reject` so that identity key changes (potential MITM)
/// are rejected rather than silently trusted.
pub async fn open_store(db_path: &Path) -> anyhow::Result<SqliteStore> {
    if let Some(parent) = db_path.parent() {
        create_dir_restricted(parent).await?;
    }
    // Prefix with sqlite: scheme so sqlx parses it as a file path correctly,
    // and append ?mode=rwc to create if missing.
    let url = format!(
        "sqlite:{}?mode=rwc",
        db_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("db path is not valid UTF-8"))?
    );
    let store = SqliteStore::open(&url, OnNewIdentity::Reject)
        .await
        .map_err(|e| anyhow::anyhow!("failed to open signal store: {e}"))?;
    Ok(store)
}

/// Create directories with mode 0700 (owner-only) to protect key material.
async fn create_dir_restricted(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path)?;
    }
    #[cfg(not(unix))]
    {
        tokio::fs::create_dir_all(path).await?;
    }
    Ok(())
}

fn dirs_fallback() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".zeroclaw")
    } else {
        PathBuf::from(".zeroclaw")
    }
}
