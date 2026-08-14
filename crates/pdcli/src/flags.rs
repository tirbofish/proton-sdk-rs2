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
    /// Show login and daemon status
    Status,
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
    /// Run the background daemon
    #[command(hide = true)]
    Daemon,
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
