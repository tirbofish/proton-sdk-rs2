use std::io::{IsTerminal, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use proton_drive_sdk::proton_sdk_rs2::{client::ProtonClientOptions, session::ProtonAPISession};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

use crate::transfer::TransferTracker;
use crate::{credentials, db::FuseDb, fs, tray, version::app_version_configuration};

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

    let mut buf = Vec::new();
    stream.take(1_048_577).read_to_end(&mut buf)?;
    anyhow::ensure!(buf.len() <= 1_048_576, "daemon response too large");
    Ok(String::from_utf8(buf)?.trim().to_string())
}

pub fn is_running() -> bool {
    request("ping").is_ok_and(|response| response == "ok")
}

pub fn active_transfers() -> anyhow::Result<Vec<crate::transfer::TransferEntry>> {
    Ok(serde_json::from_str(&request("transfers")?)?)
}

pub fn cancel_transfer(id: usize) -> anyhow::Result<()> {
    let response = request(&format!("cancel-transfer {id}"))?;
    anyhow::ensure!(response == "ok", "{response}");
    Ok(())
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

pub fn request_pause() -> anyhow::Result<()> {
    set_paused(true)
}

pub fn request_resume() -> anyhow::Result<()> {
    set_paused(false)
}

fn set_paused(want_paused: bool) -> anyhow::Result<()> {
    let status = status().ok_or_else(|| anyhow::anyhow!("pdcli daemon is not running"))?;
    let is_paused = matches!(status, DaemonStatus::Paused);
    if is_paused != want_paused {
        request_toggle_pause()?;
    }
    Ok(())
}

pub fn open_folder() -> anyhow::Result<()> {
    anyhow::ensure!(
        is_running(),
        "Proton Drive is not mounted; mount it from Status first"
    );
    let path = fs::default_mountpoint()?.join("MyFiles");
    std::fs::metadata(&path)
        .with_context(|| format!("cannot access mounted folder {}", path.display()))?;
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(not(target_os = "macos"))]
    let opener = "xdg-open";
    let output = std::process::Command::new(opener).arg(&path).output()?;
    anyhow::ensure!(
        output.status.success(),
        "could not open {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
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
        // The daemon may have been started without a tray (the systemd unit
        // uses `--no-tray`); show one attached to it. It exits if a tray exists.
        if enable_tray {
            std::process::Command::new(std::env::current_exe()?)
                .arg("tray")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
        }
        return Ok(());
    }

    let exe = std::env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(if std::io::stderr().is_terminal() {
            Stdio::inherit()
        } else {
            Stdio::null()
        });
    if force_offline {
        command.arg("--force-offline");
    }
    if !enable_tray {
        command.arg("--no-tray");
    }
    let mut child = command.spawn()?;

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

    // An attached `pdcli tray` from an earlier daemon may still own the icon.
    let tray_lock = enable_tray.then(lock_tray).flatten();
    let enable_tray = tray_lock.is_some();
    let tray_handle = if enable_tray {
        tray::init(&tray_icon_path())()
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
    let transfers = TransferTracker::new();
    let fuse_session = fs::spawn_fuse_session(&session, transfers.clone(), force_offline)
        .await
        .context("failed to mount Proton Drive filesystem")?;

    tracing::info!(socket = %path.display(), "pdcli daemon ready");

    let mut tray_update_interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (stream, _) = accept_result?;
                if handle_client_command(stream, &transfers).await? {
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

async fn handle_client_command(
    stream: tokio::net::UnixStream,
    transfers: &TransferTracker,
) -> anyhow::Result<bool> {
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
        "transfers" => {
            let body = serde_json::to_string(&transfers.snapshot())?;
            write_client_response(reader.get_mut(), body.as_bytes()).await;
            write_client_response(reader.get_mut(), b"\n").await;
        }
        other if other.starts_with("cancel-transfer ") => {
            let result = other["cancel-transfer ".len()..]
                .parse::<usize>()
                .map_err(anyhow::Error::from)
                .and_then(|id| transfers.cancel(id));
            let response = match result {
                Ok(()) => "ok".to_string(),
                Err(error) => format!("error: {error}"),
            };
            write_client_response(reader.get_mut(), response.as_bytes()).await;
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

fn tray_icon_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/icon.png"))
}

/// Takes the per-user tray lock so only one process shows the icon. The lock is
/// released when the returned file is dropped or the process exits.
fn lock_tray() -> Option<std::fs::File> {
    use std::os::fd::AsRawFd;
    let path = socket_path().ok()?.with_file_name("pdcli-tray.lock");
    let file = std::fs::File::create(path).ok()?;
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    locked.then_some(file)
}

/// Refuses daemons run by another user or from another executable, so the tray
/// never forwards actions to a process that merely bound our socket path.
#[cfg(target_os = "linux")]
fn verify_daemon_peer() -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;
    let stream = UnixStream::connect(socket_path()?)?;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    anyhow::ensure!(rc == 0, std::io::Error::last_os_error());
    anyhow::ensure!(
        cred.uid == unsafe { libc::geteuid() },
        "pdcli daemon belongs to another user"
    );
    let daemon = std::fs::read_link(format!("/proc/{}/exe", cred.pid))?;
    let ours = std::env::current_exe()?;
    anyhow::ensure!(
        same_executable(&daemon, &ours),
        "pdcli daemon runs {}, not {}",
        daemon.display(),
        ours.display()
    );
    Ok(())
}

// ponytail: other platforms rely on the per-user socket directory alone.
#[cfg(not(target_os = "linux"))]
fn verify_daemon_peer() -> anyhow::Result<()> {
    Ok(())
}

/// Compares `/proc/<pid>/exe` targets; a rebuilt or upgraded binary keeps its
/// path but gains a ` (deleted)` suffix.
fn same_executable(a: &std::path::Path, b: &std::path::Path) -> bool {
    let strip = |p: &std::path::Path| {
        let p = p.to_string_lossy();
        p.strip_suffix(" (deleted)").unwrap_or(&p).to_owned()
    };
    strip(a) == strip(b)
}

/// Shows the tray icon for a daemon started elsewhere and forwards its menu
/// actions over the daemon socket. Exits when the daemon stops.
pub fn attach_tray() -> anyhow::Result<()> {
    let Some(_lock) = lock_tray() else {
        return Ok(());
    };
    verify_daemon_peer()?;
    let _tray = tray::init(&tray_icon_path())();
    while let Some(status) = status() {
        tray::update_state(
            None,
            match status {
                DaemonStatus::Online => tray::TrayState::Online,
                DaemonStatus::Offline => tray::TrayState::Offline,
                DaemonStatus::Paused => tray::TrayState::Paused,
            },
        );
        for action in tray::poll_events() {
            let result = match action {
                tray::TrayAction::OpenFolder => open_folder(),
                tray::TrayAction::ShowHideWindow => Ok(launch_gui(None)),
                tray::TrayAction::Account => Ok(launch_gui(Some("account"))),
                tray::TrayAction::Settings => Ok(launch_gui(Some("settings"))),
                tray::TrayAction::ToggleSyncPause => request_toggle_pause().map(drop),
                tray::TrayAction::RetrySyncNow => request_retry_sync_now(),
                tray::TrayAction::SignOut => {
                    let result = request_quit();
                    credentials::remove();
                    result
                }
                tray::TrayAction::Quit => request_quit(),
            };
            if let Err(error) = result {
                tracing::error!(%error, ?action, "tray action failed");
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
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
            if let Err(error) = open_folder() {
                tracing::error!(%error, "could not open mounted folder");
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
    command.arg("gui");
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

pub(crate) async fn restore_session(force_offline: bool) -> anyhow::Result<ProtonAPISession> {
    let cred = credentials::load()
        .ok_or_else(|| anyhow::anyhow!("no stored credentials; run `pdcli login` to sign in"))?;

    let (entity_cache, secret_cache) = credentials::open_session_caches()?;

    let mut session = ProtonAPISession::from_stored_credentials(
        cred,
        app_version_configuration(),
        ProtonClientOptions {
            entity_cache_repository: Some(entity_cache),
            secret_cache_repository: Some(secret_cache),
            ..Default::default()
        },
    );

    let mut expired = session.token_credential.subscribe_refresh_token_expired();
    if !force_offline {
        if let Err(error) = session.ensure_authenticated().await {
            if expired.try_recv().is_ok() {
                forget_rejected_credentials(&session.token_credential).await;
                anyhow::bail!("session expired and was signed out; run `pdcli login`");
            }
            return Err(error);
        }
    }

    if let Ok(cred) = session.to_stored_credentials_with_latest_tokens().await {
        credentials::save(&cred).ok();
    }
    credentials::save_session_tokens_on_refresh(&session);
    let token_credential = session.token_credential.clone();
    tokio::spawn(async move {
        if expired.recv().await.is_ok() {
            forget_rejected_credentials(&token_credential).await;
        }
    });

    Ok(session)
}

/// Signs out after Proton rejects the refresh token, unless another process already
/// stored a newer token pair.
async fn forget_rejected_credentials(
    token_credential: &proton_drive_sdk::proton_sdk_rs2::auth::TokenCredential,
) {
    let Ok((_, rejected)) = token_credential.get_tokens().await else {
        return;
    };
    if credentials::load().is_some_and(|stored| stored.refresh_token() == rejected) {
        tracing::warn!("refresh token rejected; removing stored credentials");
        credentials::remove();
    }
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

#[cfg(test)]
mod tests {
    use super::same_executable;
    use std::path::Path;

    #[test]
    fn tray_attach_accepts_rebuilt_daemon_binary_but_not_another_path() {
        let ours = Path::new("/usr/bin/pdcli");
        assert!(same_executable(Path::new("/usr/bin/pdcli (deleted)"), ours));
        assert!(!same_executable(Path::new("/tmp/pdcli"), ours));
    }
}
