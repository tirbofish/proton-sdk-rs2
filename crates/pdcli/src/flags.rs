use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "pdcli",
    version,
    about = "Proton Drive client",
    after_help = "On WSL, pdcli defaults to `mount`. Use `pdcli gui` for the window."
)]
pub struct Cli {
    /// Skip network calls and use the local cache only
    #[arg(long, global = true)]
    pub force_offline: bool,

    /// Do not show a tray icon (always on WSL)
    #[arg(long, global = true)]
    pub no_tray: bool,

    /// Open the graphical app
    #[arg(long)]
    pub gui: bool,

    #[arg(long, hide = true)]
    pub daemon: bool,

    #[arg(long, hide = true)]
    pub cli: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Open the graphical app
    Gui {
        /// Initial page: status, computers, mount, about, account, settings
        #[arg(long)]
        page: Option<String>,
    },
    /// Sign in through the browser
    Login,
    /// Sign out, unmount, and stop the daemon
    Logout,
    /// Show login, daemon, mount, and journal status
    Status {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Retry a failed journal entry, or all failed entries
    Retry {
        /// Failed journal entry ID
        #[arg(
            value_name = "ID",
            conflicts_with = "all",
            required_unless_present = "all"
        )]
        id: Option<i64>,
        /// Retry every failed journal entry
        #[arg(long)]
        all: bool,
    },
    /// Sign in if needed and mount ~/ProtonDrive
    Mount,
    /// Unmount and stop the daemon
    #[command(alias = "unmount")]
    Stop,
    /// Pause background sync
    Pause,
    /// Resume background sync
    Resume,
    /// Retry sync immediately
    Sync,
    /// Open the Proton Drive folder
    Open,
    /// Manage public links and sharing for a node UID
    Share {
        #[command(subcommand)]
        command: ShareCommand,
    },
    /// Export My Files to a local directory with a resumable manifest
    Takeout { destination: std::path::PathBuf },
    /// Manage the systemd user service
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// List computers, register this machine, or manage folder sync jobs
    Computers {
        #[command(subcommand)]
        command: Option<ComputersCommand>,
    },
    /// Run the background daemon
    #[command(hide = true)]
    Daemon,
}

#[derive(Subcommand)]
pub enum ComputersCommand {
    /// Register this machine, or bind it to an existing computer
    Register {
        /// Display name (defaults to hostname)
        #[arg(long)]
        name: Option<String>,
        /// Bind to an existing computer id
        #[arg(long)]
        bind: Option<String>,
    },
    /// Back up a local folder to this computer and keep it in sync
    Sync {
        path: std::path::PathBuf,
        /// Remote folder name (defaults to the local directory name)
        #[arg(long)]
        name: Option<String>,
        /// Validate and preview without creating a device, folder, or sync job
        #[arg(long)]
        dry_run: bool,
    },
    /// Restore a computer folder to a local path and keep syncing it there
    Restore {
        computer: String,
        folder: String,
        path: std::path::PathBuf,
    },
    /// Stop a sync job without deleting local or cloud files
    Unsync { job: String },
}

#[derive(Subcommand)]
pub enum ShareCommand {
    /// Create a public link for a node UID
    Link {
        node: String,
        /// Public-link role: viewer or editor
        #[arg(long, default_value = "viewer")]
        role: String,
        /// Optional custom public-link password
        #[arg(long)]
        password: Option<String>,
    },
    /// Show members and public-link status for a node UID
    Status {
        node: String,
        #[arg(long)]
        json: bool,
    },
    /// Remove the public link for a node UID
    Remove { node: String },
    /// Report a shared node for abuse
    Report {
        node: String,
        /// Abuse category: spam, copyright, child-abuse, stolen-data, malware, non-consensual-intimate, other
        #[arg(long, short = 'c')]
        category: String,
        /// Message about the report. Required for copyright and stolen-data
        #[arg(long, short = 'm')]
        message: Option<String>,
        /// Reporter email. Optional; the signed-in address is used by default
        #[arg(long, short = 'e')]
        email: Option<String>,
        /// Confirm the report is submitted in good faith (required)
        #[arg(long)]
        bona_fide: bool,
        /// UID of a specific revision to report
        #[arg(long, short = 'r')]
        revision: Option<String>,
        /// UID of a pending invitation to report before accepting it
        #[arg(long, short = 'i')]
        invitation: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum ServiceCommand {
    /// Install a user service for this pdcli executable
    Install,
    /// Remove the source-installed user service
    Uninstall,
    /// Install, enable, and start the user service
    Enable,
    /// Stop and disable the user service
    Disable,
    /// Reload systemd's user-unit configuration
    Reload,
    /// Show the user service status
    Status,
}

#[derive(Clone, Default)]
pub struct ClientFlags {
    pub force_offline: bool,
    pub no_tray: bool,
    pub page: Option<String>,
}

impl Cli {
    pub fn client_flags(&self, page: Option<String>) -> ClientFlags {
        ClientFlags {
            force_offline: self.force_offline,
            no_tray: self.no_tray || is_wsl(),
            page,
        }
    }
}

pub fn is_wsl() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::fs::read_to_string("/proc/version")
            .map(|v| v.to_ascii_lowercase().contains("microsoft"))
            .unwrap_or(false)
}
