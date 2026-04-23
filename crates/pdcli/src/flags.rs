#[derive(Default)]
// clap too bulky, this just developer flags
pub struct ClientFlags {
    /// Forces offline mode. 
    /// 
    /// Primarily used for testing on the developer side, can be enabled by the user to check if their files are offline supported. 
    /// 
    /// Enabled with the `--force-offline` flag. 
    pub force_offline: bool,
}

impl ClientFlags {
    pub fn apply_flags(&mut self) {
        let args = std::env::args().collect::<Vec<_>>();

        self.force_offline = check_flag(&args, "--force-offline");
    }
}

fn check_flag(args: &Vec<String>, flag: impl Into<String>) -> bool {
    let flag = flag.into();
    let is = args.contains(&flag);
    tracing::info!(flag = %flag, "flag has been enabled");
    is
}