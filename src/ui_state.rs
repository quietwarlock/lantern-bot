//! Shared state between the window and the Discord task.
//!
//! The window and the bot run on different threads. Both hold a clone of
//! [`UiState`]. Logs, the connection line, a snapshot of each server's queue,
//! and the settings the bot should use are behind one mutex. Log lines are
//! also appended to `lantern-log.txt` when a log file is set.

use crate::queue::LoopMode;
use crate::settings::Settings;
use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

/// The log file is renamed to `lantern-log.old.txt` once it passes this size.
const LOG_LIMIT: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Live {
    pub connection: String,
    pub title: String,
    pub detail: String,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            connection: "Offline".into(),
            title: "Nothing playing".into(),
            detail: "Minimize to keep Lantern in the system tray.".into(),
        }
    }
}

/// What the window shows for one server.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuildView {
    pub id: u64,
    pub name: String,
    pub current: Option<String>,
    pub paused: bool,
    pub position: u64,
    pub length: Option<u64>,
    /// The current item is a live radio station: no length, shown as LIVE (1.10).
    pub live: bool,
    pub upcoming: Vec<String>,
    pub loop_mode: LoopMode,
    pub autoplay: bool,
    pub soundcloud_autoplay: bool,
    pub volume: i32,
    /// Recently played, newest first: (when, Unix seconds; title).
    pub recent: Vec<(i64, String)>,
}

#[derive(Clone)]
pub struct UiState {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    logs: VecDeque<String>,
    /// Lines logged since start, so the web page can ask for only new ones (1.9).
    logged: u64,
    live: Live,
    guilds: Vec<GuildView>,
    settings: Settings,
    log_file: Option<PathBuf>,
    /// Also print each line (server build: shows in `docker logs`).
    echo: bool,
    /// Shown once in the window when a settings or data file could not be saved.
    save_warning: Option<String>,
    /// A newer release found by the update check: (version, page link).
    update: Option<(String, String)>,
}

impl UiState {
    pub fn new(settings: Settings) -> Self {
        crate::ytdlp::set_enabled(settings.youtube_backup);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                logs: VecDeque::from([format!("{}   Lantern {}.", stamp(), crate::VERSION)]),
                logged: 1,
                live: Live::default(),
                guilds: Vec::new(),
                settings,
                log_file: None,
                echo: false,
                save_warning: None,
                update: None,
            })),
        }
    }

    /// Also write every log line to this file.
    pub fn with_log_file(self, path: PathBuf) -> Self {
        self.lock().log_file = Some(path);
        self
    }

    /// Also print every log line to standard output (the server build).
    pub fn with_stdout(self) -> Self {
        self.lock().echo = true;
        self
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic elsewhere must not take the window down with it.
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn log(&self, message: impl Into<String>) {
        let message = message.into();
        let line = format!("{}   {}", stamp(), message);
        // The file keeps the date too, so lines from different days can be told apart.
        let dated = format!("{}   {}", file_stamp(), message);
        let (file, echo) = {
            let mut inner = self.lock();
            inner.logs.push_back(line.clone());
            inner.logged += 1;
            while inner.logs.len() > 300 {
                inner.logs.pop_front();
            }
            (inner.log_file.clone(), inner.echo)
        };
        if echo {
            println!("{dated}");
        }
        if let Some(path) = file {
            append_log(&path, &dated);
        }
    }

    pub fn logs(&self) -> Vec<String> {
        self.lock().logs.iter().cloned().collect()
    }

    /// Lines numbered `from` and later that are still kept (the last 300), and
    /// the number to ask for next time (1.9, web page).
    pub fn logs_since(&self, from: u64) -> (Vec<String>, u64) {
        let inner = self.lock();
        let first = inner.logged - inner.logs.len() as u64;
        let skip = from.saturating_sub(first) as usize;
        (inner.logs.iter().skip(skip).cloned().collect(), inner.logged)
    }

    pub fn set_live(&self, connection: &str, title: &str, detail: &str) {
        let mut inner = self.lock();
        inner.live.connection = connection.into();
        inner.live.title = title.into();
        inner.live.detail = detail.into();
    }

    pub fn set_connection(&self, connection: &str) {
        self.lock().live.connection = connection.into();
    }

    pub fn live(&self) -> Live {
        self.lock().live.clone()
    }

    pub fn set_guilds(&self, guilds: Vec<GuildView>) {
        self.lock().guilds = guilds;
    }

    pub fn guilds(&self) -> Vec<GuildView> {
        self.lock().guilds.clone()
    }

    pub fn settings(&self) -> Settings {
        self.lock().settings.clone()
    }

    pub fn set_settings(&self, settings: Settings) {
        crate::ytdlp::set_enabled(settings.youtube_backup);
        self.lock().settings = settings;
    }

    /// Logs the message and sets a one-time banner in the window (first failure only).
    pub fn note_save_failure(&self, message: impl Into<String>) {
        let message = message.into();
        {
            let mut inner = self.lock();
            if inner.save_warning.is_none() {
                inner.save_warning = Some(message.clone());
            }
        }
        self.log(message);
    }

    pub fn set_update(&self, version: String, link: String) {
        self.lock().update = Some((version, link));
    }

    pub fn update(&self) -> Option<(String, String)> {
        self.lock().update.clone()
    }

    pub fn take_save_warning(&self) -> Option<String> {
        self.lock().save_warning.take()
    }
}

fn append_log(path: &PathBuf, line: &str) {
    // The bot and the window both log. One at a time, or two rotations can swallow lines.
    static FILE_LOCK: Mutex<()> = Mutex::new(());
    let _guard = FILE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if std::fs::metadata(path).map(|m| m.len() > LOG_LIMIT).unwrap_or(false) {
        let _ = std::fs::rename(path, path.with_file_name("lantern-log.old.txt"));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}

/// Date and local time for `lantern-log.txt`.
pub fn file_stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Local time, so the log matches the clock on the PC.
fn stamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::{UiState, LOG_LIMIT};
    use crate::settings::Settings;

    #[test]
    fn logs_since_returns_only_new_lines() {
        let ui = UiState::new(Settings::default());
        let (all, next) = ui.logs_since(0);
        assert_eq!(all.len(), 1);
        ui.log("a");
        ui.log("b");
        let (new, after) = ui.logs_since(next);
        assert_eq!(new.len(), 2);
        assert!(new[0].ends_with("a") && new[1].ends_with("b"));
        assert_eq!(ui.logs_since(after).0.len(), 0);
        for n in 0..400 {
            ui.log(format!("n{n}"));
        }
        let (kept, _) = ui.logs_since(0);
        assert_eq!(kept.len(), 300);
        assert!(kept.last().unwrap().ends_with("n399"));
    }

    #[test]
    fn log_file_rotates_past_the_limit() {
        let dir = std::env::temp_dir().join(format!("lantern-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lantern-log.txt");
        std::fs::write(&path, vec![b'x'; LOG_LIMIT as usize + 10]).unwrap();
        let ui = UiState::new(Settings::default()).with_log_file(path.clone());
        ui.log("hello");
        assert!(dir.join("lantern-log.old.txt").exists());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with("hello\n"), "{text}");
        let last = text.lines().last().unwrap();
        let bytes = last.as_bytes();
        assert!(bytes[4] == b'-' && bytes[7] == b'-' && bytes[10] == b' ', "the file line has a date: {last}");
        assert!(ui.logs().last().unwrap().ends_with("hello"));
    }

    #[test]
    fn simultaneous_logging_across_a_rotation_loses_no_lines() {
        let dir = std::env::temp_dir().join(format!("lantern-log-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lantern-log.txt");
        std::fs::write(&path, vec![b'x'; LOG_LIMIT as usize + 10]).unwrap();
        let ui = UiState::new(Settings::default()).with_log_file(path.clone());
        let threads: Vec<_> = (0..8)
            .map(|id| {
                let ui = ui.clone();
                std::thread::spawn(move || {
                    for n in 0..50 {
                        ui.log(format!("line-{id}-{n}"));
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let count = |file: std::path::PathBuf| {
            std::fs::read_to_string(file).unwrap_or_default().lines().filter(|l| l.contains("line-")).count()
        };
        assert_eq!(count(path) + count(dir.join("lantern-log.old.txt")), 400);
    }
}
