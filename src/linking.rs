use std::path::Path;

use futures::future;
use presage::libsignal_service::configuration::SignalServers;
use presage::manager::Registered;
use presage::Manager;
use presage_store_sqlite::SqliteStore;
use url::Url;

use crate::store::open_store;

/// Link this device as a secondary device to an existing Signal account.
///
/// Prints a QR code to the terminal that must be scanned from the primary
/// device (Signal app -> Linked Devices -> Link New Device).
///
/// Returns a registered `Manager` ready for send/receive.
pub async fn link_as_secondary(
    db_path: &Path,
    device_name: &str,
) -> anyhow::Result<Manager<SqliteStore, Registered>> {
    let store = open_store(db_path).await?;

    let (provisioning_tx, provisioning_rx) = futures::channel::oneshot::channel::<Url>();

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

    let manager = manager?;
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
fn print_qr_to_terminal(url: &Url) {
    use qrcode::QrCode;

    let url_str = url.as_str();
    match QrCode::new(url_str.as_bytes()) {
        Ok(code) => {
            let string = code
                .render::<char>()
                .quiet_zone(true)
                .module_dimensions(2, 1)
                .build();
            eprintln!("\nScan this QR code with Signal (Linked Devices):\n");
            eprintln!("{string}");
            eprintln!("\nProvisioning URL: {url_str}\n");
        }
        Err(e) => {
            // Fallback: just print the URL so the user can paste it
            eprintln!("Could not render QR code: {e}");
            eprintln!("Provisioning URL: {url_str}");
        }
    }
}
