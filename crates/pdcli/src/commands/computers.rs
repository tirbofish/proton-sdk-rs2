use crate::state::ReplState;
use anyhow::{anyhow, Result};
use proton_drive_sdk::api::devices::DeviceType;
use std::sync::Arc;
use parking_lot::Mutex;

use super::helpers::new_spinner;

pub async fn computers_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    let sub = args.first().copied().unwrap_or("ls");
    match sub {
        "ls" | "list" => computers_list(&args[args.len().min(1)..], state).await,
        "add" | "create" => computers_add(&args[1..], state).await,
        "rename" | "mv" => computers_rename(&args[1..], state).await,
        "rm" | "delete" | "remove" => computers_rm(&args[1..], state).await,
        "help" => {
            print_computers_help();
            Ok(())
        }
        _ => Err(anyhow!(
            "Unknown computers subcommand '{}'. Use 'computers help'.",
            sub
        )),
    }
}

async fn computers_list(_args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    let client = {
        let s = state.lock();
        s.get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone()
    };

    let sp = new_spinner("Fetching computers...");
    let devices = client.list_devices().await?;
    sp.finish_and_clear();

    if devices.is_empty() {
        println!("\n  No computers registered.\n");
        return Ok(());
    }

    println!();
    println!("  {:30}  {:8}  {:26}  {}", "Name", "Type", "Created", "ID");
    println!("  {}  {}  {}  {}", "-".repeat(30), "-".repeat(8), "-".repeat(26), "-".repeat(36));
    for d in &devices {
        let type_str = device_type_label(d.device_type);
        let created = d.create_time.format("%Y-%m-%d %H:%M UTC").to_string();
        println!(
            "  {:30}  {:8}  {:26}  {}",
            truncate(&d.name, 30),
            type_str,
            created,
            d.device_id,
        );
    }
    println!("\n  {} computer(s)\n", devices.len());
    Ok(())
}

async fn computers_add(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: computers add <name> [windows|macos|linux]"));
    }
    let name = args[0].to_string();
    let device_type = if let Some(t) = args.get(1) {
        parse_device_type(t)?
    } else {
        DeviceType::Linux
    };

    let client = {
        let s = state.lock();
        s.get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone()
    };

    let sp = new_spinner(format!("Registering computer '{}'...", name));
    let device = client.create_device(name.clone(), device_type).await?;
    sp.finish_and_clear();

    println!(
        "Computer '{}' registered (ID: {}).",
        device.name, device.device_id
    );
    Ok(())
}

async fn computers_rename(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.len() < 2 {
        return Err(anyhow!("Usage: computers rename <device_id> <new_name>"));
    }
    let device_id = args[0];
    let new_name = args[1].to_string();

    let client = {
        let s = state.lock();
        s.get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone()
    };

    let sp = new_spinner(format!("Renaming computer '{}'...", device_id));
    let device = client.rename_device(device_id, new_name).await?;
    sp.finish_and_clear();

    println!("Computer renamed to '{}'.", device.name);
    Ok(())
}

async fn computers_rm(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: computers rm <device_id>"));
    }
    let device_id = args[0];

    // Resolve the name for a friendlier confirmation message.
    let (client, name) = {
        let s = state.lock();
        let client = s
            .get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone();
        (client, device_id.to_string())
    };

    let confirmed = dialoguer::Confirm::new()
        .with_prompt(format!("Unregister computer '{}'?", name))
        .default(false)
        .interact()?;

    if !confirmed {
        println!("Aborted.");
        return Ok(());
    }

    let sp = new_spinner("Unregistering computer...");
    client.delete_device(device_id).await?;
    sp.finish_and_clear();
    println!("Computer '{}' unregistered.", name);
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn device_type_label(t: DeviceType) -> &'static str {
    match t {
        DeviceType::Windows => "Windows",
        DeviceType::MacOS => "macOS",
        DeviceType::Linux => "Linux",
    }
}

fn parse_device_type(s: &str) -> Result<DeviceType> {
    match s.to_lowercase().as_str() {
        "windows" => Ok(DeviceType::Windows),
        "macos" | "mac" => Ok(DeviceType::MacOS),
        "linux" => Ok(DeviceType::Linux),
        other => Err(anyhow!(
            "Unknown device type '{}'. Use windows, macos, or linux.",
            other
        )),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}

fn print_computers_help() {
    println!(
        r#"
COMMAND: computers [subcommand]

Manage registered Computers (backup devices) for this account.

SUBCOMMANDS:
  ls                       List all registered computers
  add <name> [type]        Register a new computer (type: windows, macos, linux)
  rename <id> <new_name>   Rename an existing computer
  rm <id>                  Unregister a computer

EXAMPLES:
  computers ls
  computers add "My Server" linux
  computers rename abc123 "Workstation"
  computers rm abc123
"#
    );
}
