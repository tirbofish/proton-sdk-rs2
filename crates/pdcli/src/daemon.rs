use crate::app_paths::resolve_paths;
use anyhow::{anyhow, Result};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::time::Duration;

const SYNC_INTERVAL_SECS: u64 = 300;

pub fn is_daemon_alive() -> bool {
    let Ok(paths) = resolve_paths() else { return false };
    read_daemon_pid(&paths.daemon_pid_path)
        .map(|pid| process_alive(pid))
        .unwrap_or(false)
}

pub fn daemon_start() -> Result<()> {
    if is_daemon_alive() {
        println!("Daemon is already running.");
        return Ok(());
    }

    let exe = std::env::current_exe()
        .map_err(|e| anyhow!("Cannot resolve current executable: {e}"))?;

    let child = std::process::Command::new("nohup")
        .arg(&exe)
        .arg("--daemon-run")
        .env("PDCLI_DAEMON", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| anyhow!("Failed to spawn daemon: {e}"))?;

    println!("Daemon started (PID {}).", child.id());
    Ok(())
}

pub fn daemon_stop() -> Result<()> {
    let paths = resolve_paths()?;
    if let Some(pid) = read_daemon_pid(&paths.daemon_pid_path) {
        if process_alive(pid) {
            #[cfg(unix)]
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM); }
            for _ in 0..50 {
                std::thread::sleep(Duration::from_millis(100));
                if !process_alive(pid) { break; }
            }
            if process_alive(pid) {
                #[cfg(unix)]
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL); }
            }
            println!("Daemon stopped.");
        } else {
            println!("Daemon PID file exists but process is gone — cleaning up.");
            let _ = std::fs::remove_file(&paths.daemon_pid_path);
        }
    } else {
        println!("Daemon is not running.");
    }
    Ok(())
}

pub fn daemon_status() -> Result<()> {
    let paths = resolve_paths()?;
    if let Some(pid) = read_daemon_pid(&paths.daemon_pid_path) {
        if process_alive(pid) {
            println!("Daemon is running (PID {}).", pid);
            if let Ok(reply) = send_daemon_command("ping") {
                println!("  Socket reply: {reply}");
            }
        } else {
            println!("Daemon PID file exists but process {pid} is gone.");
        }
    } else {
        println!("Daemon is not running.");
    }
    Ok(())
}

#[allow(dead_code)]
pub fn daemon_mount(mount_point: &Path) -> Result<String> {
    if !is_daemon_alive() {
        anyhow::bail!("Daemon is not running. Start it first with: daemon init");
    }
    send_daemon_command(&format!("mount {}", mount_point.display()))
}

#[allow(dead_code)]
pub fn daemon_umount(mount_point: &Path) -> Result<String> {
    if !is_daemon_alive() {
        anyhow::bail!("Daemon is not running.");
    }
    send_daemon_command(&format!("umount {}", mount_point.display()))
}

pub fn send_daemon_command(cmd: &str) -> Result<String> {
    let paths = resolve_paths()?;
    let mut stream = UnixStream::connect(&paths.daemon_socket_path)
        .map_err(|e| anyhow!("Cannot connect to daemon socket: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;

    writeln!(stream, "{cmd}")?;
    stream.flush()?;

    let mut reader = BufReader::new(&stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    Ok(reply.trim().to_string())
}

pub async fn run_daemon_process() -> Result<()> {
    let paths = resolve_paths()?;
    let log_path = paths.cache_dir.join("daemon.log");
    let _log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path);

    write_daemon_pid(&paths.daemon_pid_path)?;
    let _cleanup = PidFileGuard {
        pid_path: paths.daemon_pid_path.clone(),
        sock_path: paths.daemon_socket_path.clone(),
    };

    eprintln!("[daemon] Starting (PID {})", std::process::id());

    let (session, username) = crate::auth::try_resume_session()
        .await?
        .ok_or_else(|| anyhow!("No saved session — run 'login' interactively first"))?;

    let state = {
        use crate::state::ReplState;
        use std::sync::Arc;
        use parking_lot::Mutex;
        let cache_path = paths.cache_dir.join("drive_cache.db");
        let cache = Arc::new(crate::rusqlite_cache::RusqliteCache::new(&cache_path)?);
        let mut s = ReplState::new();
        s.set_cache(cache);
        let state = Arc::new(Mutex::new(s));
        crate::commands::apply_authenticated_session_with_options(&state, session, username, false).await?;
        state
    };

    eprintln!("[daemon] Authenticated.");

    let sock_path = &paths.daemon_socket_path;
    let _ = std::fs::remove_file(sock_path);
    let listener = UnixListener::bind(sock_path)
        .map_err(|e| anyhow!("Cannot bind daemon socket: {e}"))?;
    listener.set_nonblocking(true)?;

    tokio::spawn(daemon_sync_loop(state.clone()));
    tokio::spawn(daemon_socket_listener(listener, state));

    tokio::signal::ctrl_c().await?;
    eprintln!("[daemon] Shutting down.");
    Ok(())
}

async fn daemon_sync_loop(state: std::sync::Arc<parking_lot::Mutex<crate::state::ReplState>>) {
    loop {
        let (client_opt, cache_opt) = {
            let s = state.lock();
            (s.get_client().cloned(), s.get_cache())
        };
        if let (Some(client), Some(cache)) = (client_opt, cache_opt) {
            eprintln!("[daemon] Indexing computers...");
            if let Ok(devices) = client.list_devices().await {
                use crate::commands::helpers::list_children;
                for device in &devices {
                    if let Ok(nodes) = list_children(&client, device.root_uid.clone()).await {
                        for node in &nodes {
                            let _ = cache.upsert_node(node, false);
                        }
                    }
                }
                eprintln!("[daemon] Index pass complete ({} computer(s)).", devices.len());
            }
        }
        tokio::time::sleep(Duration::from_secs(SYNC_INTERVAL_SECS)).await;
    }
}

async fn daemon_socket_listener(
    listener: UnixListener,
    state: std::sync::Arc<parking_lot::Mutex<crate::state::ReplState>>,
) {
    let listener = tokio::net::UnixListener::from_std(listener).expect("convert listener");
    loop {
        match listener.accept().await {
            Ok((stream, _)) => { tokio::spawn(handle_daemon_client(stream, state.clone())); }
            Err(e) => {
                eprintln!("[daemon] Socket accept error: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn handle_daemon_client(
    stream: tokio::net::UnixStream,
    state: std::sync::Arc<parking_lot::Mutex<crate::state::ReplState>>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    if reader.read_line(&mut line).await.is_ok() {
        let cmd = line.trim();
        let reply = match cmd {
            "ping" => "pong".to_string(),
            c if c.starts_with("mount ") => {
                let path_str = c["mount ".len()..].to_string();
                eprintln!("[daemon] Mounting '{path_str}'...");
                tokio::spawn(async move {
                    if let Err(e) = crate::commands::mount_command(&[path_str.as_str()], &state).await {
                        eprintln!("[daemon] Mount ended: {e}");
                    }
                });
                "mounting".to_string()
            }
            c if c.starts_with("umount ") => {
                let mp = &c["umount ".len()..];
                eprintln!("[daemon] Unmounting '{mp}'...");
                let _ = std::process::Command::new("fusermount3")
                    .arg("-u").arg("-z").arg(mp)
                    .output();
                "ok".to_string()
            }
            "status" => format!("running pid={}", std::process::id()),
            other => format!("unknown: {other}"),
        };
        let _ = write_half.write_all(format!("{reply}\n").as_bytes()).await;
    }
}

fn read_daemon_pid(pid_path: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

fn write_daemon_pid(pid_path: &Path) -> Result<()> {
    std::fs::write(pid_path, format!("{}\n", std::process::id()))
        .map_err(|e| anyhow!("Cannot write PID file: {e}"))
}

fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    #[cfg(not(unix))]
    false
}

struct PidFileGuard {
    pid_path: std::path::PathBuf,
    sock_path: std::path::PathBuf,
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.pid_path);
        let _ = std::fs::remove_file(&self.sock_path);
    }
}
