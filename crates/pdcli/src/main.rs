use crate::app::ProtonDrive;

mod auth;
mod credentials;
mod db;
mod app;
mod tray;
mod transfer;
mod flags;
mod fs;
mod thumbnail;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pdcli=info".into()),
        )
        .init();

    // Unmount FUSE on panic.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        fs::force_unmount();
        default_hook(info);
    }));

    // Unmount FUSE on SIGTERM / SIGINT / SIGHUP.
    unsafe {
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::signal(sig, handle_signal as *const () as libc::sighandler_t);
        }
    }

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

extern "C" fn handle_signal(sig: libc::c_int) {
    fs::force_unmount();
    // Re-raise with default handler so the process exits with the correct code.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}