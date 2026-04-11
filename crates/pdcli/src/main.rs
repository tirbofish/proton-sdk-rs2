pub mod auth;
pub mod cancellation;
pub mod commands;

use clap::{Parser, Subcommand};
use console::style;
use std::path::PathBuf;
use tokio::select;

use crate::auth::ProtonAuth;
use crate::cancellation::{CancellationStack, CancellationGuard, spawn_ctrlc_handler};

#[derive(Parser)]
#[command(name = "pdcli")]
#[command(about = "Proton Drive CLI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
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
    async fn execute_command(&self, command: Commands, _guard: &CancellationGuard) -> anyhow::Result<()> {
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
            Commands::Mount { path: _ } => {
                // Ensure authenticated before mounting
                if !self.auth.ensure_authenticated().await? {
                    println!("{}", style("Authentication required. Exiting.").red());
                    return Ok(());
                }
                // TODO: Implement mount
                println!("{}", style("Mount command not yet implemented.").dim());
                Ok(())
            }
        }
    }
}

#[tokio::main]
pub async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let pdcli = ProtonDriveCommandLineInterface::new()?;
    
    // Spawn the Ctrl+C handler
    let _ctrlc_handle = spawn_ctrlc_handler(pdcli.cancellation().clone());
    
    // Get the root token to know when to exit
    let root = pdcli.cancellation().root().await;
    
    // Run the command with cancellation support
    select! {
        result = pdcli.run(cli.command) => {
            result
        }
        _ = root.cancelled() => {
            println!("Application shutdown requested");
            Ok(())
        }
    }
}
