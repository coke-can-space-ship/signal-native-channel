//! One-shot binary to link this device as a secondary Signal device.
//!
//! Usage:
//!   cargo run --bin link -- [--db-path <path>] [--device-name <name>]
//!
//! Prints a QR code to the terminal. Scan it from Signal on your phone
//! (Settings -> Linked Devices -> Link New Device).

use std::path::PathBuf;

use signal_native_channel::store::default_db_path;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let db_path = std::env::args()
        .position(|a| a == "--db-path")
        .and_then(|i| std::env::args().nth(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(default_db_path);

    let device_name = std::env::args()
        .position(|a| a == "--device-name")
        .and_then(|i| std::env::args().nth(i + 1))
        .unwrap_or_else(|| "ZeroClaw".to_string());

    eprintln!("Linking device as \"{device_name}\"...");
    eprintln!("Database: {}", db_path.display());

    let _manager =
        signal_native_channel::linking::link_as_secondary(&db_path, &device_name).await?;

    eprintln!("Device linked successfully. You can now use the signal-native channel.");
    Ok(())
}
