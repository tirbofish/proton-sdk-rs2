#[derive(Default)]
// clap too bulky, this just developer flags
pub struct ClientFlags {
    /// Forces offline mode.
    ///
    /// Primarily used for testing on the developer side, can be enabled by the user to check if their files are offline supported.
    ///
    /// Enabled with the `--force-offline` flag.
    pub force_offline: bool,

    /// Runs the background daemon instead of the GUI.
    pub daemon: bool,

    /// Disables the daemon-owned tray icon.
    pub no_tray: bool,

    /// Initial GUI page to open.
    pub page: Option<String>,
}

impl ClientFlags {
    pub fn apply_flags(&mut self) {
        let args = std::env::args().collect::<Vec<_>>();

        self.force_offline = check_flag(&args, "--force-offline");
        self.daemon = check_flag(&args, "--daemon");
        self.no_tray = check_flag(&args, "--no-tray");
        self.page = value_after(&args, "--page");
    }
}

fn check_flag(args: &[String], flag: impl Into<String>) -> bool {
    let flag = flag.into();
    let is = args.contains(&flag);
    if is {
        tracing::info!(flag = %flag, "flag has been enabled");
    }
    is
}

fn value_after(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|window| window[0] == flag)
        .map(|window| window[1].clone())
}
