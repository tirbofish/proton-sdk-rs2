pub mod computers;
pub mod fs;
pub mod info;
pub mod nav;
pub mod photos;
pub mod revisions;
pub mod settings;
pub mod trash;

use crate::app::AppState;
use crate::vfs::VfsSection;

/// Parses and runs `line`. Automatically cancels if `state.cancel` is triggered
/// before or during execution — new commands added to `run_cmd` are covered
/// without any extra wiring.
pub async fn dispatch(line: &str, state: &mut AppState) -> anyhow::Result<()> {
    let Some(tokens) = shlex::split(line) else {
        eprintln!("Syntax error: {line}");
        return Ok(());
    };
    let Some(cmd) = tokens.first().map(|s| s.as_str()) else {
        return Ok(());
    };
    let args = &tokens[1..];

    // Clone before the mutable borrow so that both arms of select! can coexist.
    let cancel = state.cancel.clone();
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Ok(()),
        result = run_cmd(cmd, args, state) => result,
    }
}

async fn run_cmd(cmd: &str, args: &[String], state: &mut AppState) -> anyhow::Result<()> {
    match cmd {
        "whoami" => info::whoami(state).await,
        "logout" => info::logout(args, state).await,
        "stat" => info::stat(args, state).await,
        "pwd" => nav::pwd(state).await,
        "ls" => nav::ls(args, state).await,
        "cd" => nav::cd(args, state).await,
        "mkdir" => match &state.cwd.section {
            VfsSection::Photos => photos::mkdir(args, state).await,
            _ => fs::mkdir(args, state).await,
        },
        "mv" => fs::mv(args, state).await,
        "cp" => fs::cp(args, state).await,
        "rm" if state.cwd.section == VfsSection::Computers => {
            computers::remove(args, state).await
        }
        "rm" if state.cwd.section == VfsSection::Trash => trash::drop_cmd(args, state).await,
        "rm" => fs::rm(args, state).await,
        "get" => fs::get(args, state).await,
        "put" => fs::put(args, state).await,
        "touch" => fs::touch(args, state).await,
        "rev" => revisions::rev(args, state).await,
        "drop" => trash::drop_cmd(args, state).await,
        "restore" => match &state.cwd.section {
            VfsSection::Trash => trash::restore(args, state).await,
            _ => {
                eprintln!("restore is only valid in /Trash — use 'cd /Trash' first");
                Ok(())
            }
        },
        "sync" => computers::sync(args, state).await,
        "add" if state.cwd.section == VfsSection::Computers => {
            computers::add(args, state).await
        }
        "rename" if state.cwd.section == VfsSection::Computers => {
            computers::rename(args, state).await
        }
        "add" | "rename" => {
            eprintln!("'{}' is only valid in /Computers — use 'cd /Computers' first", cmd);
            Ok(())
        }
        "settings" => settings::settings_cmd(args, state).await,
        "clear" => {
            eprint!("\x1b[2J\x1b[H");
            Ok(())
        }
        "exit" | "quit" => {
            state.should_quit = true;
            Ok(())
        }
        "help" => {
            let topic = args.first().map(|s| s.as_str());
            print_help(topic);
            Ok(())
        }
        other => {
            eprintln!("Unknown command: '{}'. Type 'help' for a full list.", other);
            Ok(())
        }
    }
}

fn print_help(topic: Option<&str>) {
    use console::style;
    match topic {
        Some("ls") => eprintln!("\
{hdr}
  ls [pattern]

Lists the contents of the current directory. Optional glob pattern filters results.
Sections: MyFiles, Photos show indexed file trees; Trash lists trashed items; Computers lists devices.
Examples:
  ls              list all items
  ls '*.jpeg'     list JPEG files only
  ls Docs*        list items starting with 'Docs'", hdr=style("ls — list directory contents").bold()),

        Some("cd") => eprintln!("\
{hdr}
  cd [path]

Changes the current directory. Accepts absolute paths (/MyFiles, /Trash, /Computers, /Photos),
relative paths, ~ for MyFiles root, and .. to go up one level.
Within /Computers and /Trash, sub-navigation is not supported.
Examples:
  cd Documents     navigate into Documents folder
  cd ~/Pictures    absolute path to Pictures in MyFiles
  cd /Photos       switch to Photos section
  cd ..            go up one level", hdr=style("cd — change directory").bold()),

        Some("get") => eprintln!("\
{hdr}
  get <remote> [local-path]

Downloads a file from Proton Drive to the local filesystem.
Shows a download progress bar (white fill on purple backing).
Examples:
  get report.pdf           download to ./report.pdf
  get '~/Docs/notes.pdf' ~/Downloads/notes.pdf", hdr=style("get — download a file").bold()),

        Some("put") => eprintln!("\
{hdr}
  put <local-path> [name]

Uploads a local file into the current Drive directory.
Shows an upload progress bar (purple fill on white backing).
Examples:
  put ~/photo.jpg          upload to current folder
  put ~/img.png banner.png upload with a new name", hdr=style("put — upload a file").bold()),

        Some("rev") => eprintln!("\
{hdr}
  rev ls [file]            open interactive revision pager for a file
  rev restore <uid>        restore a specific revision by UID
  rev delete <uid>         permanently delete a revision by UID

Interactive pager controls:
  ↑/↓   navigate items
  Enter  restore selected superseded revision
  D      delete selected revision
  Esc/q  exit pager

Only superseded revisions can be restored or deleted — the active revision is protected.", hdr=style("rev — file revision management").bold()),

        Some("stat") => eprintln!("\
{hdr}
  stat [-s] [path]

Shows metadata for a file or folder: name, kind, size, last modified, MIME type.
Use -s/--sensitive to also print the UID and parent UID.
Examples:
  stat            show metadata for current directory
  stat photo.jpg  show metadata for a file
  stat -s file    include UID fields", hdr=style("stat — show file metadata").bold()),

        Some("settings") => eprintln!("\
{hdr}
  settings [show]              display current configuration
  settings set <key> <value>   update a setting (persisted to disk)

Keys:
  entity_cache_max_size   number (or 'unlimited') — LRU cap for the entity SQLite cache
  secret_cache_max_size   number (or 'unlimited') — LRU cap for the secret SQLite cache", hdr=style("settings — view and change configuration").bold()),

        Some("drop") => eprintln!("\
{hdr}
  drop [pattern] [-f]

Permanently deletes items from the trash. Must be in /Trash or use rm when in /Trash.
Without -f/--force, prompts for confirmation.
Examples:
  drop            delete all trash items (with prompt)
  drop '*.tmp' -f delete all .tmp items without prompting", hdr=style("drop — permanently delete from trash").bold()),

        Some("restore") => eprintln!("\
{hdr}
  restore [pattern]

Restores trashed items back to their original location. Must be in /Trash.
Examples:
  restore              restore all trashed items
  restore 'report*'    restore items matching the pattern", hdr=style("restore — restore from trash").bold()),

        _ => eprintln!("\
{hdr}

Navigation:
  pwd                     print working directory
  ls [pattern]            list directory contents (glob pattern supported)
  cd <path>               change directory  (~, /, /Trash, /Computers, /Photos, ..)

File operations:
  mkdir <name>            create folder (or photo album in /Photos)
  mv <src> <dst>          move a file or folder
  cp <src> <dst>          copy a file or folder
  rm <pattern>            send to trash (or delete in /Trash, or remove device in /Computers)
  get <remote> [local]    download a file  (purple/white progress bar)
  put <local> [name]      upload a file    (purple/white progress bar)
  touch <name>            create an empty file
  stat [-s] <path>        show metadata    (-s includes UID fields)

Revisions:
  rev ls [file]           interactive pager  (↑↓ navigate, Enter=restore, D=delete, Esc=exit)
  rev restore <uid>       restore a revision directly
  rev delete <uid>        delete a revision directly

Trash  (navigate to /Trash first):
  drop [pattern] [-f]     permanently delete trash items
  restore [pattern]       restore trash items to original location

Computers  (navigate to /Computers first):
  sync                    list registered backup devices
  add <name>              register this machine as a Linux backup device
  rename <old> <new>      rename a device
  rm [-f] <name>          remove a device

Account & config:
  whoami                  show authenticated username
  logout [-c]             log out  (-c also clears credentials and cache)
  settings [set ...]      view or change configuration
  help [command]          show this help, or detailed help for a command
  exit / quit             exit the REPL

Tip: Tab-completes file and folder names in MyFiles and Photos.", hdr=style("pdcli — Proton Drive CLI").bold().underlined()),
    }
}
