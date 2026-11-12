//! Settings saved as `lantern-settings.json` beside the executable.
//!
//! The bot token lives in that file after you paste it into the window.
//! It is not compiled into the program. Every field has a default, so a
//! settings file from an older version still loads. A file that can't be
//! read is copied to `lantern-settings.json.bak` before defaults are used.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub prefix: String,
    /// Volume for servers that haven't set their own with `volume`.
    pub volume: i32,
    pub playlist_limit: usize,
    pub stay_in_channel: bool,
    pub leave_alone: bool,
    /// Discord "Listening to" presence for the current song. Independent of `quiet`.
    pub song_status: bool,
    /// Fewer routine posts in the music text channel (Queued, Now playing cards,
    /// skip/shuffle button shout-outs, the restart restore offer).
    /// Does not turn off `song_status`.
    pub quiet: bool,
    pub start_on_open: bool,
    /// Hide the window in the notification area instead of the taskbar.
    pub minimize_to_tray: bool,
    /// Text searches show four YouTube results and wait for a reaction.
    pub choose_result: bool,
    /// Fall back to yt-dlp when YouTube's own clients stop working.
    pub youtube_backup: bool,
    /// Leave the voice channel after this many minutes with nothing playing. 0 = never.
    pub idle_minutes: u32,
    /// Discord bot token. Stored beside the exe, never inside it.
    pub token: String,
    /// Discord user ID with full control everywhere. Empty means anyone,
    /// unless a server has set a DJ role.
    pub owner: String,
    /// Music library folders, added in the window. A network share like
    /// `\\nas\music` works. Discord can search them, never change them.
    pub library_folders: Vec<String>,
    /// Volume leveling: ReplayGain tags for files, YouTube's loudness figure for videos.
    pub level_volume: bool,
    /// Songs one non-DJ can have waiting in a server's queue. 0 = no limit.
    pub max_per_user: usize,
    /// Longest song a non-DJ can queue, in minutes. 0 = any length.
    pub max_song_minutes: u32,
    /// Check GitHub Releases for a newer Lantern at startup (notify only).
    pub update_check: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            prefix: "!".into(),
            volume: 80,
            playlist_limit: 25,
            stay_in_channel: true,
            leave_alone: false,
            song_status: true,
            quiet: false,
            start_on_open: true,
            minimize_to_tray: true,
            choose_result: true,
            youtube_backup: true,
            idle_minutes: 5,
            token: String::new(),
            owner: String::new(),
            library_folders: Vec::new(),
            level_volume: true,
            max_per_user: 0,
            max_song_minutes: 0,
            update_check: true,
        }
    }
}

impl Settings {
    pub fn clamp(&mut self) {
        self.prefix = self.prefix.trim().to_string();
        if self.prefix.is_empty() {
            self.prefix = "!".into();
        }
        self.volume = self.volume.clamp(0, 200);
        self.playlist_limit = self.playlist_limit.clamp(1, 100);
        self.idle_minutes = self.idle_minutes.min(240);
        self.max_per_user = self.max_per_user.min(1000);
        self.max_song_minutes = self.max_song_minutes.min(24 * 60);
        self.token = self.token.trim().trim_matches('"').trim_start_matches("Bot ").trim().to_string();
        self.owner = self.owner.trim().trim_matches('"').trim().to_string();
        let mut folders: Vec<String> = Vec::new();
        for folder in self.library_folders.iter().map(|f| crate::library::normalize_folder(f)) {
            if !folder.is_empty() && !folders.iter().any(|f| f.eq_ignore_ascii_case(&folder)) {
                folders.push(folder);
            }
        }
        self.library_folders = folders;
    }

    /// Routine music-channel posts. Off when Quiet is on.
    /// Listening status is [`Self::song_status`] and is not this.
    pub fn routine_posts(&self) -> bool {
        routine_channel_post(self.quiet)
    }

    /// Loads the file. The second value is a warning for the activity log when
    /// the file existed but couldn't be used.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        let (mut settings, warning) = match read_text(path) {
            Ok(None) => (Settings::default(), None),
            Ok(Some(text)) => match serde_json::from_str::<Settings>(&text) {
                Ok(settings) => (settings, None),
                // One bad value (say a hand-edited `"volume": "loud"`) used to
                // throw away the whole file, token included. Keep every field that's fine.
                Err(err) => match Self::salvage(&text) {
                    Some(settings) => {
                        let note = format!("{} Kept the settings that were fine; check the rest.", damaged_note(path, &err.to_string()));
                        (settings, Some(note.replace(" Starting fresh.", "")))
                    }
                    None => (Settings::default(), Some(settings_note(path, &err.to_string()))),
                },
            },
            Err(err) => (Settings::default(), Some(settings_note(path, &err.to_string()))),
        };
        settings.clamp();
        (settings, warning)
    }

    /// Reads each field on its own, skipping the ones that don't fit. `None`
    /// when the text isn't a JSON object at all.
    fn salvage(text: &str) -> Option<Self> {
        let serde_json::Value::Object(fields) = serde_json::from_str(text).ok()? else { return None };
        let mut base = serde_json::to_value(Settings::default()).ok()?;
        for (key, value) in fields {
            let mut trial = base.clone();
            trial.as_object_mut()?.insert(key, value);
            if serde_json::from_value::<Settings>(trial.clone()).is_ok() {
                base = trial;
            }
        }
        serde_json::from_value(base).ok()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        write_atomic(path, &text)?;
        // 1.9: the server's web page saves this file. It holds the token, so
        // only its owner may read it (Windows keeps its own folder rules).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

/// Whether a routine channel post should go out.
/// Covers "Queued …", Now playing cards, skip/shuffle button shout-outs,
/// and the unsolicited `restore` offer after a restart.
/// Command answers, errors, the activity log, and Listening status are separate.
pub fn routine_channel_post(quiet: bool) -> bool {
    !quiet
}

fn settings_note(path: &Path, why: &str) -> String {
    format!("{} Paste your token again.", damaged_note(path, why))
}

pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".bak");
    path.with_file_name(name)
}

/// Reads a text file. `None` means it doesn't exist yet. A leading byte-order
/// mark is dropped, since Notepad can add one when someone edits the file.
pub fn read_text(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text.trim_start_matches('\u{feff}').to_string())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Copies a file that couldn't be used to `.bak` and says so, for the activity log.
pub fn damaged_note(path: &Path, why: &str) -> String {
    let backup = backup_path(path);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if std::fs::copy(path, &backup).is_ok() {
        format!("{name} couldn't be read ({why}). A copy is at {}. Starting fresh.", backup.display())
    } else {
        format!("{name} couldn't be read ({why}). Starting fresh.")
    }
}

/// Writes to a temporary file first so a crash never leaves half a file. Each
/// call has its own temporary name, so two writers can't trample each other.
pub fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut tmp_name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    tmp_name.push(format!(".{}.{}.tmp", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
    let tmp = path.with_file_name(tmp_name);
    let written = std::fs::File::create(&tmp).and_then(|mut file| {
        file.write_all(text.as_bytes())?;
        file.sync_all()
    });
    if let Err(err) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(err.to_string());
    }
    // Keep the old file's permissions (a private 0600 file stays private).
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    // On Windows a virus scanner can hold the old file for a moment.
    let mut result = std::fs::rename(&tmp, path);
    for _ in 0..3 {
        if result.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        result = std::fs::rename(&tmp, path);
    }
    result.map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        err.to_string()
    })
}

/// Folder that holds the exe. Settings, data, and the log live here.
/// Set this to keep settings, data, and the log in another folder (Docker: `/config`).
pub const DIR_ENV: &str = "LANTERN_DIR";

/// The folder beside the exe, or `LANTERN_DIR` when it's set.
pub fn app_dir() -> PathBuf {
    // 1.8.3: the server build (Docker) keeps everything in one config folder.
    if let Some(dir) = std::env::var_os(DIR_ENV).filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn settings_path() -> PathBuf {
    app_dir().join("lantern-settings.json")
}

#[cfg(test)]
mod tests {
    use super::{routine_channel_post, Settings};

    #[cfg(unix)]
    #[test]
    fn saved_files_stay_private_on_linux() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp("private");
        Settings::default().save(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600, "a new settings file is private");
        let data = path.with_file_name("lantern-data.json");
        std::fs::write(&data, "{}").unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o600)).unwrap();
        super::write_atomic(&data, "{\"a\":1}").unwrap();
        assert_eq!(std::fs::metadata(&data).unwrap().permissions().mode() & 0o777, 0o600, "rewriting keeps 0600");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn one_bad_value_keeps_the_token_and_the_rest() {
        let dir = temp("salvage");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lantern-settings.json");
        std::fs::write(&path, r#"{"token":"abc.def.ghi","prefix":"?","volume":"loud"}"#).unwrap();
        let (settings, warning) = Settings::load(&path);
        assert_eq!(settings.token, "abc.def.ghi");
        assert_eq!(settings.prefix, "?");
        assert_eq!(settings.volume, Settings::default().volume);
        let warning = warning.expect("a warning");
        assert!(!warning.contains("Paste your token again"), "{warning}");
        assert!(dir.join("lantern-settings.json.bak").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lantern-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("lantern-settings.json")
    }

    #[test]
    fn version_1_0_settings_file_loads() {
        let path = temp("v10");
        std::fs::write(
            &path,
            r#"{
            "prefix":"?",
            "volume":55,
            "playlist_limit":10,
            "stay_in_channel":false,
            "leave_alone":true,
            "song_status":true,
            "start_on_open":true,
            "minimize_to_tray":true,
            "choose_result":false,
            "token":"abc.def.ghi",
            "owner":"123"
        }"#,
        )
        .unwrap();
        let (settings, warning) = Settings::load(&path);
        assert!(warning.is_none());
        assert_eq!(settings.prefix, "?");
        assert_eq!(settings.volume, 55);
        assert_eq!(settings.playlist_limit, 10);
        assert!(!settings.stay_in_channel);
        assert!(settings.leave_alone);
        assert!(!settings.choose_result);
        assert!(!settings.quiet, "older files stay chatty");
        assert!(settings.song_status, "quiet must not turn listening status off");
        assert!(settings.routine_posts());
        assert_eq!(settings.token, "abc.def.ghi");
        assert_eq!(settings.owner, "123");
        assert_eq!(settings.idle_minutes, 5, "new field gets its default");
    }

    #[test]
    fn version_1_1_1_settings_file_loads_with_the_backup_on() {
        let path = temp("v111");
        std::fs::write(
            &path,
            r#"{
            "prefix": "!",
            "volume": 70,
            "playlist_limit": 25,
            "stay_in_channel": true,
            "leave_alone": false,
            "song_status": true,
            "start_on_open": true,
            "minimize_to_tray": true,
            "choose_result": true,
            "idle_minutes": 7,
            "token": "abc.def.ghi",
            "owner": "123"
        }"#,
        )
        .unwrap();
        let (settings, warning) = Settings::load(&path);
        assert!(warning.is_none(), "{warning:?}");
        assert!(settings.youtube_backup, "the new setting defaults to on");
        assert_eq!(settings.idle_minutes, 7);
        assert_eq!(settings.token, "abc.def.ghi");
        assert_eq!(settings.owner, "123");
    }

    #[test]
    fn quiet_is_off_by_default_and_does_not_gate_song_status() {
        let fresh = Settings::default();
        assert!(!fresh.quiet);
        assert!(fresh.song_status);
        assert!(fresh.routine_posts());
        assert!(routine_channel_post(false));
        assert!(!routine_channel_post(true));

        let path = temp("quiet-on");
        let saved = Settings { quiet: true, song_status: true, ..Default::default() };
        saved.save(&path).unwrap();
        let (loaded, warning) = Settings::load(&path);
        assert!(warning.is_none());
        assert!(loaded.quiet);
        assert!(loaded.song_status, "listening status is its own setting");
        assert!(!loaded.routine_posts());
    }

    #[test]
    fn turning_the_backup_off_is_remembered() {
        let path = temp("backup-off");
        Settings { youtube_backup: false, ..Default::default() }.save(&path).unwrap();
        let (loaded, warning) = Settings::load(&path);
        assert!(warning.is_none());
        assert!(!loaded.youtube_backup);
    }

    #[test]
    fn older_settings_leave_credentials_blank() {
        let raw = r#"{"prefix":"!","volume":80,"playlist_limit":25,"stay_in_channel":true,
            "leave_alone":false,"song_status":true,"start_on_open":true}"#;
        let settings: Settings = serde_json::from_str(raw).unwrap();
        assert!(settings.token.is_empty());
        assert!(settings.owner.is_empty());
        assert!(settings.minimize_to_tray);
    }

    #[test]
    fn damaged_file_is_backed_up_not_lost() {
        let path = temp("bad");
        std::fs::write(&path, "{ \"token\": \"abc\", oops").unwrap();
        let (settings, warning) = Settings::load(&path);
        assert!(warning.is_some());
        assert!(settings.token.is_empty());
        let backup = super::backup_path(&path);
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "{ \"token\": \"abc\", oops");
    }

    #[test]
    fn save_then_load_round_trips() {
        let path = temp("round");
        let settings = Settings { idle_minutes: 0, token: "x.y.z".into(), ..Default::default() };
        settings.save(&path).unwrap();
        let (loaded, warning) = Settings::load(&path);
        assert!(warning.is_none());
        assert_eq!(loaded.idle_minutes, 0);
        assert_eq!(loaded.token, "x.y.z");
    }

    #[test]
    fn file_saved_by_notepad_with_a_byte_order_mark_loads() {
        let path = temp("bom");
        std::fs::write(&path, "\u{feff}{\"prefix\":\"?\",\"token\":\"abc.def.ghi\"}").unwrap();
        let (settings, warning) = Settings::load(&path);
        assert!(warning.is_none(), "{warning:?}");
        assert_eq!(settings.prefix, "?");
        assert_eq!(settings.token, "abc.def.ghi");
    }

    #[test]
    fn unreadable_text_is_backed_up_not_thrown_away() {
        let path = temp("latin1");
        std::fs::write(&path, b"{\"token\":\"caf\xe9\"}").unwrap();
        let (settings, warning) = Settings::load(&path);
        assert!(warning.is_some());
        assert!(settings.token.is_empty());
        assert_eq!(std::fs::read(super::backup_path(&path)).unwrap(), b"{\"token\":\"caf\xe9\"}");
    }

    #[test]
    fn missing_file_is_not_a_warning() {
        let path = temp("missing");
        let (_, warning) = Settings::load(&path);
        assert!(warning.is_none());
    }

    #[test]
    fn many_writers_never_corrupt_or_fail() {
        let path = temp("race");
        let threads: Vec<_> = (0..8u8)
            .map(|id| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let text = String::from((b'a' + id) as char).repeat(200_000);
                    for _ in 0..25 {
                        super::write_atomic(&path, &text).expect("write");
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.len(), 200_000);
        assert!(text.chars().all(|c| c == text.chars().next().unwrap()), "file is a mix of two writes");
        let leftovers = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "temporary files were left behind");
    }

    #[test]
    fn library_folders_load_from_older_files_and_are_cleaned() {
        let path = temp("library");
        std::fs::write(&path, r#"{"prefix":"!"}"#).unwrap();
        assert!(Settings::load(&path).0.library_folders.is_empty());
        let mut settings = Settings {
            library_folders: vec![r"D:\Music\".into(), r"d:\music".into(), "  ".into(), r"\\nas\music".into()],
            ..Default::default()
        };
        settings.clamp();
        assert_eq!(settings.library_folders, vec![r"D:\Music".to_string(), r"\\nas\music".to_string()]);
    }

    #[test]
    fn pasted_token_is_trimmed() {
        let mut settings = Settings {
            token: "  Bot abc.def.ghi  ".into(),
            owner: " 123 \"".into(),
            prefix: "   ".into(),
            volume: 900,
            ..Default::default()
        };
        settings.clamp();
        assert_eq!(settings.token, "abc.def.ghi");
        assert_eq!(settings.owner, "123");
        assert_eq!(settings.prefix, "!");
        assert_eq!(settings.volume, 200);
    }
}
