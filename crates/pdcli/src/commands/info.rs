use crate::app::AppState;
use crate::vfs::VirtualPath;

pub async fn whoami(state: &AppState) -> anyhow::Result<()> {
    println!("{}", state.session.username);
    Ok(())
}

pub async fn logout(args: &[String], state: &mut AppState) -> anyhow::Result<()> {
    let clear_all = args.iter().any(|a| a == "--clear-everything" || a == "-c");
    if clear_all {
        crate::auth::clear_all_data()?;
        println!("Logged out and cleared all local data");
    } else {
        crate::auth::clear_credentials()?;
        println!("Logged out");
    }
    state.should_quit = true;
    Ok(())
}

pub async fn stat(args: &[String], state: &AppState) -> anyhow::Result<()> {
    let sensitive = args.iter().any(|a| a == "--sensitive" || a == "-s");
    let path_arg = args.iter().find(|a| !a.starts_with('-')).map(|s| s.as_str());

    let target = match path_arg {
        Some(p) => VirtualPath::resolve(&state.cwd, p),
        None => state.cwd.clone(),
    };

    let pb = crate::ui::spinner(format!("stat {}…", target));
    let uid = state.resolve_uid(&target).await;
    pb.finish_and_clear();
    let uid = uid?.ok_or_else(|| anyhow::anyhow!("Not found: {}", target))?;

    let entry = state
        .index
        .get(&uid)
        .ok_or_else(|| anyhow::anyhow!("Node not in index"))?;

    println!("Name:     {}", entry.name);
    println!("Kind:     {}", if entry.is_folder { "folder" } else { "file" });
    if let Some(size) = entry.size {
        println!("Size:     {} bytes ({})", size, format_size(size));
    }
    if let Some(mtime) = entry.modification_time {
        println!("Modified: {}", mtime.format("%Y-%m-%d %H:%M:%S UTC"));
    }
    if let Some(mt) = &entry.media_type {
        println!("MIME:     {}", mt);
    }
    if sensitive {
        println!("UID:      {}", entry.uid);
        if let Some(p) = &entry.parent_uid {
            println!("Parent:   {}", p);
        }
    }
    Ok(())
}

fn format_size(bytes: i64) -> String {
    let b = bytes as f64;
    if b < 1024.0 {
        format!("{b:.0} B")
    } else if b < 1024.0 * 1024.0 {
        format!("{:.1} KB", b / 1024.0)
    } else if b < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", b / (1024.0 * 1024.0 * 1024.0))
    }
}
