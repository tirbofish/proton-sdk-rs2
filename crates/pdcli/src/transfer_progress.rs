//! Live terminal view of a batch transfer: an overall bar plus one line per active file.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::transfer::format_bytes;

const BAR_WIDTH: usize = 24;
const JOB_BAR_WIDTH: usize = 12;
const REDRAW_INTERVAL: Duration = Duration::from_millis(250);
// Long enough to smooth over 4 MB block bursts on slow links.
const RATE_WINDOW: Duration = Duration::from_secs(15);

/// Words that describe one kind of batch transfer.
pub struct Labels {
    /// Leads the overall line, such as "Importing".
    pub verb: &'static str,
    /// What the files are called, such as "photos".
    pub noun: &'static str,
    /// Count label for files transferred, such as "uploaded".
    pub done: &'static str,
}

#[derive(Clone, Copy)]
pub enum Stage {
    Hashing,
    Checking,
    /// Waiting for the SDK to admit another transfer.
    Queued,
    Uploading,
    Downloading,
    Retrying {
        attempt: u32,
    },
}

impl Stage {
    fn label(self) -> String {
        match self {
            Stage::Hashing => "hashing".into(),
            Stage::Checking => "checking".into(),
            Stage::Queued => "queued".into(),
            Stage::Uploading => "uploading".into(),
            Stage::Downloading => "downloading".into(),
            Stage::Retrying { attempt } => format!("retry #{attempt}"),
        }
    }

    fn shows_bytes(self) -> bool {
        matches!(
            self,
            Stage::Uploading | Stage::Downloading | Stage::Retrying { .. }
        )
    }
}

pub enum Outcome {
    Transferred,
    Skipped,
    Failed,
}

struct Job {
    name: String,
    size: u64,
    sent: u64,
    stage: Stage,
}

struct State {
    jobs: BTreeMap<usize, Job>,
    total_files: usize,
    total_bytes: u64,
    finished: usize,
    transferred_files: usize,
    skipped: usize,
    failed: usize,
    finished_bytes: u64,
    // Bytes actually moved, including retried ones, for the transfer rate.
    transferred: u64,
    samples: VecDeque<(Instant, u64)>,
    status: Option<String>,
    drawn_lines: usize,
}

impl State {
    fn rate(&self) -> Option<f64> {
        let (start, start_bytes) = *self.samples.front()?;
        let (end, end_bytes) = *self.samples.back()?;
        let elapsed = end.duration_since(start).as_secs_f64();
        (elapsed >= 1.0).then(|| (end_bytes - start_bytes) as f64 / elapsed)
    }

    fn lines(&self, labels: &Labels, max_jobs: usize) -> Vec<String> {
        let status = self
            .status
            .as_ref()
            .map(|status| format!(" · {status}"))
            .unwrap_or_default();
        // Still discovering what to transfer.
        if self.total_files == 0 {
            return vec![format!("{}…{status}", labels.verb)];
        }
        let done = self.finished_bytes + self.jobs.values().map(|job| job.sent).sum::<u64>();
        let fraction = if self.total_bytes == 0 {
            self.finished as f64 / self.total_files as f64
        } else {
            (done as f64 / self.total_bytes as f64).min(1.0)
        };
        let rate = self.rate();
        let speed = match rate {
            Some(rate) => format!("{}/s", format_bytes(rate as i64)),
            None => "–/s".into(),
        };
        let eta = match rate {
            Some(rate) if rate > 0.0 => format_duration(Duration::from_secs_f64(
                self.total_bytes.saturating_sub(done) as f64 / rate,
            )),
            _ => "–".into(),
        };
        let mut lines = vec![
            format!(
                "{} {} {:3}%  {}/{} {}  {} / {}  {speed}  ETA {eta}",
                labels.verb,
                bar(fraction, BAR_WIDTH),
                (fraction * 100.0) as u32,
                self.finished,
                self.total_files,
                labels.noun,
                format_bytes(done as i64),
                format_bytes(self.total_bytes as i64),
            ),
            format!(
                "  {} {} · {} skipped · {} failed{status}",
                self.transferred_files, labels.done, self.skipped, self.failed,
            ),
        ];
        for job in self.jobs.values().take(max_jobs) {
            let detail = if job.stage.shows_bytes() && job.size > 0 {
                let fraction = (job.sent as f64 / job.size as f64).min(1.0);
                format!(
                    "{} {:3}% {:>9} / {:<9}",
                    bar(fraction, JOB_BAR_WIDTH),
                    (fraction * 100.0) as u32,
                    format_bytes(job.sent as i64),
                    format_bytes(job.size as i64)
                )
            } else {
                // Line the size up under the transferring rows' total column.
                format!("{:32}{:<9}", "", format_bytes(job.size as i64))
            };
            lines.push(format!(
                "  {:<11} {detail}  {}",
                job.stage.label(),
                job.name
            ));
        }
        if self.jobs.len() > max_jobs {
            lines.push(format!("  … and {} more", self.jobs.len() - max_jobs));
        }
        lines
    }
}

/// Shows batch progress on stderr when it is a terminal, and routes per-file messages.
///
/// Messages go to stdout as before. When stdout shares the terminal with the live view,
/// they are printed above it and JSON events are shown as readable lines instead.
pub struct TransferProgress {
    labels: Labels,
    state: Mutex<State>,
    live: bool,
    shares_terminal: bool,
    // Background work (the daemon) logs through tracing instead of printing.
    hidden: bool,
}

impl TransferProgress {
    pub fn new(labels: Labels) -> Self {
        let live = std::io::stderr().is_terminal();
        Self::with_output(labels, live, live && std::io::stdout().is_terminal(), false)
    }

    /// Tracks nothing on screen and sends messages to the log.
    pub fn hidden(labels: Labels) -> Self {
        Self::with_output(labels, false, false, true)
    }

    fn with_output(labels: Labels, live: bool, shares_terminal: bool, hidden: bool) -> Self {
        Self {
            labels,
            state: Mutex::new(State {
                jobs: BTreeMap::new(),
                total_files: 0,
                total_bytes: 0,
                finished: 0,
                transferred_files: 0,
                skipped: 0,
                failed: 0,
                finished_bytes: 0,
                transferred: 0,
                samples: VecDeque::new(),
                status: None,
                drawn_lines: 0,
            }),
            live,
            shares_terminal,
            hidden,
        }
    }

    pub fn set_totals(&self, files: usize, bytes: u64) {
        let mut state = self.state.lock().unwrap();
        state.total_files = files;
        state.total_bytes = bytes;
    }

    pub fn start(&self, id: usize, name: impl Into<String>, size: u64) {
        self.state.lock().unwrap().jobs.insert(
            id,
            Job {
                name: name.into(),
                size,
                sent: 0,
                stage: Stage::Queued,
            },
        );
    }

    pub fn stage(&self, id: usize, stage: Stage) {
        if let Some(job) = self.state.lock().unwrap().jobs.get_mut(&id) {
            job.stage = stage;
            if matches!(stage, Stage::Hashing) {
                job.sent = 0;
            }
        }
    }

    /// Records the bytes of one file moved so far.
    pub fn sent(&self, id: usize, sent: u64) {
        let mut state = self.state.lock().unwrap();
        let Some(job) = state.jobs.get_mut(&id) else {
            return;
        };
        let delta = sent.saturating_sub(job.sent);
        job.sent = sent;
        state.transferred += delta;
    }

    /// An SDK progress callback that feeds [`Self::sent`].
    pub fn callback(self: &std::sync::Arc<Self>, id: usize) -> Box<dyn Fn(i64, i64) + Send + Sync> {
        self.callback_after(id, 0)
    }

    /// Like [`Self::callback`], for a later transfer of a file that already moved `offset` bytes.
    pub fn callback_after(
        self: &std::sync::Arc<Self>,
        id: usize,
        offset: u64,
    ) -> Box<dyn Fn(i64, i64) + Send + Sync> {
        let progress = self.clone();
        Box::new(move |sent, _| progress.sent(id, offset + sent.max(0) as u64))
    }

    pub fn finish(&self, id: usize, outcome: Outcome) {
        let mut state = self.state.lock().unwrap();
        let Some(job) = state.jobs.remove(&id) else {
            return;
        };
        state.finished += 1;
        state.finished_bytes += job.size;
        match outcome {
            Outcome::Transferred => state.transferred_files += 1,
            Outcome::Skipped => state.skipped += 1,
            Outcome::Failed => state.failed += 1,
        }
    }

    pub fn status(&self, status: Option<String>) {
        self.state.lock().unwrap().status = status;
    }

    /// Prints one machine-readable event, readable when a person is watching.
    pub fn event(&self, event: serde_json::Value) {
        if self.shares_terminal {
            self.print(&readable_event(&event));
        } else {
            self.print(&event.to_string());
        }
    }

    pub fn log(&self, line: &str) {
        if self.hidden {
            tracing::info!("{line}");
        } else {
            self.print(line);
        }
    }

    pub fn warn(&self, line: &str) {
        if self.hidden {
            tracing::warn!("{line}");
        } else {
            self.print(&format!("! {line}"));
        }
    }

    fn print(&self, line: &str) {
        let mut state = self.state.lock().unwrap();
        if !self.shares_terminal {
            println!("{line}");
            return;
        }
        let mut stderr = std::io::stderr().lock();
        let _ = stderr.write_all(clear_sequence(state.drawn_lines).as_bytes());
        let _ = stderr.flush();
        state.drawn_lines = 0;
        println!("{line}");
        let _ = std::io::stdout().flush();
        self.draw(&mut state, &mut stderr);
    }

    /// Runs `work` while keeping the view and transfer rate current.
    pub async fn drive<T>(&self, work: impl Future<Output = T>) -> T {
        tokio::pin!(work);
        let mut redraw = tokio::time::interval(REDRAW_INTERVAL);
        loop {
            tokio::select! {
                output = &mut work => {
                    self.redraw();
                    return output;
                }
                _ = redraw.tick() => self.redraw(),
            }
        }
    }

    fn redraw(&self) {
        if !self.live {
            return;
        }
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        let transferred = state.transferred;
        state.samples.push_back((now, transferred));
        while state
            .samples
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > RATE_WINDOW)
        {
            state.samples.pop_front();
        }
        self.draw(&mut state, &mut std::io::stderr().lock());
    }

    fn draw(&self, state: &mut State, out: &mut impl Write) {
        if !self.live {
            return;
        }
        let (width, height) = terminal_size();
        // Keep the view shorter than the screen so cursor movement stays inside it.
        let lines = state.lines(&self.labels, height.saturating_sub(4).max(1));
        let mut frame = clear_sequence(state.drawn_lines);
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                frame.push('\n');
            }
            frame.extend(line.chars().take(width.saturating_sub(1)));
        }
        let _ = out.write_all(frame.as_bytes());
        let _ = out.flush();
        state.drawn_lines = lines.len();
    }
}

impl Drop for TransferProgress {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap();
        if self.live && state.drawn_lines > 0 {
            let _ = writeln!(std::io::stderr().lock());
        }
    }
}

/// Moves to the first line of the previous frame and erases everything below it.
fn clear_sequence(drawn_lines: usize) -> String {
    match drawn_lines {
        0 => String::new(),
        1 => "\r\x1b[J".into(),
        lines => format!("\r\x1b[{}A\x1b[J", lines - 1),
    }
}

fn readable_event(event: &serde_json::Value) -> String {
    let text = |key: &str| event[key].as_str().unwrap_or_default();
    let file = Path::new(text("path"))
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    match text("action") {
        "uploaded" if event["duplicate"].as_bool() == Some(true) => {
            format!("✓ uploaded  {} (kept both copies)", text("name"))
        }
        "uploaded" if text("name") != file => {
            format!("✓ uploaded  {file} as {}", text("name"))
        }
        "uploaded" => format!("✓ uploaded  {file}"),
        "skipped" => format!("· skipped   {file} (already in library)"),
        "retrying" => format!(
            "↻ retrying  {file} (attempt {}): {}",
            event["attempt"],
            text("error")
        ),
        "failed" => format!("✗ failed    {file}: {}", text("error")),
        "summary" => format!(
            "Done: {} uploaded, {} skipped, {} failed of {} photos",
            event["uploaded"], event["skipped"], event["failed"], event["total"]
        ),
        _ => event.to_string(),
    }
}

fn bar(fraction: f64, width: usize) -> String {
    let filled = ((fraction * width as f64) as usize).min(width);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(width - filled))
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

fn terminal_size() -> (usize, usize) {
    // SAFETY: TIOCGWINSZ only writes a winsize into the provided struct.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    if ok && size.ws_col > 0 && size.ws_row > 0 {
        (usize::from(size.ws_col), usize::from(size.ws_row))
    } else {
        (80, 24)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LABELS: Labels = Labels {
        verb: "Importing",
        noun: "photos",
        done: "uploaded",
    };

    fn lines(progress: &TransferProgress, max_jobs: usize) -> Vec<String> {
        progress
            .state
            .lock()
            .unwrap()
            .lines(&progress.labels, max_jobs)
    }

    #[test]
    fn overall_progress_counts_finished_and_in_flight_bytes() {
        let progress = TransferProgress::hidden(LABELS);
        progress.set_totals(2, 200);
        progress.start(0, "a.jpg", 100);
        progress.start(1, "b.mov", 100);
        progress.finish(0, Outcome::Skipped);
        progress.stage(1, Stage::Uploading);
        progress.sent(1, 50);
        let lines = lines(&progress, 8);
        assert!(lines[0].contains(" 75%"), "{}", lines[0]);
        assert!(lines[0].contains("1/2 photos"), "{}", lines[0]);
        assert!(lines[1].contains("1 skipped"), "{}", lines[1]);
        assert!(lines[2].contains("uploading"), "{}", lines[2]);
        assert!(lines[2].ends_with("b.mov"), "{}", lines[2]);
    }

    #[test]
    fn listing_shows_only_the_status_until_totals_are_known() {
        let progress = TransferProgress::hidden(LABELS);
        progress.status(Some("12 files found".into()));
        assert_eq!(lines(&progress, 8), ["Importing… · 12 files found"]);
    }

    #[test]
    fn restarting_a_file_resets_its_bytes_but_not_the_transfer_total() {
        let progress = TransferProgress::hidden(LABELS);
        progress.start(0, "a.jpg", 100);
        progress.sent(0, 60);
        progress.stage(0, Stage::Hashing);
        progress.sent(0, 20);
        let state = progress.state.lock().unwrap();
        assert_eq!(state.jobs[&0].sent, 20);
        assert_eq!(state.transferred, 80);
    }

    #[test]
    fn hidden_jobs_are_summarized_when_the_screen_is_short() {
        let progress = TransferProgress::hidden(LABELS);
        progress.set_totals(3, 30);
        for id in 0..3 {
            progress.start(id, "a.jpg", 10);
        }
        assert_eq!(lines(&progress, 1).last().unwrap(), "  … and 2 more");
    }

    #[test]
    fn readable_events_name_the_file_and_outcome() {
        let event = serde_json::json!({"action": "uploaded", "duplicate": false, "name": "a (1).jpg", "path": "/x/a.jpg"});
        assert_eq!(readable_event(&event), "✓ uploaded  a.jpg as a (1).jpg");
        let event = serde_json::json!({"action": "failed", "path": "/x/b.png", "error": "boom"});
        assert_eq!(readable_event(&event), "✗ failed    b.png: boom");
    }
}
