mod app_paths;
mod auth;
mod commands;
mod file_cache;
mod fs_permissions;
mod state;

use anyhow::Result;
use indicatif::ProgressBar;
use reedline::{
    DefaultPrompt, DefaultPromptSegment, FileBackedHistory, Reedline, Signal,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::app_paths::resolve_paths;
use state::ReplState;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    println!("ProtonDrive CLI"); // change this to ascii art sometime later

    let cli_args: Vec<String> = std::env::args().skip(1).collect();

    if cli_args.len() == 1 && cli_args[0] == "help" {
        show_help();
        return Ok(());
    }

    let state = Arc::new(Mutex::new(ReplState::new()));

    let restore_spinner = ProgressBar::new_spinner();
    restore_spinner.set_message("Authenticating...");
    restore_spinner.enable_steady_tick(Duration::from_millis(100));
    let restored = auth::try_resume_session().await?;
    restore_spinner.finish_and_clear();

    let announce = cli_args.is_empty();
    if let Some((session, username)) = restored {
        if let Err(error) = commands::apply_authenticated_session_with_options(&state, session, username, announce).await {
            eprintln!("Failed to restore session: {error}");
        }
    }

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
        // user must be authenticated to access the repl
        if !state.lock().await.is_authenticated() {
            println!("Please log in to continue.");
            if let Err(e) = commands::auth_command_with_options(&state, true).await {
                eprintln!("Login failed: {e}");
                // let them try again
                continue;
            }
        }

        println!("Type 'help' for available commands.\n");

        'repl: loop {
            let prompt = {
                let s = state.lock().await;
                let left = if s.is_authenticated() {
                    let user = s.get_username().unwrap_or("?");
                    format!("{} {}", user, s.current_path_display())
                } else {
                    "not logged in".to_string()
                };
                DefaultPrompt::new(
                    DefaultPromptSegment::Basic(left),
                    DefaultPromptSegment::Empty,
                )
            };

            match editor.read_line(&prompt) {
                Ok(Signal::Success(line)) => {
                    let trimmed = line.trim();

                    if trimmed.is_empty() {
                        continue;
                    }

                    // Clear any previous cancellation state before running new command
                    state.lock().await.clear_cancelled();

                    match handle_command(trimmed, &state).await {
                        Ok(should_exit) => {
                            if should_exit {
                                println!("Goodbye!");
                                return Ok(());
                            }
                            // If logout cleared the session, break back to auth loop
                            if !state.lock().await.is_authenticated() {
                                print!("\x1B[2J\x1B[1;1H");
                                break 'repl;
                            }
                        }
                        Err(e) => {
                            eprintln!("Error: {}", e);
                        }
                    }
                }
                Ok(Signal::CtrlC) => {
                    state.lock().await.set_cancelled();
                    println!("Cancelling operation...");
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

    if parts.is_empty() {
        return Ok(false);
    }

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
        // Authentication
        "login" => {
            commands::auth_command_with_options(state, !one_shot_mode).await?;
            Ok(false)
        }
        "whoami" => {
            commands::whoami_command(state).await?;
            Ok(false)
        }
        "logout" => {
            commands::logout_command(state).await?;
            Ok(false)
        }

        // Navigation
        "pwd" => {
            commands::pwd_command(state).await?;
            Ok(false)
        }
        "ls" => {
            commands::ls_command(&args, state).await?;
            Ok(false)
        }
        "cd" => {
            commands::cd_command(&args, state).await?;
            Ok(false)
        }

        // File operations
        "mkdir" => {
            commands::mkdir_command(&args, state).await?;
            Ok(false)
        }
        "mv" => {
            commands::move_command(&args, state).await?;
            Ok(false)
        }
        "stat" => {
            commands::stat_command(&args, state).await?;
            Ok(false)
        }
        "rm" => {
            commands::remove_command(&args, state).await?;
            Ok(false)
        }
        "drop" => {
            commands::drop_command(&args, state).await?;
            Ok(false)
        }

        // Transfer
        "get" => {
            commands::download_command(&args, state).await?;
            Ok(false)
        }
        "put" => {
            commands::upload_command(&args, state).await?;
            Ok(false)
        }

        "clear" => {
            print!("\x1B[2J\x1B[1;1H");
            Ok(false)
        }

        // Meta
        "help" => {
            if args.is_empty() {
                show_help();
            } else {
                show_command_help(args[0]);
            }
            Ok(false)
        }
        "exit" | "quit" => Ok(true),

        _ => {
            eprintln!("Unknown command: '{}'. Type 'help' for available commands.", cmd);
            Ok(false)
        }
    }
}

fn show_help() {
    println!(
        r#"
pdcli - Proton Drive file manager for client

AUTHENTICATION:
  login           - Authenticate with your ProtonDrive account
  whoami          - Display current user information
  logout          - End the current session

NAVIGATION:
  pwd             - Print working directory
  ls [path]       - List files in directory (default: current)
  cd [path]       - Change directory (supports ./ and ../ style paths)

FILE OPERATIONS:
  mkdir <path>    - Create a new folder
  mv <src> <dst>  - Move/rename (supports wildcards: *.png)
  rm <path>       - Move item(s) to Trash (supports wildcards)
  drop <pattern>  - Permanently delete Trash item(s) by name/pattern
  stat <path>     - Display file information (supports wildcards)

TRANSFER:
  get <remote> [local] - Download file(s) (supports wildcards)
  put <local> [remote]       - Upload local file(s) with optional remote destination

OTHER:
  help [command]  - Show general help or help for a specific command
  clear           - Clear the screen
  exit/quit       - Exit the program
"#
    );
}

fn show_command_help(cmd: &str) {
    match cmd {
        "mv" => println!(
            r#"
COMMAND: mv [--force] <src> <dst>
Move or rename items. Supports wildcards.

OPTIONS:
  --force, -f  - Overwrite destination without prompting

EXAMPLES:
  > mv file.txt Documents/
  > mv *.png backup/
  > mv folder new-name
  > mv --force old.png new.png  (if new.png exists, overwrite it)

BEHAVIOR:
When destination exists, you can:
  0 - Replace the file
  1 - Compare file stats (size, type)
  2 - Change name and move (auto-renames to filename (1), etc)
  3 - Skip
"#
        ),
        "rm" => println!(
            r#"
COMMAND: rm [--force] <path|pattern> ...
Move item(s) to Trash. Supports wildcards and multiple paths.

EXAMPLES:
  > rm file.txt
  > rm *.log
  > rm folder/
  > rm file1.txt file2.txt
  > rm --force *.tmp  (skip prompts)

NOTE: Files are moved to Trash, not permanently deleted.
Use 'cd ../Trash; drop *' to permanently delete items.
"#
        ),
        "drop" => println!(
            r#"
COMMAND: drop <name|pattern> ...
Permanently delete item(s) from Trash. Requires confirmation.

EXAMPLES:
  > drop file.txt
  > drop *.log
  > drop old-*

NOTE: This permanently deletes files. Use with caution.
You must be in the Trash folder to use this command.
"#
        ),
        "ls" => println!(
            r#"
COMMAND: ls [path|pattern]
List files in directory. Supports wildcards for filtering.

EXAMPLES:
  > ls              (list current directory)
  > ls Documents/   (list Documents folder)
  > ls *.png        (list only PNG files in current dir)
  > ls Documents/*.txt  (list TXT files in Documents)

OUTPUT:
  [DIR] marks directories
  File sizes shown in human-readable format
  (mimetype) shown for files
"#
        ),
        "cd" => println!(
            r#"
COMMAND: cd [path]
Change directory. Supports Unix-style paths.

EXAMPLES:
  > cd Documents
  > cd ..           (go up one level)
  > cd ../Pictures  (go up, then into Pictures)
  > cd ./MyFolder   (go into MyFolder in current dir)
  > cd /            (go to top-level)
  > cd MyFiles      (go to My Files)
  > cd Trash        (go to Trash)

TOP-LEVEL ENTRIES:
  MyFiles  - Your main file storage
  Trash    - Deleted items
  Photos   - (not implemented yet)
"#
        ),
        "get" => println!(
            r#"
COMMAND: get <remote> [local]
Download file(s) from cloud to local. Supports wildcards.

EXAMPLES:
  > get file.txt ~/file.txt         (download to home)
  > get file.txt                    (download to current dir)
  > get Documents/report.pdf ~/
  > get *.png ./downloads/          (wildcard download)

PROGRESS:
  Progress bar shown during download
  File size checked before transfer
"#
        ),
        "put" => println!(
            r#"
COMMAND: put <local_path|pattern> ...
Upload file(s) to cloud storage. Supports wildcards and multiple files.

EXAMPLES:
  > put ~/file.txt
  > put ~/data.csv Documents/
  > put ~/screenshots/*.png
  > put ~/file1.txt ~/file2.txt ~/file3.txt

NOTES:
  - When using wildcards, local_path must be a directory
  - Progress bar shown during upload
  - Use absolute paths (~/...) or relative paths
"#
        ),
        "mkdir" => println!(
            r#"
COMMAND: mkdir <path>
Create a new folder.

EXAMPLES:
  > mkdir NewFolder
  > mkdir Documents/Subfolder
  > mkdir backup-2026
"#
        ),
        "stat" => println!(
            r#"
COMMAND: stat <path|pattern> ...
Display detailed file information. Supports wildcards.

EXAMPLES:
  > stat file.txt
  > stat *.png        (shows info for all PNG files)
  > stat Documents/*  (shows info for all items in Documents)

INFO DISPLAYED:
  Name, UID, Created date
  Parent UID, Type, MIME type
  Size (actual file size), Revision ID
"#
        ),
        "login" => println!(
            r#"
COMMAND: login
Authenticate with your ProtonDrive account.

EXAMPLES:
  > login

You will be prompted for username and password.
Session credentials are saved securely.
"#
        ),
        "logout" => println!(
            r#"
COMMAND: logout
End the current session and clear cached credentials.

EXAMPLES:
  > logout

Your session will be terminated.
"#
        ),
        _ => println!("Unknown command: '{}'. Type 'help' for available commands.", cmd),
    }
}


