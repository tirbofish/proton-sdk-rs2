use crate::app::ProtonDrive;

mod auth;
mod credentials;
mod db;
mod app;
mod tray;
mod transfer;
mod flags;
mod fs;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pdcli=info".into()),
        )
        .init();

    let native_options = eframe::NativeOptions::default();

    let icon_path = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/icon.png"));
    let create_tray = tray::init(&icon_path);

    eframe::run_native(
        "Proton Drive",
        native_options,
        Box::new(move |_| {
            let tray_handle = create_tray();
            Ok(Box::new({
                ProtonDrive::new().with_tray(tray_handle).handle_flags()
            }))
        }),
    )
    .unwrap();
}