use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use serde::Serialize;

use crate::app::ProtonDrive;
use crate::flags::{Cli, Command, ServiceCommand, is_wsl};

mod app;
mod auth;
mod computers;
mod credentials;
mod daemon;
mod db;
mod flags;
mod fs;
mod pdignore;
mod quoted;
mod service;
mod share;
mod takeout;
mod thumbnail;
mod transfer;
mod tray;
mod version;

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
        if is_storage_quota_error(&e) {
            eprintln!(
                "Proton Drive is out of storage. Free space (including Trash) or upgrade your plan:"
            );
            eprintln!(
                "https://account.proton.me/drive/dashboard?plan=drive2022&target=compare&ref=upsell_drive_cli"
            );
        }
        std::process::exit(1);
    }
}

fn is_storage_quota_error(error: &anyhow::Error) -> bool {
    const CODES: [u32; 4] = [200001, 200002, 200100, 200101];
    error.chain().any(|cause| {
        cause
            .to_string()
            .split(|character: char| !character.is_ascii_digit())
            .filter_map(|part| part.parse().ok())
            .any(|code| CODES.contains(&code))
    })
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
        Some(Command::Status { json }) => cmd_status(json),
        Some(Command::Retry { id, all }) => cmd_retry(id, all),
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
        Some(Command::Share { command }) => share::run_cli(flags.force_offline, command).await,
        Some(Command::Takeout { destination }) => {
            takeout::run_cli(flags.force_offline, destination).await
        }
        Some(Command::Service { command }) => cmd_service(command),
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
        "pdcli (unofficial)",
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

fn cmd_status(json: bool) -> anyhow::Result<()> {
    let credential = credentials::load();
    let daemon = daemon::status();
    let mount = fs::default_mountpoint()?;
    let mounted = std::fs::read_to_string("/proc/mounts")
        .map(|s| s.contains("proton-drive"))
        .unwrap_or(false);

    let journal = if credential.is_some() {
        db::FuseDb::open_default()
            .ok()
            .map(|db| db.journal_summary(50))
    } else {
        None
    };

    if json {
        #[derive(Serialize)]
        struct StatusOutput {
            signed_in: bool,
            username: Option<String>,
            daemon: &'static str,
            mountpoint: String,
            mounted: bool,
            journal: Option<db::JournalSummary>,
        }
        let daemon = match daemon {
            Some(daemon::DaemonStatus::Online) => "online",
            Some(daemon::DaemonStatus::Offline) => "offline",
            Some(daemon::DaemonStatus::Paused) => "paused",
            None => "not running",
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&StatusOutput {
                signed_in: credential.is_some(),
                username: credential.map(|cred| cred.username().to_owned()),
                daemon,
                mountpoint: mount.display().to_string(),
                mounted,
                journal,
            })?
        );
        return Ok(());
    }

    match credential {
        Some(cred) => println!("signed in: {}", cred.username()),
        None => println!("signed in: no"),
    }
    println!(
        "daemon: {}",
        match daemon {
            Some(daemon::DaemonStatus::Online) => "online",
            Some(daemon::DaemonStatus::Offline) => "offline",
            Some(daemon::DaemonStatus::Paused) => "paused",
            None => "not running",
        }
    );
    println!(
        "mount: {} ({})",
        mount.display(),
        if mounted { "mounted" } else { "not mounted" }
    );
    if let Some(summary) = journal {
        println!(
            "journal: {} pending, {} failed",
            summary.pending, summary.failed
        );
        for entry in summary
            .entries
            .iter()
            .filter(|entry| entry.status == "failed")
        {
            println!(
                "  failed #{} {}{}",
                entry.id,
                entry.event_type,
                entry
                    .error
                    .as_deref()
                    .map(|error| format!(": {error}"))
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}

fn cmd_retry(id: Option<i64>, all: bool) -> anyhow::Result<()> {
    let db = db::FuseDb::open_default()
        .context("cannot open the local journal; sign in once before retrying")?;
    let entry_id = if all {
        None
    } else {
        Some(id.ok_or_else(|| anyhow::anyhow!("provide a failed journal ID or --all"))?)
    };
    let count = db.retry_failed(entry_id, all)?;
    if daemon::is_running() {
        daemon::request_retry_sync_now().ok();
    }
    if all {
        println!("retrying {count} failed journal entries");
    } else if let Some(entry_id) = entry_id {
        println!("retrying failed journal entry {entry_id}");
    }
    Ok(())
}

fn cmd_service(command: ServiceCommand) -> anyhow::Result<()> {
    match command {
        ServiceCommand::Install => {
            let path = service::install()?;
            service::reload()?;
            println!("installed {}", path.display());
        }
        ServiceCommand::Uninstall => {
            let _ = service::disable();
            service::uninstall()?;
            service::reload()?;
            println!("service removed");
        }
        ServiceCommand::Enable => {
            stop_daemon();
            let path = service::install()?;
            service::enable()?;
            println!("enabled {}", path.display());
        }
        ServiceCommand::Disable => {
            service::disable()?;
            println!("service disabled");
        }
        ServiceCommand::Reload => {
            service::reload()?;
            println!("systemd user units reloaded");
        }
        ServiceCommand::Status => println!("{}", service::status()?),
    }
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

#[cfg(test)]
mod tests {
    use super::is_storage_quota_error;

    #[test]
    fn detects_only_storage_quota_response_codes() {
        assert!(is_storage_quota_error(&anyhow::anyhow!(
            "API error 200002: Storage quota exceeded"
        )));
        assert!(!is_storage_quota_error(&anyhow::anyhow!(
            "API error 2500: upload failed"
        )));
    }
}
