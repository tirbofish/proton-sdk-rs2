use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

pub fn spinner(msg: impl Into<String>) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner:.magenta} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.set_message(msg.into());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

/// Print a subtle success line to stderr.
pub fn ok(msg: impl std::fmt::Display) {
    use console::style;
    eprintln!("{} {}", style("✓").green().dim(), style(msg).dim());
}

/// Print a subtle skip/warning line to stderr.
pub fn skip(msg: impl std::fmt::Display) {
    use console::style;
    eprintln!("{} {}", style("↪").yellow().dim(), style(msg).dim());
}

/// Upload bar: purple fill on white backing — progress is "from" local (white/neutral), destination is Drive (purple).
pub fn upload_bar(total: i64) -> ProgressBar {
    let pb = ProgressBar::new(total.max(0) as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "  {spinner:.magenta} {msg:<40} [{bar:40.magenta/white}] {bytes}/{total_bytes} {binary_bytes_per_sec} ({eta})",
        )
        .unwrap()
        .progress_chars("█░"),
    );
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

/// Download bar: white fill on purple backing — progress is "from" Drive (purple), destination is local (white/neutral).
pub fn download_bar(total: i64) -> ProgressBar {
    let pb = ProgressBar::new(total.max(0) as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "  {spinner:.white} Downloading [{bar:40.white/magenta}] {bytes}/{total_bytes} {binary_bytes_per_sec} ({eta})",
        )
        .unwrap()
        .progress_chars("█░"),
    );
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}
