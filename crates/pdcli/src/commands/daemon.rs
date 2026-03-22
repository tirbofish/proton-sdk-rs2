use anyhow::{anyhow, Result};
use std::sync::Arc;
use parking_lot::Mutex;
use crate::state::ReplState;
use crate::daemon;

pub async fn daemon_command(args: &[&str], _state: &Arc<Mutex<ReplState>>) -> Result<()> {
    let sub = args.first().copied().unwrap_or("status");
    match sub {
        "init" | "start" => daemon::daemon_start(),
        "stop" => daemon::daemon_stop(),
        "restart" => {
            daemon::daemon_stop()?;
            // Give the old process a moment to release the socket.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            daemon::daemon_start()
        }
        "status" => daemon::daemon_status(),
        "help" => {
            println!(
                r#"
daemon — manage the pdcli background daemon

  daemon init       Start the daemon (continuous computer sync)
  daemon stop       Stop the running daemon
  daemon restart    Restart the daemon
  daemon status     Show whether the daemon is running
"#
            );
            Ok(())
        }
        other => Err(anyhow!(
            "Unknown daemon subcommand '{other}'. Use: init | stop | restart | status"
        )),
    }
}
