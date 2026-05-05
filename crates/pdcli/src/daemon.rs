use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use proton_drive_sdk::cache::sqlite::SqliteCacheRepository;
use proton_sdk_rs2::{
    AppVersionConfiguration, cache::CacheRepository, client::ProtonClientOptions,
    session::ProtonAPISession,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::transfer::TransferTracker;
use crate::{credentials, db::FuseDb, fs};

const SOCKET_NAME: &str = "pdcli-daemon.sock";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonStatus {
    Online,
    Offline,
    Paused,
}

fn socket_path() -> anyhow::Result<PathBuf> {
    let config_dir = platform_dirs::AppDirs::new(Some("pdcli"), false)
        .ok_or_else(|| anyhow::anyhow!("failed to resolve config directory"))?
        .config_dir;
    std::fs::create_dir_all(&config_dir)?;
    Ok(config_dir.join(SOCKET_NAME))
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

pub fn ensure_running(force_offline: bool) -> anyhow::Result<()> {
    if is_running() {
        return Ok(());
    }

    let exe = std::env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command.arg("--daemon");
    if force_offline {
        command.arg("--force-offline");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    for _ in 0..40 {
        if is_running() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    anyhow::bail!("daemon did not become ready");
}

pub async fn run(force_offline: bool) -> anyhow::Result<()> {
    if is_running() {
        anyhow::bail!("pdcli daemon is already running");
    }

    let path = socket_path()?;
    if path.exists() {
        std::fs::remove_file(&path)?;
    }

    let listener = UnixListener::bind(&path)?;
    let session = restore_session(force_offline).await?;
    let fuse_session = fs::spawn_fuse_session(&session, TransferTracker::new(), force_offline)
        .ok_or_else(|| anyhow::anyhow!("failed to mount Proton Drive filesystem"))?;

    tracing::info!(socket = %path.display(), "pdcli daemon ready");

    loop {
        let (stream, _) = listener.accept().await?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            continue;
        }

        match line.trim() {
            "ping" => {
                reader.get_mut().write_all(b"ok\n").await?;
            }
            "status" => {
                reader
                    .get_mut()
                    .write_all(status_response().as_bytes())
                    .await?;
            }
            "toggle-pause" => {
                fs::toggle_sync_paused();
                reader
                    .get_mut()
                    .write_all(status_response().as_bytes())
                    .await?;
            }
            "retry-sync" => {
                fs::retry_sync_now();
                reader.get_mut().write_all(b"ok\n").await?;
            }
            "events" => {
                let events = load_recent_events();
                let body = serde_json::to_string(&events)?;
                reader.get_mut().write_all(body.as_bytes()).await?;
                reader.get_mut().write_all(b"\n").await?;
            }
            "quit" => {
                reader.get_mut().write_all(b"ok\n").await.ok();
                break;
            }
            other => {
                tracing::warn!(command = other, "unknown daemon command");
                reader.get_mut().write_all(b"error\n").await.ok();
            }
        }
    }

    drop(fuse_session);
    fs::force_unmount();
    std::fs::remove_file(&path).ok();
    tracing::info!("pdcli daemon stopped");
    Ok(())
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
