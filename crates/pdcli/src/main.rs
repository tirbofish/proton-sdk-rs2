use std::time::{Duration, Instant};

use clap::Parser;

use crate::app::ProtonDrive;
use crate::flags::{Cli, Command, is_wsl};

mod app;
mod auth;
mod computers;
mod credentials;
mod daemon;
mod db;
mod flags;
mod fs;
mod pdignore;
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

    let cli = Cli::parse();
    if let Err(e) = dispatch(cli).await {
        tracing::error!(error = %e, "pdcli failed");
        std::process::exit(1);
    }
}

async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    let flags = cli.client_flags(None);
    match cli.command {
        Some(Command::Gui { page }) => {
            let mut flags = flags.clone();
            flags.page = page;
            run_gui(flags)
        }
        Some(Command::Login) => cmd_login().await,
        Some(Command::Logout) => cmd_logout(),
        Some(Command::Status) => cmd_status(),
        Some(Command::Mount) => cmd_mount(flags.force_offline, flags.no_tray).await,
        Some(Command::Stop) => cmd_stop(),
        Some(Command::Pause) => cmd_pause(true),
        Some(Command::Resume) => cmd_pause(false),
        Some(Command::Sync) => {
            daemon::request_retry_sync_now()?;
            println!("sync retry requested");
            Ok(())
        }
        Some(Command::Open) => {
            daemon::open_folder();
            Ok(())
        }
        Some(Command::Computers { command }) => {
            computers::run_cli(flags.force_offline, command).await
        }
        Some(Command::Daemon) => run_daemon(flags.force_offline, !flags.no_tray).await,
        None if cli.daemon => run_daemon(flags.force_offline, !flags.no_tray).await,
        None if cli.cli => cmd_mount(flags.force_offline, flags.no_tray).await,
        None if cli.gui || !is_wsl() => run_gui(flags),
        None => cmd_mount(flags.force_offline, flags.no_tray).await,
    }
}

fn run_gui(flags: flags::ClientFlags) -> anyhow::Result<()> {
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Proton Drive",
        native_options,
        Box::new(move |_| Ok(Box::new(ProtonDrive::new(flags)))),
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))
}

async fn run_daemon(force_offline: bool, enable_tray: bool) -> anyhow::Result<()> {
    install_daemon_exit_hooks();
    daemon::run(force_offline, enable_tray).await
}

async fn cmd_login() -> anyhow::Result<()> {
    if let Some(cred) = credentials::load() {
        println!("already signed in as {}", cred.username());
        println!("run `pdcli logout` first to switch accounts");
        return Ok(());
    }
    let session = auth::login_cli().await?;
    println!("signed in as {}", session.username);
    Ok(())
}

async fn cmd_mount(force_offline: bool, no_tray: bool) -> anyhow::Result<()> {
    if credentials::load().is_none() {
        auth::login_cli().await?;
    }
    daemon::ensure_running(force_offline, !no_tray)?;
    println!("mounted at {}", fs::default_mountpoint()?.display());
    Ok(())
}

fn cmd_stop() -> anyhow::Result<()> {
    stop_daemon();
    println!("stopped");
    Ok(())
}

fn cmd_logout() -> anyhow::Result<()> {
    stop_daemon();
    credentials::remove();
    println!("signed out");
    Ok(())
}

fn stop_daemon() {
    if daemon::is_running() {
        let _ = daemon::request_quit();
        let started = Instant::now();
        while daemon::is_running() && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    if let Ok(path) = fs::default_mountpoint() {
        fs::unmount_path(&path);
    }
}

fn cmd_pause(pause: bool) -> anyhow::Result<()> {
    if pause {
        daemon::request_pause()?;
        println!("sync paused");
    } else {
        daemon::request_resume()?;
        println!("sync resumed");
    }
    Ok(())
}

fn cmd_status() -> anyhow::Result<()> {
    match credentials::load() {
        Some(cred) => println!("signed in: {}", cred.username()),
        None => println!("signed in: no"),
    }

    match daemon::status() {
        Some(daemon::DaemonStatus::Online) => println!("daemon: online"),
        Some(daemon::DaemonStatus::Offline) => println!("daemon: offline"),
        Some(daemon::DaemonStatus::Paused) => println!("daemon: paused"),
        None => println!("daemon: not running"),
    }

    let mount = fs::default_mountpoint()?;
    let mounted = std::fs::read_to_string("/proc/mounts")
        .map(|s| s.contains("proton-drive"))
        .unwrap_or(false);
    println!(
        "mount: {} ({})",
        mount.display(),
        if mounted { "mounted" } else { "not mounted" }
    );
    Ok(())
}

fn install_daemon_exit_hooks() {
    // Unmount FUSE on a panic in this thread. Do not unmount when a helper
    // thread panics (GTK tray init on WSL), or the mount is torn down while
    // the daemon keeps running.
    let main_id = std::thread::current().id();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == main_id {
            fs::force_unmount();
        }
        default_hook(info);
    }));

    unsafe {
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::signal(sig, handle_signal as *const () as libc::sighandler_t);
        }
    }
}

extern "C" fn handle_signal(sig: libc::c_int) {
    fs::force_unmount();
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}
