use crate::app::ProtonDrive;

mod app;
mod auth;
mod credentials;
mod daemon;
mod db;
mod flags;
mod fs;
mod thumbnail;
mod transfer;
mod tray;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pdcli=info".into()),
        )
        .init();

    let mut flags = flags::ClientFlags::default();
    flags.apply_flags();
    if flags.daemon {
        install_daemon_exit_hooks();
        if let Err(e) = daemon::run(flags.force_offline).await {
            tracing::error!(error = %e, "pdcli daemon failed");
            std::process::exit(1);
        }
        return;
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

fn install_daemon_exit_hooks() {
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
}

extern "C" fn handle_signal(sig: libc::c_int) {
    fs::force_unmount();
    // Re-raise with default handler so the process exits with the correct code.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}
