use std::path::Path;

use anyhow::{Context, Result};
use console::style;

use super::thumbnail::get_thumbnail_config;

/// Check if a process is a known thumbnailer/preview process.
pub(super) fn is_thumbnailer_process(proc_name: &str, pid: u32) -> bool {
    let config = get_thumbnail_config();
    let name = proc_name.to_lowercase();

    if name.contains("thumbnailer") || name.contains("thumbnail") {
        tracing::debug!("Blocking thumbnailer process: {}", proc_name);
        return true;
    }

    const THUMBNAILER_NAMES: &[&str] = &[
        "gnome-desktop-thu",
        "gdk-pixbuf-thumbn",
        "gdk-pixbuf-thumb",
        "evince-thumbnaile",
        "papers-thumbnaile",
        "totem-video-thumb",
        "gs-thumbnailer",
        "raw-thumbnailer",
        "tumbler",
        "tumblerd",
        "tracker-extract",
        "tracker-miner-fs",
        "ffmpegthumbnailer",
        "kio_thumbnail",
    ];

    for pattern in THUMBNAILER_NAMES {
        if name == *pattern || name.starts_with(pattern) {
            tracing::debug!("Blocking known thumbnailer: {} (matched {})", proc_name, pattern);
            return true;
        }
    }

    if name.starts_with("pool-") && pid > 0 {
        let (tgid, tg_leader_name, tg_leader_exe) = get_thread_group_leader_with_exe(pid);
        tracing::debug!(
            "Pool thread detected: proc={}, pid={}, tgid={:?}, leader={:?}, exe={:?}",
            proc_name,
            pid,
            tgid,
            tg_leader_name,
            tg_leader_exe
        );

        if let Some(ref exe) = tg_leader_exe {
            let exe_lower = exe.to_lowercase();
            if exe_lower.contains("thumbnailer") || exe_lower.contains("thumbnail") {
                tracing::debug!("Pool thread: exe {} is thumbnailer, blocking", exe);
                return true;
            }

            match config.is_exe_allowed(exe) {
                Some(true) => {
                    tracing::debug!("Pool thread: exe {} allowed by config", exe);
                    return false;
                }
                Some(false) => {
                    tracing::debug!("Pool thread: exe {} blocked by config", exe);
                    return true;
                }
                None => {}
            }
        }

        if let Some(ref leader) = tg_leader_name {
            let leader_lower = leader.to_lowercase();
            if leader_lower.contains("thumbnailer") || leader_lower.contains("thumbnail") {
                tracing::debug!("Pool thread: leader {} is thumbnailer, blocking", leader);
                return true;
            }

            match config.is_name_allowed(leader) {
                Some(true) => {
                    tracing::debug!("Pool thread: leader {} allowed by config", leader);
                    return false;
                }
                Some(false) => {
                    tracing::debug!("Pool thread: leader {} blocked by config", leader);
                    return true;
                }
                None => {}
            }
        }

        tracing::debug!("Pool thread: unknown leader {:?}, allowing by default", tg_leader_name);
        return false;
    }

    if pid > 0 {
        let cmdline_path = format!("/proc/{}/cmdline", pid);
        if let Ok(cmdline) = std::fs::read_to_string(&cmdline_path) {
            let cmdline = cmdline.replace('\0', " ").to_lowercase();
            if cmdline.contains("--thumbnail")
                || cmdline.contains("-thumbnail")
                || cmdline.contains("thumbnailer")
            {
                return true;
            }
        }
    }

    false
}

fn get_thread_group_leader_with_exe(pid: u32) -> (Option<u32>, Option<String>, Option<String>) {
    let status_path = format!("/proc/{}/status", pid);
    let status = match std::fs::read_to_string(&status_path) {
        Ok(s) => s,
        Err(_) => return (None, None, None),
    };

    let tgid: u32 = match status
        .lines()
        .find(|line| line.starts_with("Tgid:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
    {
        Some(t) if t > 0 => t,
        _ => return (None, None, None),
    };

    let comm_path = format!("/proc/{}/comm", tgid);
    let leader_name = std::fs::read_to_string(&comm_path)
        .ok()
        .map(|s| s.trim().to_string());

    let exe_path = format!("/proc/{}/exe", tgid);
    let leader_exe = std::fs::read_link(&exe_path)
        .ok()
        .map(|p| p.to_string_lossy().to_string());

    (Some(tgid), leader_name, leader_exe)
}

pub(super) fn add_gtk_bookmark(path: &Path, name: &str) {
    let bookmarks_path = match dirs::config_dir() {
        Some(config) => config.join("gtk-3.0").join("bookmarks"),
        None => return,
    };

    let uri = format!("file://{}", path.display());
    let bookmark_line = format!("{} {}", uri, name);

    let existing = std::fs::read_to_string(&bookmarks_path).unwrap_or_default();
    if existing.lines().any(|line| line.starts_with(&uri)) {
        return;
    }

    let new_content = if existing.is_empty() || existing.ends_with('\n') {
        format!("{}{}\n", existing, bookmark_line)
    } else {
        format!("{}\n{}\n", existing, bookmark_line)
    };

    if let Some(parent) = bookmarks_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if let Err(e) = std::fs::write(&bookmarks_path, new_content) {
        tracing::warn!("Failed to add GTK bookmark: {}", e);
    } else {
        tracing::info!("Added Proton Drive to GTK bookmarks");
    }
}

pub(super) fn remove_gtk_bookmark(path: &Path) {
    let bookmarks_path = match dirs::config_dir() {
        Some(config) => config.join("gtk-3.0").join("bookmarks"),
        None => return,
    };

    let uri = format!("file://{}", path.display());

    let existing = match std::fs::read_to_string(&bookmarks_path) {
        Ok(content) => content,
        Err(_) => return,
    };

    let new_content: String = existing
        .lines()
        .filter(|line| !line.starts_with(&uri))
        .collect::<Vec<_>>()
        .join("\n");

    let new_content = if new_content.is_empty() {
        String::new()
    } else {
        format!("{}\n", new_content)
    };

    if let Err(e) = std::fs::write(&bookmarks_path, new_content) {
        tracing::warn!("Failed to remove GTK bookmark: {}", e);
    } else {
        tracing::info!("Removed Proton Drive from GTK bookmarks");
    }
}

pub fn clear_cache() -> Result<()> {
    use indicatif::{ProgressBar, ProgressStyle};

    let cache_dir = dirs::cache_dir()
        .context("Could not determine cache directory")?
        .join("pdcli")
        .join("files");

    if !cache_dir.exists() {
        println!("{}", style("Cache is already empty.").yellow());
        return Ok(());
    }

    let mut file_count = 0u64;
    let mut total_size = 0u64;

    if let Ok(entries) = std::fs::read_dir(&cache_dir) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata() {
                if metadata.is_file() {
                    file_count += 1;
                    total_size += metadata.len();
                }
            }
        }
    }

    if file_count == 0 {
        println!("{}", style("Cache is already empty.").yellow());
        return Ok(());
    }

    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap(),
    );
    spinner.enable_steady_tick(std::time::Duration::from_millis(80));
    spinner.set_message(format!(
        "Clearing {} cached files ({})...",
        file_count,
        humanize_size(total_size)
    ));

    if let Ok(entries) = std::fs::read_dir(&cache_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    spinner.finish_with_message(format!(
        "{} Cleared {} cached files ({})",
        style("✓").green().bold(),
        file_count,
        humanize_size(total_size)
    ));

    Ok(())
}

pub fn clear_pending_uploads() -> Result<()> {
    let pending_dir = dirs::config_dir()
        .context("Could not determine config directory")?
        .join("pdcli")
        .join("pending_uploads");

    if !pending_dir.exists() {
        println!("{}", style("No pending uploads directory found.").yellow());
        return Ok(());
    }

    let mut file_count = 0u64;
    if let Ok(entries) = std::fs::read_dir(&pending_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                let _ = std::fs::remove_file(&path);
                file_count += 1;
            }
        }
    }

    if file_count == 0 {
        println!("{}", style("No pending uploads.").yellow());
    } else {
        println!(
            "{} Cleared {} pending uploads",
            style("✓").green().bold(),
            file_count
        );
    }

    Ok(())
}

fn humanize_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}
