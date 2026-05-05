use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use proton_drive_sdk::cache::sqlite::SqliteCacheRepository;
use proton_sdk_rs2::{
    AppVersionConfiguration, cache::CacheRepository, client::ProtonClientOptions,
    session::ProtonAPISession,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

use crate::transfer::TransferTracker;
use crate::{credentials, db::FuseDb, fs, tray};

const SOCKET_NAME: &str = "pdcli-daemon.sock";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonStatus {
    Online,
    Offline,
    Paused,
}

fn socket_path() -> anyhow::Result<PathBuf> {
    for dir in socket_dir_candidates() {
        if std::fs::create_dir_all(&dir).is_ok() {
            return Ok(dir.join(SOCKET_NAME));
        }
    }

    anyhow::bail!("failed to resolve writable daemon socket directory")
}

fn socket_dir_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        candidates.push(PathBuf::from(runtime_dir).join("pdcli"));
    }

    candidates.push(std::env::temp_dir().join(format!("pdcli-{}", unsafe { libc::geteuid() })));

    candidates
}

fn request(command: &str) -> anyhow::Result<String> {
    let mut stream = UnixStream::connect(socket_path()?)?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    stream.write_all(command.as_bytes())?;
    stream.write_all(b"\n")?;

    let mut buf = [0_u8; 64];
    let n = stream.read(&mut buf)?;
    Ok(std::str::from_utf8(&buf[..n])
        .unwrap_or_default()
        .trim()
        .to_string())
}

pub fn is_running() -> bool {
    request("ping").is_ok_and(|response| response == "ok")
}

pub fn status() -> Option<DaemonStatus> {
    match request("status").ok()?.as_str() {
        "online" => Some(DaemonStatus::Online),
        "offline" => Some(DaemonStatus::Offline),
        "paused" => Some(DaemonStatus::Paused),
        _ => None,
    }
}

pub fn request_toggle_pause() -> anyhow::Result<DaemonStatus> {
    match request("toggle-pause")?.as_str() {
        "paused" => Ok(DaemonStatus::Paused),
        "online" => Ok(DaemonStatus::Online),
        "offline" => Ok(DaemonStatus::Offline),
        response => anyhow::bail!("unexpected daemon response: {response}"),
    }
}

pub fn request_retry_sync_now() -> anyhow::Result<()> {
    anyhow::ensure!(request("retry-sync")? == "ok", "retry request failed");
    Ok(())
}

pub fn recent_events() -> Vec<crate::db::SyncEvent> {
    request("events")
        .ok()
        .and_then(|response| serde_json::from_str(&response).ok())
        .unwrap_or_default()
}

pub fn request_quit() -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(socket_path()?)?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    stream.write_all(b"quit\n")?;
    Ok(())
}

pub fn ensure_running(force_offline: bool, enable_tray: bool) -> anyhow::Result<()> {
    if is_running() {
        return Ok(());
    }

    let exe = std::env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command.arg("--daemon");
    if force_offline {
        command.arg("--force-offline");
    }
    if !enable_tray {
        command.arg("--no-tray");
    }
    let mut child = command.stdin(Stdio::null()).spawn()?;

    for _ in 0..600 {
        if is_running() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("daemon exited before becoming ready: {status}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    anyhow::bail!("daemon did not become ready");
}

pub async fn run(force_offline: bool, enable_tray: bool) -> anyhow::Result<()> {
    if is_running() {
        anyhow::bail!("pdcli daemon is already running");
    }

    let tray_handle = if enable_tray {
        let icon_path = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/icon.png"));
        let create_tray = tray::init(&icon_path);
        create_tray()
    } else {
        None
    };
    let mut tray_actions = if enable_tray {
        Some(spawn_tray_action_forwarder())
    } else {
        None
    };

    let path = socket_path().context("failed to resolve daemon socket path")?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| {
            format!("failed to remove stale daemon socket at {}", path.display())
        })?;
    }

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to bind daemon socket at {}", path.display()))?;
    let session = restore_session(force_offline)
        .await
        .context("failed to restore daemon session")?;
    let fuse_session = fs::spawn_fuse_session(&session, TransferTracker::new(), force_offline)
        .await
        .ok_or_else(|| anyhow::anyhow!("failed to mount Proton Drive filesystem"))?;

    tracing::info!(socket = %path.display(), "pdcli daemon ready");

    let mut tray_update_interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (stream, _) = accept_result?;
                if handle_client_command(stream).await? {
                    break;
                }
            }
            action = async {
                match &mut tray_actions {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(action) = action {
                    if handle_tray_action(action) {
                        break;
                    }
                }
            }
            _ = tray_update_interval.tick(), if enable_tray => {
                tray::update_state(tray_handle.as_ref(), daemon_tray_state());
            }
        }
    }

    drop(fuse_session);
    fs::force_unmount();
    std::fs::remove_file(&path).ok();
    tracing::info!("pdcli daemon stopped");
    Ok(())
}

async fn handle_client_command(stream: tokio::net::UnixStream) -> anyhow::Result<bool> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(false);
    }

    match line.trim() {
        "ping" => {
            write_client_response(reader.get_mut(), b"ok\n").await;
        }
        "status" => {
            write_client_response(reader.get_mut(), status_response().as_bytes()).await;
        }
        "toggle-pause" => {
            fs::toggle_sync_paused();
            write_client_response(reader.get_mut(), status_response().as_bytes()).await;
        }
        "retry-sync" => {
            fs::retry_sync_now();
            write_client_response(reader.get_mut(), b"ok\n").await;
        }
        "events" => {
            let events = load_recent_events();
            let body = serde_json::to_string(&events)?;
            write_client_response(reader.get_mut(), body.as_bytes()).await;
            write_client_response(reader.get_mut(), b"\n").await;
        }
        "quit" => {
            reader.get_mut().write_all(b"ok\n").await.ok();
            return Ok(true);
        }
        other => {
            tracing::warn!(command = other, "unknown daemon command");
            reader.get_mut().write_all(b"error\n").await.ok();
        }
    }

    Ok(false)
}

async fn write_client_response(stream: &mut tokio::net::UnixStream, body: &[u8]) {
    if let Err(e) = stream.write_all(body).await {
        tracing::debug!(error = %e, "failed to write daemon client response");
    }
}

fn spawn_tray_action_forwarder() -> mpsc::UnboundedReceiver<tray::TrayAction> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        loop {
            for action in tray::poll_events() {
                if tx.send(action).is_err() {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    rx
}

fn daemon_tray_state() -> tray::TrayState {
    if fs::is_sync_paused() {
        tray::TrayState::Paused
    } else if fs::is_online() {
        tray::TrayState::Online
    } else {
        tray::TrayState::Offline
    }
}

fn handle_tray_action(action: tray::TrayAction) -> bool {
    match action {
        tray::TrayAction::OpenFolder => {
            if let Some(path) = dirs::home_dir().map(|h| h.join("ProtonDrive").join("MyFiles")) {
                #[cfg(target_os = "macos")]
                let opener = "open";
                #[cfg(not(target_os = "macos"))]
                let opener = "xdg-open";
                let _ = std::process::Command::new(opener).arg(path).spawn();
            }
            false
        }
        tray::TrayAction::ShowHideWindow => {
            launch_gui(None);
            false
        }
        tray::TrayAction::ToggleSyncPause => {
            let paused = fs::toggle_sync_paused();
            tracing::info!(paused, "sync pause toggled from tray");
            false
        }
        tray::TrayAction::RetrySyncNow => {
            fs::retry_sync_now();
            tracing::info!("manual sync retry requested from tray");
            false
        }
        tray::TrayAction::Account => {
            launch_gui(Some("account"));
            false
        }
        tray::TrayAction::Settings => {
            launch_gui(Some("settings"));
            false
        }
        tray::TrayAction::SignOut => {
            credentials::remove();
            true
        }
        tray::TrayAction::Quit => true,
    }
}

fn launch_gui(page: Option<&str>) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut command = std::process::Command::new(exe);
    if let Some(page) = page {
        command.arg("--page").arg(page);
    }
    let _ = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn status_response() -> &'static str {
    if fs::is_sync_paused() {
        "paused\n"
    } else if fs::is_online() {
        "online\n"
    } else {
        "offline\n"
    }
}

async fn restore_session(force_offline: bool) -> anyhow::Result<ProtonAPISession> {
    let cred =
        credentials::load().ok_or_else(|| anyhow::anyhow!("no stored credentials available"))?;

    let config_dir = platform_dirs::AppDirs::new(Some("pdcli"), false)
        .ok_or_else(|| anyhow::anyhow!("failed to resolve config directory"))?
        .config_dir;
    std::fs::create_dir_all(&config_dir)?;
    let cache_db_path = config_dir.join("cache.db");

    let entity_cache: std::sync::Arc<dyn CacheRepository> = std::sync::Arc::new(
        SqliteCacheRepository::open_file(&cache_db_path, Some(10_000))?,
    );
    let secret_cache: std::sync::Arc<dyn CacheRepository> = std::sync::Arc::new(
        SqliteCacheRepository::open_file(&cache_db_path, Some(5_000))?,
    );

    let mut session = ProtonAPISession::from_stored_credentials(
        cred,
        AppVersionConfiguration::new("pdcli", 0, 1, 0),
        ProtonClientOptions {
            entity_cache_repository: Some(entity_cache),
            secret_cache_repository: Some(secret_cache),
            ..Default::default()
        },
    );

    if !force_offline {
        session.ensure_authenticated().await?;
    }

    if let Ok(cred) = session.to_stored_credentials_with_latest_tokens().await {
        credentials::save(&cred).ok();
    }
    credentials::save_session_tokens_on_refresh(&session);

    Ok(session)
}

fn load_recent_events() -> Vec<crate::db::SyncEvent> {
    let config_dir = match platform_dirs::AppDirs::new(Some("pdcli"), false) {
        Some(app_dirs) => app_dirs.config_dir,
        None => return Vec::new(),
    };
    let db_path = config_dir.join("fuse.db");
    FuseDb::open(&db_path)
        .map(|db| db.recent_sync_events(20))
        .unwrap_or_default()
}
