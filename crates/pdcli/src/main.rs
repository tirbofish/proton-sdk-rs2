mod app_paths;
mod auth;
mod commands;
mod file_cache;
mod rusqlite_cache;
mod state;
mod fuse;

use anyhow::Result;
use indicatif::ProgressBar;
use reedline::{FileBackedHistory, Reedline, Signal};
use std::sync::Arc;
use parking_lot::Mutex;
use std::time::Duration;

use crate::app_paths::resolve_paths;
use crate::state::ReplState;
use reedline::Prompt;
use std::borrow::Cow;

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

    let init_spinner = ProgressBar::new_spinner();
    init_spinner.set_message("Initializing...");
    init_spinner.enable_steady_tick(Duration::from_millis(100));
    
    let state = Arc::new(Mutex::new(ReplState::new()));
    let paths = resolve_paths()?;
    
    let cache_path = paths.cache_dir.join("drive_cache.db");
    let cache = Arc::new(rusqlite_cache::RusqliteCache::new(&cache_path)?);
    state.lock().set_cache(cache.clone());
    
    let restored = auth::try_resume_session().await?;
    init_spinner.finish_and_clear();

    let cli_args: Vec<String> = std::env::args().skip(1).collect();

    if cli_args.len() == 1 && cli_args[0] == "help" {
        show_help();
        return Ok(());
    }

    if let Some((session, username)) = restored {
        if let Err(error) = commands::apply_authenticated_session_with_options(&state, session, username, cli_args.is_empty()).await {
            eprintln!("Failed to restore session: {error}");
        } else {
            let (client, volume_id, cache, local_root) = {
                let s = state.lock();
                let c = s.get_client().expect("client exists").clone();
                let root = s.get_root_node_uid().expect("root exists");
                let ca = s.get_cache().expect("cache exists").clone();
                let v = root.volume_id.clone();
                let lr = ca.get_sync_state(&v).ok().flatten().and_then(|(_, r)| r);
                (c, v, ca, lr)
            };

            // Block here until initial indexing is done so the REPL is fully
            // navigable as soon as it appears.
            state.lock().set_sync_status(Some("Indexing...".to_string()));
            if let Err(e) = commands::sync::run_initial_sync(&client, &volume_id, &cache, local_root.clone(), &state).await {
                eprintln!("Initial sync warning: {e}");
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
    let mut editor = Reedline::create().with_history(history);

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
            
            match editor.read_line(&prompt) {
                Ok(Signal::Success(line)) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() { continue; }
                    state.lock().clear_cancelled();
                    match handle_command(trimmed, &state).await {
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
                    // If we were in the middle of a command, this would cancel it.
                    // But reedline returns CtrlC when the prompt is empty.
                    println!("Goodbye!");
                    return Ok(());
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
        "stat" => { commands::stat_command(&args, state).await?; Ok(false) }
        "rm" => { commands::remove_command(&args, state).await?; Ok(false) }
        "drop" => { commands::drop_command(&args, state).await?; Ok(false) }
        "restore" => { commands::restore_command(&args, state).await?; Ok(false) }
        "get" => { commands::download_command(&args, state).await?; Ok(false) }
        "put" => { commands::upload_command(&args, state).await?; Ok(false) }
        "hydrate" => { commands::hydrate_command(&args, state).await?; Ok(false) }
        "sync" => { commands::sync_command(&args, state).await?; Ok(false) }
        "cache" => { commands::cache_command(&args, state).await?; Ok(false) }
        "mount" => {
            commands::mount_command(&args, state).await?;
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
  mkdir, mv, rm, drop, stat

TRANSFER:
  get, put, hydrate, sync, cache, mount, umount

OTHER:
  help [command], clear, exit
"#
    );
}

fn show_command_help(cmd: &str) {
    match cmd {
        "sync" => println!("COMMAND: sync <local_path>\nStart background sync with SQLite caching."),
        "cache" => println!("COMMAND: cache <get|clear>\nManage local data and SQLite database."),
        "mount" => println!("COMMAND: mount <mount_point>\nMount Drive as a local FUSE filesystem."),
        "hydrate" => println!("COMMAND: hydrate <path>\nDownload a file to the persistent cache for offline FUSE access."),
        _ => println!("Use 'help' to see all commands."),
    }
}
