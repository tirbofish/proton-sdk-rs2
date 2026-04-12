pub mod auth;
pub mod cancellation;
pub mod commands;

use anyhow::Context;
use clap::{Parser, Subcommand};
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::select;

use crate::auth::ProtonAuth;
use crate::cancellation::{CancellationStack, CancellationGuard, spawn_ctrlc_handler};

#[derive(Parser)]
#[command(name = "pdcli")]
#[command(about = "Proton Drive CLI", long_about = None)]
struct Cli {
    /// Clear the file download cache (keeps session)
    #[arg(long)]
    clear_cache: bool,

    /// Clear pending uploads from queue
    #[arg(long)]
    clear_pending_uploads: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Login to Proton Drive
    Login,
    /// Logout from Proton Drive
    Logout {
        /// Clear all stored data (session, cache, etc.)
        #[arg(long)]
        clear: bool,
    },
    /// Show current account information
    Whoami,
    /// Mount Proton Drive to a local path
    Mount {
        /// Path to mount the drive
        path: PathBuf,
    },
}

/// The main CLI application with hierarchical cancellation support.
/// 
/// Each Ctrl+C cancels the innermost active operation first.
/// Subsequent Ctrl+C presses cancel progressively outer operations
/// until the entire application exits.
pub struct ProtonDriveCommandLineInterface {
    /// The cancellation stack for hierarchical Ctrl+C handling
    cancellation: CancellationStack,
    /// Authentication handler
    auth: ProtonAuth,
}

impl ProtonDriveCommandLineInterface {
    /// Create a new CLI instance with cancellation support.
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            cancellation: CancellationStack::new(),
            auth: ProtonAuth::new()?,
        })
    }

    /// Get the cancellation stack for spawning the Ctrl+C handler.
    pub fn cancellation(&self) -> &CancellationStack {
        &self.cancellation
    }

    /// Get the auth handler.
    pub fn auth(&self) -> &ProtonAuth {
        &self.auth
    }

    /// Push a new cancellation context for a nested operation.
    /// The returned guard will automatically clean up when dropped.
    pub async fn push_context(&self) -> CancellationGuard {
        self.cancellation.push().await
    }

    /// Run a command with cancellation support.
    pub(crate) async fn run(&self, command: Commands) -> anyhow::Result<()> {
        // Create a cancellation context for this command
        let guard = self.push_context().await;
        
        select! {
            result = self.execute_command(command, &guard) => result,
            _ = guard.cancelled() => {
                tracing::info!("Command cancelled by user");
                Ok(())
            }
        }
    }

    /// Execute a command within a cancellation context.
    async fn execute_command(&self, command: Commands, guard: &CancellationGuard) -> anyhow::Result<()> {
        match command {
            Commands::Login => {
                self.auth.login_interactive().await
            }
            Commands::Logout { clear } => {
                if !self.auth.has_stored_session() {
                    println!("{}", style("You are not logged in.").yellow());
                    return Ok(());
                }
                if clear {
                    self.auth.logout_clear().await
                } else {
                    self.auth.logout().await
                }
            }
            Commands::Whoami => {
                match self.auth.get_account_info() {
                    Some(info) => {
                        println!();
                        println!("  {} {}", style("Username:").bold(), style(&info.username).cyan());
                        println!("  {}  {}", style("User ID:").bold(), style(&info.user_id).dim());
                        println!(" {} {}", style("Session:").bold(), style(&info.session_id).dim());
                        println!();
                    }
                    None => {
                        println!("{}", style("Not logged in.").yellow());
                    }
                }
                Ok(())
            }
            Commands::Mount { path } => {
                // Ensure authenticated before mounting
                if !self.auth.ensure_authenticated().await? {
                    println!("{}", style("Authentication required. Exiting.").red());
                    return Ok(());
                }
                
                // Handle mount path - may need to create or clean up stale mount
                // Try to stat the directory - if it fails with EIO/ENOTCONN, it's a stale mount
                let path_status = std::fs::metadata(&path);
                match &path_status {
                    Ok(meta) if meta.is_dir() => {
                        // Good to go
                    }
                    Ok(_) => {
                        anyhow::bail!("Mount path is not a directory: {}", path.display());
                    }
                    Err(e) if e.raw_os_error() == Some(libc::ENOTCONN) || 
                              e.raw_os_error() == Some(libc::EIO) => {
                        // Transport endpoint not connected - stale FUSE mount
                        println!("{}", style("Cleaning up stale mount...").yellow());
                        let _ = std::process::Command::new("fusermount3")
                            .args(["-u", "-z"])
                            .arg(&path)
                            .output();
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // Create the directory
                        std::fs::create_dir_all(&path)
                            .with_context(|| format!("Failed to create mount directory: {}", path.display()))?;
                        println!("{}", style(format!("Created mount directory: {}", path.display())).dim());
                    }
                    Err(e) => {
                        anyhow::bail!("Cannot access mount path {}: {}", path.display(), e);
                    }
                }
                
                // Verify path is now accessible
                if !path.is_dir() {
                    anyhow::bail!("Mount path is not a directory: {}", path.display());
                }
                
                // Create spinner for mount progress
                let spinner = Arc::new(ProgressBar::new_spinner());
                spinner.set_style(
                    ProgressStyle::default_spinner()
                        .template("{spinner:.cyan} {msg}")
                        .unwrap()
                        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
                );
                spinner.enable_steady_tick(std::time::Duration::from_millis(80));
                spinner.set_message("Initializing...");
                
                let spinner_for_progress = spinner.clone();
                let progress: crate::commands::mount::ProgressCallback = Box::new(move |msg| {
                    spinner_for_progress.set_message(msg.to_string());
                });
                
                // Channel to signal when mount is ready
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                let spinner_for_ready = spinner.clone();
                let path_display = path.display().to_string();
                
                // Spawn task to wait for ready signal and show success
                tokio::spawn(async move {
                    if ready_rx.await.is_ok() {
                        spinner_for_ready.finish_with_message(format!(
                            "{} Mounted at {}",
                            style("✓").green().bold(),
                            style(&path_display).cyan()
                        ));
                        println!("{}", style("Press Ctrl+C to unmount").dim());
                    }
                });
                
                // Try to mount - may fail if keys are not unlocked
                let mount_result = {
                    let session_guard = self.auth.session().await
                        .ok_or_else(|| anyhow::anyhow!("No session after authentication"))?;
                    let session = session_guard.as_ref()
                        .ok_or_else(|| anyhow::anyhow!("Session is None in guard"))?;
                    
                    crate::commands::mount::mount(&path, session, guard.token(), Some(progress), Some(ready_tx)).await
                };
                
                // Check if it failed due to locked keys
                match &mount_result {
                    Err(e) => {
                        spinner.finish_and_clear();
                        let err_str = format!("{:?}", e);
                        if err_str.contains("none could be unlocked") || 
                           err_str.contains("passphrase") ||
                           err_str.contains("Unable to locate passphrase") {
                            // Keys not unlocked - prompt for password and retry
                            println!();
                            println!("{}", style("Keys need to be unlocked.").yellow());
                            
                            if !self.auth.unlock_keys_with_password().await? {
                                println!("{}", style("Failed to unlock keys. Exiting.").red());
                                return Ok(());
                            }
                            
                            // New spinner for retry
                            let spinner = Arc::new(ProgressBar::new_spinner());
                            spinner.set_style(
                                ProgressStyle::default_spinner()
                                    .template("{spinner:.cyan} {msg}")
                                    .unwrap()
                                    .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
                            );
                            spinner.enable_steady_tick(std::time::Duration::from_millis(80));
                            spinner.set_message("Retrying mount...");
                            
                            let spinner_for_progress = spinner.clone();
                            let progress: crate::commands::mount::ProgressCallback = Box::new(move |msg| {
                                spinner_for_progress.set_message(msg.to_string());
                            });
                            
                            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                            let spinner_for_ready = spinner.clone();
                            let path_display = path.display().to_string();
                            
                            tokio::spawn(async move {
                                if ready_rx.await.is_ok() {
                                    spinner_for_ready.finish_with_message(format!(
                                        "{} Mounted at {}",
                                        style("✓").green().bold(),
                                        style(&path_display).cyan()
                                    ));
                                    println!("{}", style("Press Ctrl+C to unmount").dim());
                                }
                            });
                            
                            // Retry mount with unlocked keys
                            let session_guard = self.auth.session().await
                                .ok_or_else(|| anyhow::anyhow!("No session"))?;
                            let session = session_guard.as_ref()
                                .ok_or_else(|| anyhow::anyhow!("Session is None"))?;
                            
                            crate::commands::mount::mount(&path, session, guard.token(), Some(progress), Some(ready_tx)).await
                        } else {
                            mount_result
                        }
                    }
                    Ok(()) => Ok(())
                }
            }
        }
    }
}

#[tokio::main]
pub async fn main() -> anyhow::Result<()> {
    // Configure tracing - only enabled via RUST_LOG env var
    // Example: RUST_LOG=info pdcli mount ~/ProtonDrive
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    if let Ok(filter) = EnvFilter::try_from_default_env() {
        tracing_subscriber::registry()
            .with(fmt::layer())
            .with(filter)
            .try_init()
            .ok();
    }

    let cli = Cli::parse();

    // Handle standalone flags first
    if cli.clear_cache {
        return crate::commands::mount::clear_cache();
    }
    if cli.clear_pending_uploads {
        return crate::commands::mount::clear_pending_uploads();
    }

    // Require a subcommand if no flags given
    let Some(command) = cli.command else {
        use clap::CommandFactory;
        Cli::command().print_help()?;
        return Ok(());
    };

    let pdcli = ProtonDriveCommandLineInterface::new()?;
    
    // Spawn the Ctrl+C handler
    let _ctrlc_handle = spawn_ctrlc_handler(pdcli.cancellation().clone());
    
    // Get the root token to know when to exit
    let root = pdcli.cancellation().root().await;
    
    // Run the command with cancellation support
    select! {
        result = pdcli.run(command) => {
            result
        }
        _ = root.cancelled() => {
            println!("Application shutdown requested");
            Ok(())
        }
    }
}
