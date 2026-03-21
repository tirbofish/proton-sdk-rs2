use crate::state::ReplState;
use anyhow::{anyhow, Result};
use proton_drive_sdk::node::NodeUid;
use proton_drive_sdk::links::LinkId;
use proton_drive_sdk::photo::ProtonPhotosClient;
use std::sync::Arc;
use parking_lot::Mutex;

use super::helpers::{new_spinner, progress_bar_for, progress_callback};

pub async fn photos_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    let sub = args.first().copied().unwrap_or("ls");
    match sub {
        "ls" | "list" => photos_list(&args[args.len().min(1)..], state).await,
        "get" | "download" => photos_get(&args[1..], state).await,
        "help" => {
            print_photos_help();
            Ok(())
        }
        _ => Err(anyhow!(
            "Unknown photos subcommand '{}'. Use 'photos help'.",
            sub
        )),
    }
}

async fn photos_list(_args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    let photos = build_photos_client(state)?;

    let sp = new_spinner("Fetching photo timeline...");
    let items = photos.iterate_timeline().await?;
    sp.finish_and_clear();

    if items.is_empty() {
        println!("\n  No photos in library.\n");
        return Ok(());
    }

    // Show a compact summary: oldest → newest capture times, total count.
    let oldest = items.iter().min_by_key(|i| i.capture_time);
    let newest = items.iter().max_by_key(|i| i.capture_time);

    println!();
    println!("  Photos library: {} item(s)", items.len());
    if let (Some(o), Some(n)) = (oldest, newest) {
        println!(
            "  Range: {} — {}",
            o.capture_time.format("%Y-%m-%d"),
            n.capture_time.format("%Y-%m-%d"),
        );
    }
    println!();

    // Show up to 20 most-recent items.
    let display_limit = 20;
    let start = items.len().saturating_sub(display_limit);
    let recent: Vec<_> = items[start..].iter().rev().collect();
    println!(
        "  {:38}  {}",
        "Link ID", "Captured"
    );
    println!("  {}  {}", "-".repeat(38), "-".repeat(19));
    for item in &recent {
        println!(
            "  {:38}  {}",
            item.uid.link_id.raw(),
            item.capture_time.format("%Y-%m-%d %H:%M UTC"),
        );
    }
    if items.len() > display_limit {
        println!("  … and {} more (oldest items omitted)", items.len() - display_limit);
    }
    println!();
    Ok(())
}

async fn photos_get(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.len() < 2 {
        return Err(anyhow!("Usage: photos get <link_id> <local_path>"));
    }
    let link_id_str = args[0];
    let local_path = std::path::Path::new(args[1]);

    let photos = build_photos_client(state)?;

    // Resolve the volume ID so we can construct a NodeUid.
    let sp = new_spinner("Resolving photos volume...");
    let volume_id = photos.get_photos_volume_id().await?;
    sp.finish_and_clear();

    let uid = NodeUid::new(volume_id, LinkId::new(link_id_str.to_string()));

    let node_result = photos.get_node(uid.clone()).await?;
    let node = match node_result {
        proton_drive_sdk::utils::PotentialObject::Node(n) => n,
        proton_drive_sdk::utils::PotentialObject::Degraded(_) => {
            return Err(anyhow!("Could not decrypt node '{}'", link_id_str));
        }
    };

    let name = node.base().name.clone();
    let size = match &node {
        proton_drive_sdk::node::Node::File(f) | proton_drive_sdk::node::Node::Photo(f) => {
            f.active_revision.size_on_cloud_storage.max(0) as u64
        }
        _ => return Err(anyhow!("'{}' is a folder, not a photo/file", link_id_str)),
    };

    let dest = if local_path.is_dir() {
        local_path.join(&name)
    } else {
        local_path.to_path_buf()
    };

    let pb = progress_bar_for(&name, size);
    photos
        .drive()
        .download_to_file(uid, &dest, progress_callback(pb.clone()))
        .await?;
    pb.finish_and_clear();

    println!("Downloaded '{}' → {}", name, dest.display());
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn build_photos_client(state: &Arc<Mutex<ReplState>>) -> Result<ProtonPhotosClient> {
    let s = state.lock();
    let session = s
        .get_session()
        .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?;
    // The uid is the same as the drive client's uid (ProtonUser ID).
    ProtonPhotosClient::new(session, None)
        .map_err(|e| anyhow!("Failed to build photos client: {e}"))
}

fn print_photos_help() {
    println!(
        r#"
COMMAND: photos [subcommand]

Browse and download from your Proton Drive photo library.

SUBCOMMANDS:
  ls                           Show a summary of the photo timeline
  get <link_id> <local_path>   Download a photo by its link ID

EXAMPLES:
  photos ls
  photos get abc123def456 ~/Downloads/photo.jpg
"#
    );
}
