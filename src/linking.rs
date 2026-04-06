use std::path::Path;
use std::time::Duration;

use futures::future;
use presage::libsignal_service::configuration::SignalServers;
use presage::manager::Registered;
use presage::Manager;
use presage_store_sqlite::SqliteStore;
use url::Url;

use crate::store::open_store;

/// Maximum time to wait for the user to scan the QR code before aborting.
const LINKING_TIMEOUT: Duration = Duration::from_secs(120);

/// Link this device as a secondary device to an existing Signal account.
///
/// Prints a QR code to the terminal that must be scanned from the primary
/// device (Signal app -> Linked Devices -> Link New Device).
///
/// Times out after 120 seconds if the QR code is not scanned.
///
/// Returns a registered `Manager` ready for send/receive.
pub async fn link_as_secondary(
    db_path: &Path,
    device_name: &str,
) -> anyhow::Result<Manager<SqliteStore, Registered>> {
    let store = open_store(db_path).await?;

    let (provisioning_tx, provisioning_rx) = futures::channel::oneshot::channel::<Url>();

    let link_future = async {
        let (manager, _) = future::join(
            async {
                Manager::link_secondary_device(
                    store,
                    SignalServers::Production,
                    device_name.to_string(),
                    provisioning_tx,
                )
                .await
                .map_err(|e| anyhow::anyhow!("linking failed: {e}"))
            },
            async {
                match provisioning_rx.await {
                    Ok(url) => print_qr_to_terminal(&url),
                    Err(e) => tracing::error!("provisioning channel closed: {e}"),
                }
            },
        )
        .await;
        manager
    };

    let manager = tokio::time::timeout(LINKING_TIMEOUT, link_future)
        .await
        .map_err(|_| anyhow::anyhow!("device linking timed out after {LINKING_TIMEOUT:?} — QR code was not scanned"))??;

    tracing::info!("successfully linked as secondary device");
    Ok(manager)
}

/// Load an already-linked manager from the store.
pub async fn load_registered(
    db_path: &Path,
) -> anyhow::Result<Manager<SqliteStore, Registered>> {
    let store = open_store(db_path).await?;
    Manager::load_registered(store)
        .await
        .map_err(|e| anyhow::anyhow!("failed to load registered manager: {e}"))
}

/// Render a provisioning URL as a QR code in the terminal.
///
/// Only the QR code is displayed — the raw URL is never printed to avoid
/// leaking the provisioning secret to terminal logs or process supervisors.
fn print_qr_to_terminal(url: &Url) {
    use qrcode::QrCode;

    match QrCode::new(url.as_str().as_bytes()) {
        Ok(code) => {
            let string = code
                .render::<char>()
                .quiet_zone(true)
                .module_dimensions(2, 1)
                .build();
            eprintln!("\nScan this QR code with Signal (Linked Devices):\n");
            eprintln!("{string}");
            eprintln!();
        }
        Err(e) => {
            eprintln!("Could not render QR code: {e}");
            eprintln!("Re-run this command in a terminal that supports UTF-8.");
        }
    }
}
