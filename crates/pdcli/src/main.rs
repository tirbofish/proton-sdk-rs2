mod app_paths;
mod auth;
mod commands;
mod daemon;
mod file_cache;
mod photos_index;
mod rusqlite_cache;
mod state;
mod fuse;
mod settings;

use anyhow::Result;
use reedline::{
    Completer, FileBackedHistory, Reedline, Signal, Span, Suggestion,
};
use std::sync::Arc;
use parking_lot::Mutex;
use std::time::Duration;

use crate::app_paths::resolve_paths;
use crate::state::ReplState;
use reedline::Prompt;
use std::borrow::Cow;

// ── Tab completion ─────────────────────────────────────────────────────────── 

const REMOTE_PATH_CMDS: &[&str] = &[
    "cd", "ls", "get", "hydrate", "mkdir", "mv", "cp", "rm", "stat", "restore", "drop",
];

const ALL_CMDS: &[&str] = &[
    "login", "whoami", "logout",
    "pwd", "ls", "cd",
    "mkdir", "mv", "cp", "rm", "drop", "stat", "restore",
    "get", "put", "hydrate",
    "cache", "computers", "photos", "sync", "settings", "daemon",
    "mount", "umount",
    "clear", "help", "exit", "quit",
];

struct DriveCompleter {
    state: Arc<Mutex<ReplState>>,
}

impl DriveCompleter {
    fn new(state: Arc<Mutex<ReplState>>) -> Self {
        DriveCompleter { state }
    }

    fn navigate_path(
        &self,
        cache: &crate::rusqlite_cache::RusqliteCache,
        start_uid: &proton_drive_sdk::node::NodeUid,
        parts: &[&str],
    ) -> Option<proton_drive_sdk::node::NodeUid> {
        use proton_drive_sdk::links::LinkId;
        let mut uid = start_uid.clone();
        for seg in parts {
            if seg.is_empty() || *seg == "." { continue; }
            let node = cache.get_child_by_name(&uid.volume_id, Some(&uid.link_id), seg).ok()??;
            uid = proton_drive_sdk::node::NodeUid::new(
                uid.volume_id.clone(),
                LinkId::new(node.link_id),
            );
        }
        Some(uid)
    }

    fn complete_remote(
        &self,
        path_prefix: &str,
    ) -> Vec<(String, bool)> {
        let s = match self.state.try_lock() {
            Some(s) => s,
            None => return Vec::new(),
        };
        let cache = match s.get_cache() {
            Some(c) => c,
            None => return Vec::new(),
        };
        let current_uid = match s.get_current_node_uid() {
            Some(u) => u.clone(),
            None => return Vec::new(),
        };

        // Split into the directory path and the name prefix.
        let (dir_str, name_prefix) = if let Some(slash) = path_prefix.rfind('/') {
            (&path_prefix[..slash + 1], &path_prefix[slash + 1..])
        } else {
            ("", path_prefix)
        };

        // Navigate to the directory.
        let dir_uid = if dir_str.is_empty() {
            current_uid
        } else {
            let segs: Vec<&str> = dir_str.trim_matches('/').split('/').collect();
            match self.navigate_path(&cache, &current_uid, &segs) {
                Some(u) => u,
                None => return Vec::new(),
            }
        };

        // List children of that directory.
        let children = cache
            .list_children(&dir_uid.volume_id, Some(&dir_uid.link_id))
            .unwrap_or_default();

        children
            .into_iter()
            .filter(|n| n.name.starts_with(name_prefix))
            .map(|n| {
                let is_dir = n.node_type == "Folder" || n.node_type == "Album";
                let full = format!("{}{}", dir_str, quote_for_shell(&n.name));
                (full, is_dir)
            })
            .collect()
    }
}

/// Wrap a file/dir name in single quotes if it contains characters that need
/// quoting in shell-style input (spaces, parens, apostrophes, etc.).
fn quote_for_shell(name: &str) -> String {
    let needs_quoting = name.chars().any(|c| matches!(c, ' ' | '\'' | '(' | ')' | '[' | ']' | '&' | '|' | ';' | '<' | '>' | '!' | '?' | '*' | '#' | '~'));
    if needs_quoting {
        // Escape any embedded single-quotes: foo'bar → foo'\''bar
        format!("'{}'", name.replace('\'', r"'\''"))
    } else {
        name.to_string()
    }
}

impl Completer for DriveCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let input = &line[..pos];
        let tokens: Vec<&str> = input.split_whitespace().collect();
        let ends_with_space = input.ends_with(' ');
        let n_tokens = tokens.len();

        // Complete command names when typing the first token.
        if n_tokens == 0 || (n_tokens == 1 && !ends_with_space) {
            let prefix = tokens.first().copied().unwrap_or("");
            let span = Span::new(0, pos);
            return ALL_CMDS
                .iter()
                .filter(|c| c.starts_with(prefix))
                .map(|c| Suggestion {
                    value: c.to_string(),
                    description: None,
                    style: None,
                    extra: None,
                    span,
                    append_whitespace: true,
                })
                .collect();
        }

        // Complete remote paths for supported commands.
        let cmd = tokens[0];
        if !REMOTE_PATH_CMDS.contains(&cmd) { return Vec::new(); }

        let raw_token = if ends_with_space { "" } else { tokens.last().copied().unwrap_or("") };
        let (in_quote, path_prefix) = if raw_token.starts_with('\'') {
            (true, &raw_token[1..])
        } else {
            (false, raw_token)
        };
        let token_start = pos - raw_token.len();
        let span = Span::new(token_start, pos);

        self.complete_remote(path_prefix)
            .into_iter()
            .map(|(mut name, is_dir)| {
                if in_quote && !name.starts_with('\'') {
                    name = format!("'{}'", name);
                }
                Suggestion {
                    value: name,
                    description: None,
                    style: None,
                    extra: None,
                    span,
                    append_whitespace: !is_dir,
                }
            })
            .collect()
    }
}

struct LivePrompt {
    state: Arc<Mutex<ReplState>>,
}

impl Prompt for LivePrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        if let Some(s) = self.state.try_lock() {
            if s.is_authenticated() {
                let user = s.get_username().unwrap_or("?");
                Cow::Owned(format!("{} {}", user, s.current_path_display()))
            } else {
                Cow::Borrowed("not logged in ")
            }
        } else {
            Cow::Borrowed("... ")
        }
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        if let Some(s) = self.state.try_lock() {
            if let Some(status) = s.get_sync_status() {
                Cow::Owned(format!("[{}]", status))
            } else {
                Cow::Borrowed("")
            }
        } else {
            Cow::Borrowed("[Busy]")
        }
    }

    fn render_prompt_indicator(&self, _prompt_mode: reedline::PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("〉")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("::: ")
    }

    fn render_prompt_history_search_indicator(&self, _history_search: reedline::PromptHistorySearch) -> Cow<'_, str> {
        Cow::Borrowed("search: ")
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Start deadlock detection thread
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(10));
            let deadlocks = parking_lot::deadlock::check_deadlock();
            if deadlocks.is_empty() {
                continue;
            }

            println!("{} deadlocks detected", deadlocks.len());
            for (i, threads) in deadlocks.iter().enumerate() {
                println!("Deadlock #{}", i);
                for t in threads {
                    println!("Thread Id {:#?}", t.thread_id());
                    println!("{:#?}", t.backtrace());
                }
            }
        }
    });

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    println!("ProtonDrive CLI");

    // If invoked as the daemon sub-process, run the daemon loop and exit.
    let cli_args: Vec<String> = std::env::args().skip(1).collect();
    if cli_args.first().map(String::as_str) == Some("--daemon-run")
        || std::env::var("PDCLI_DAEMON").is_ok()
    {
        return daemon::run_daemon_process().await;
    }

    let init_spinner = commands::helpers::new_spinner("Initialising...");
    
    let mut state = ReplState::new();
    let paths = resolve_paths()?;
    
    // Load settings
    let settings = commands::settings::load_settings(&paths)?;
    state.set_settings(settings);
    
    let state = Arc::new(Mutex::new(state));
    let cache_path = paths.cache_dir.join("drive_cache.db");
    let cache = Arc::new(rusqlite_cache::RusqliteCache::new(&cache_path)?);
    state.lock().set_cache(cache.clone());
    
    let restored = auth::try_resume_session().await?;
    init_spinner.finish_and_clear();

    if cli_args.len() == 1 && cli_args[0] == "help" {
        show_help();
        return Ok(());
    }

    if let Some((session, username)) = restored {
        if let Err(error) = commands::apply_authenticated_session_with_options(&state, session, username, cli_args.is_empty()).await {
            eprintln!("Failed to restore session: {error}");
        } else {
            let (client, volume_id, cache, local_root, index_mode) = {
                let s = state.lock();
                let c = s.get_client().expect("client exists").clone();
                let root = s.get_root_node_uid().expect("root exists");
                let ca = s.get_cache().expect("cache exists").clone();
                let v = root.volume_id.clone();
                let lr = ca.get_sync_state(&v).ok().flatten().and_then(|(_, r)| r);
                let mode = s.get_settings().indexing.mode;
                (c, v, ca, lr, mode)
            };

            use crate::settings::IndexMode;
            match index_mode {
                IndexMode::IndexOnInit => {
                    state.lock().set_sync_status(Some("Indexing...".to_string()));
                    if let Err(e) = commands::sync::run_initial_sync(&client, &volume_id, &cache, local_root.clone(), &state).await {
                        eprintln!("Initial sync warning: {e}");
                    }
                    state.lock().set_myfiles_indexed(true);
                }
                IndexMode::IndexOnDemand => {
                    // Minimal setup: snapshot the event cursor and root children only.
                    state.lock().set_sync_status(Some("Ready".to_string()));
                    if let Err(e) = commands::sync::run_minimal_sync(&client, &volume_id, &cache, local_root.clone()).await {
                        eprintln!("Sync warning: {e}");
                    }
                    state.lock().set_sync_status(Some("Up to date (Idle)".to_string()));
                }
            }

            // Spawn the events loop in the background — it runs forever.
            let state_clone = state.clone();
            let lr_clone = local_root.clone();
            tokio::spawn(async move {
                let _ = commands::sync::run_events_loop(client, volume_id, cache, lr_clone, state_clone, None, None).await;
            });
        }
    }

    run_main_loop(state, cli_args).await
}

async fn run_main_loop(state: Arc<Mutex<ReplState>>, cli_args: Vec<String>) -> Result<()> {
    if !cli_args.is_empty() {
        let cmd = cli_args[0].as_str();
        let args: Vec<&str> = cli_args[1..].iter().map(String::as_str).collect();
        let _ = dispatch_command(cmd, &args, &state, true).await?;
        return Ok(());
    }

    let history_file = resolve_paths()
        .map(|p| p.history_path)
        .unwrap_or_else(|_| ".pdcli_history".into());

    let history = Box::new(
        FileBackedHistory::with_file(1000, history_file)
            .unwrap_or_else(|_| FileBackedHistory::new(1000).expect("history")),
    );
    let mut editor = Reedline::create()
        .with_history(history)
        .with_completer(Box::new(DriveCompleter::new(state.clone())));

    loop {
        if !state.lock().is_authenticated() {
            println!("Please log in to continue.");
            if let Err(e) = commands::auth_command_with_options(&state, true).await {
                eprintln!("Login failed: {e}");
                continue;
            }
        }

        println!("Type 'help' for available commands.\n");

        let prompt = LivePrompt { state: state.clone() };

        'repl: loop {
            // Reedline doesn't naturally support background redraws easily,
            // but we can use update_prompt if we had access to the editor.
            // For now, the implementation above ensures that every time reedline 
            // decides to draw (e.g. on every keypress), it gets the latest state.
            let mut ctrl_c_count = 0u32;
            match editor.read_line(&prompt) {
                Ok(Signal::Success(line)) => {
                    ctrl_c_count = 0; let _ = ctrl_c_count;
                    let trimmed = line.trim();
                    if trimmed.is_empty() { continue; }
                    state.lock().clear_cancelled();
                    let cmd_result = tokio::select! {
                        res = handle_command(trimmed, &state) => res,
                        _ = tokio::signal::ctrl_c() => {
                            eprintln!("\n  Interrupted.");
                            continue;
                        }
                    };
                    match cmd_result {
                        Ok(should_exit) => {
                            if should_exit {
                                // Auto-unmount any active FUSE mount before exiting
                                if let Some(mp) = state.lock().get_mount_point().cloned() {
                                    println!("Unmounting {}...", mp.display());
                                    let _ = std::process::Command::new("fusermount3")
                                        .arg("-u").arg("-z").arg(&mp).output();
                                }
                                println!("Goodbye!");
                                return Ok(());
                            }
                            if !state.lock().is_authenticated() {
                                print!("\x1B[2J\x1B[1;1H");
                                break 'repl;
                            }
                        }
                        Err(e) => eprintln!("Error: {}", e),
                    }
                }
                Ok(Signal::CtrlC) => {
                    ctrl_c_count += 1;
                    if ctrl_c_count >= 2 {
                        // Double Ctrl+C = quit immediately.
                        if let Some(mp) = state.lock().get_mount_point().cloned() {
                            let _ = std::process::Command::new("fusermount3")
                                .arg("-u").arg("-z").arg(&mp).output();
                        }
                        println!("Goodbye!");
                        return Ok(());
                    }
                    println!("(Press Ctrl+C again to quit, or Ctrl+D)");
                }
                Ok(Signal::CtrlD) => {
                    println!("Goodbye!");
                    return Ok(());
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    return Ok(());
                }
            }
        }
    }
}

async fn handle_command(input: &str, state: &Arc<Mutex<ReplState>>) -> Result<bool> {
    let parts = shlex::split(input).ok_or_else(|| anyhow::anyhow!("Unmatched quote in command"))?;
    if parts.is_empty() { return Ok(false); }
    let cmd = parts[0].as_str();
    let args: Vec<&str> = parts[1..].iter().map(String::as_str).collect();
    dispatch_command(cmd, &args, state, false).await
}

async fn dispatch_command(
    cmd: &str,
    args: &[&str],
    state: &Arc<Mutex<ReplState>>,
    one_shot_mode: bool,
) -> Result<bool> {
    match cmd {
        "login" => { commands::auth_command_with_options(state, !one_shot_mode).await?; Ok(false) }
        "whoami" => { commands::whoami_command(state).await?; Ok(false) }
        "logout" => { commands::logout_command(state).await?; Ok(false) }
        "pwd" => { commands::pwd_command(state).await?; Ok(false) }
        "ls" => { commands::ls_command(&args, state).await?; Ok(false) }
        "cd" => { commands::cd_command(&args, state).await?; Ok(false) }
        "mkdir" => { commands::mkdir_command(&args, state).await?; Ok(false) }
        "mv" => { commands::move_command(&args, state).await?; Ok(false) }
        "cp" => { commands::cp_command(&args, state).await?; Ok(false) }
        "stat" => { commands::stat_command(&args, state).await?; Ok(false) }
        "rm" => { commands::remove_command(&args, state).await?; Ok(false) }
        "drop" => { commands::drop_command(&args, state).await?; Ok(false) }
        "restore" => { commands::restore_command(&args, state).await?; Ok(false) }
        "get" => { commands::download_command(&args, state).await?; Ok(false) }
        "put" => { commands::upload_command(&args, state).await?; Ok(false) }
        "hydrate" => { commands::hydrate_command(&args, state).await?; Ok(false) }
        "cache" => { commands::cache_command(&args, state).await?; Ok(false) }
        "computers" => { commands::computers_command(&args, state).await?; Ok(false) }
        "photos" => { commands::photos_command(&args, state).await?; Ok(false) }
        "sync" => { commands::sync_command(&args, state).await?; Ok(false) }
        "settings" => { commands::settings_command(&args, state).await?; Ok(false) }
        "daemon" => { commands::daemon_command(&args, state).await?; Ok(false) }
        "mount" => {
            let use_daemon = args.iter().any(|a| *a == "--daemon" || *a == "-d");
            let mp_args: Vec<&str> = args.iter().copied().filter(|a| *a != "--daemon" && *a != "-d").collect();
            if use_daemon {
                if mp_args.is_empty() {
                    eprintln!("Usage: mount <mount_point> [--daemon|-d]");
                } else {
                    if !crate::daemon::is_daemon_alive() {
                        println!("  Starting daemon...");
                        crate::daemon::daemon_start()?;
                    }
                    // Wait up to 10 s for the daemon socket to become available.
                    let mut ready = false;
                    for _ in 0..20 {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        if crate::daemon::is_daemon_alive() {
                            if crate::daemon::send_daemon_command("ping").is_ok() {
                                ready = true;
                                break;
                            }
                        }
                    }
                    if !ready {
                        eprintln!("  Daemon did not start in time. Try 'daemon start' manually.");
                    } else {
                        match crate::daemon::send_daemon_command(&format!("mount {}", mp_args[0])) {
                            Ok(_) => println!("  Mount request sent. The drive will appear at {} shortly.", mp_args[0]),
                            Err(e) => eprintln!("  Failed to send mount request: {e}"),
                        }
                    }
                }
            } else {
                commands::mount_command(&mp_args, state).await?;
            }
            Ok(false)
        }
        "umount" => {
            commands::umount_command(&args, state).await?;
            Ok(false)
        }

        "clear" => { print!("\x1B[2J\x1B[1;1H"); Ok(false) }
        "help" => { if args.is_empty() { show_help(); } else { show_command_help(args[0]); } Ok(false) }
        "exit" | "quit" => Ok(true),
        _ => { eprintln!("Unknown command: '{}'", cmd); Ok(false) }
    }
}

fn show_help() {
    println!(
        r#"
pdcli - Proton Drive file manager

AUTHENTICATION:
  login, whoami, logout

NAVIGATION:
  pwd, ls [path], cd [path]

FILE OPERATIONS:
  mkdir, mv, cp, rm, drop, stat, restore

TRANSFER:
  get, put, hydrate, cache, mount, umount

PHOTOS:
  photos ls                  Show photo library timeline summary
  photos get <id> <path>     Download a photo by link ID

COMPUTERS:
  computers ls               List registered computers
  computers add <name>       Register a new computer
  computers rename <id> <n>  Rename a computer
  computers rm <id>          Unregister a computer
  computers sync <folder>    Sync a local folder to this computer's backup

SETTINGS:
  settings

OTHER:
  help [command], clear, exit
"#
    );
}

fn show_command_help(cmd: &str) {
    match cmd {
        "cp" => println!("COMMAND: cp <src_name> <dst_name>\nCreate a copy of a file or folder."),
        "cache" => println!("COMMAND: cache <get|clear>\nManage local data and SQLite database."),
        "computers" => println!("COMMAND: computers [ls|add|rename|rm]\nManage registered Computers (backup devices). Run 'computers help' for details."),
        "photos" => println!("COMMAND: photos [ls|get]\nBrowse and download your Proton Drive photo library. Run 'photos help' for details."),
        "settings" => println!("COMMAND: settings [display|reset]\nManage application settings (indexing, mounting, etc.). Run without args for interactive menu."),
        "mount" => println!("COMMAND: mount <mount_point>\nMount Drive as a local FUSE filesystem."),
        "hydrate" => println!("COMMAND: hydrate <path|pattern>\nDownload a file, folder, or wildcard pattern to the persistent cache for offline FUSE access."),
        _ => println!("Use 'help' to see all commands."),
    }
}
