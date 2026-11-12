//! The music library: folders the owner adds in the window (a folder on this
//! PC or a network share like `\\nas\music`), scanned for tags and
//! searchable from Discord with `play local:` and `/local`.
//!
//! Discord never sends a path. It sends words to search for, or the short
//! key of a track that autocomplete offered. Only the window adds folders.

use crate::media::{Item, Kind};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

/// File types the library scans.
pub const LIBRARY_EXTS: &[&str] = &["mp3", "flac", "m4a", "ogg", "wav"];
/// Stop scanning one folder after this many tracks.
pub const FOLDER_LIMIT: usize = 100_000;
pub const INDEX_FILE: &str = "lantern-library.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub path: String,
    /// The library folder this track was found under.
    pub folder: String,
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub length: Option<u64>,
}

impl Track {
    /// Short stable key for autocomplete. Not a path.
    pub fn key(&self) -> String {
        format!("#{:016x}", fnv1a(self.path.to_lowercase().as_bytes()))
    }

    /// `Title · Artist · Album`, without empty parts.
    pub fn label(&self) -> String {
        let mut parts = vec![self.title.as_str()];
        if !self.artist.is_empty() {
            parts.push(&self.artist);
        }
        if !self.album.is_empty() {
            parts.push(&self.album);
        }
        parts.join(" · ")
    }

    pub fn item(&self) -> Item {
        let title = if self.artist.is_empty() { self.title.clone() } else { format!("{} - {}", self.artist, self.title) };
        Item {
            title,
            source: self.path.clone(),
            query: None,
            kind: Kind::File,
            length: self.length,
            allow_private_host: false,
            requested_by: None,
        }
    }

    fn haystack(&self) -> String {
        let file = Path::new(&self.path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        format!("{} {} {} {}", self.title, self.artist, self.album, file).to_lowercase()
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Index {
    pub tracks: Vec<Track>,
    /// Folder -> problem from the last scan, like an unreachable share.
    #[serde(default)]
    pub problems: HashMap<String, String>,
}

impl Index {
    /// Best matches first. Every word must appear in the title, artist, album, or file name.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&Track> {
        let query = query.trim().to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        if words.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(i32, &Track)> = self
            .tracks
            .iter()
            .filter_map(|track| {
                let hay = track.haystack();
                if !words.iter().all(|w| hay.contains(w)) {
                    return None;
                }
                let title = track.title.to_lowercase();
                let mut score = 0;
                if title == query {
                    score += 100;
                } else if title.starts_with(&query) {
                    score += 50;
                } else if title.contains(&query) {
                    score += 25;
                }
                score += words.iter().filter(|w| title.contains(*w)).count() as i32 * 5;
                score += words.iter().filter(|w| track.artist.to_lowercase().contains(*w)).count() as i32 * 3;
                Some((score, track))
            })
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.title.to_lowercase().cmp(&b.1.title.to_lowercase())));
        hits.into_iter().take(limit).map(|(_, t)| t).collect()
    }

    pub fn by_key(&self, key: &str) -> Option<&Track> {
        let key = key.trim().to_ascii_lowercase();
        self.tracks.iter().find(|t| t.key() == key)
    }

    /// `local:` text from Discord: an autocomplete key, or words to search for.
    pub fn find(&self, text: &str) -> Option<&Track> {
        let text = text.trim();
        if text.starts_with('#') {
            if let Some(track) = self.by_key(text) {
                return Some(track);
            }
        }
        self.search(text, 1).into_iter().next()
    }

    /// A library track at this path (case doesn't matter, `/` and `\\` are the same).
    pub fn by_path(&self, path: &str) -> Option<&Track> {
        let want = path.replace('/', "\\").to_lowercase();
        self.tracks.iter().find(|t| t.path.replace('/', "\\").to_lowercase() == want)
    }

    pub fn count_in(&self, folder: &str) -> usize {
        self.tracks.iter().filter(|t| t.folder == folder).count()
    }
}

fn store() -> &'static RwLock<Index> {
    static INDEX: OnceLock<RwLock<Index>> = OnceLock::new();
    INDEX.get_or_init(|| RwLock::new(Index::default()))
}

/// Runs `f` on the shared index.
pub fn with<T>(f: impl FnOnce(&Index) -> T) -> T {
    f(&store().read().unwrap_or_else(|e| e.into_inner()))
}

pub fn replace(index: Index) {
    *store().write().unwrap_or_else(|e| e.into_inner()) = index;
}

pub fn index_path() -> PathBuf {
    crate::settings::app_dir().join(INDEX_FILE)
}

/// Loads the index saved by the last scan, so search works before a rescan finishes.
pub fn load_saved(path: &Path) -> Index {
    crate::settings::read_text(path)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(index: &Index, path: &Path) -> Result<(), String> {
    let text = serde_json::to_string(index).map_err(|e| e.to_string())?;
    crate::settings::write_atomic(path, &text)
}

/// Scans every folder. A folder that can't be reached (a share that is off,
/// or a login Windows doesn't have) keeps the tracks from its last good scan
/// and gets a note in `problems`. Removed folders drop out.
pub fn scan(folders: &[String], previous: &Index) -> Index {
    let mut index = Index::default();
    for folder in folders {
        let root = Path::new(folder);
        match std::fs::read_dir(root) {
            Ok(_) => {
                let mut found = Vec::new();
                let mut failed = Vec::new();
                walk(root, folder, 0, &mut found, &mut failed);
                if found.len() >= FOLDER_LIMIT {
                    index.problems.insert(folder.clone(), format!("Stopped at {FOLDER_LIMIT} tracks."));
                } else if !failed.is_empty() {
                    // A share that drops mid-scan (or one locked subfolder) used to
                    // lose every track under it until the next good scan.
                    let kept = previous.tracks.iter().filter(|t| &t.folder == folder && failed.iter().any(|dir| is_under(&t.path, dir)));
                    found.extend(kept.cloned());
                    index.problems.insert(
                        folder.clone(),
                        format!("{} subfolder(s) couldn't be read. Kept their tracks from the last scan.", failed.len()),
                    );
                }
                index.tracks.extend(found);
            }
            Err(err) => {
                index.problems.insert(folder.clone(), unreachable_note(&err));
                index.tracks.extend(previous.tracks.iter().filter(|t| &t.folder == folder).cloned());
            }
        }
    }
    index.tracks.sort_by_key(|a| a.path.to_lowercase());
    index.tracks.dedup_by(|a, b| a.path.eq_ignore_ascii_case(&b.path));
    index
}

fn unreachable_note(err: &std::io::Error) -> String {
    match err.raw_os_error() {
        // ERROR_LOGON_FAILURE, ERROR_ACCESS_DENIED, ERROR_SESSION_CREDENTIAL_CONFLICT
        Some(1326) | Some(5) | Some(1219) => format!(
            "Can't open it ({err}). If this is a network share, open it once in File Explorer and tick \"Remember my credentials\", then Rescan."
        ),
        _ => format!("Can't reach it right now ({err}). Kept the tracks from the last scan."),
    }
}

/// True when `path` is inside `dir` (case and slash direction don't matter).
fn is_under(path: &str, dir: &Path) -> bool {
    let norm = |s: &str| s.replace('/', "\\").to_lowercase();
    let dir = norm(&dir.to_string_lossy());
    let dir = dir.trim_end_matches('\\');
    norm(path).strip_prefix(dir).is_some_and(|rest| rest.starts_with('\\'))
}

/// True when a folder in `wanted` wasn't part of the scan that just finished,
/// like one added while it ran.
pub fn unscanned(scanned: &[String], wanted: &[String]) -> bool {
    wanted.iter().any(|w| !scanned.iter().any(|s| s.eq_ignore_ascii_case(w)))
}

fn walk(dir: &Path, folder: &str, depth: usize, out: &mut Vec<Track>, failed: &mut Vec<PathBuf>) {
    if depth > 32 || out.len() >= FOLDER_LIMIT {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        failed.push(dir.to_path_buf());
        return;
    };
    for entry in read.flatten() {
        if out.len() >= FOLDER_LIMIT {
            return;
        }
        let path = entry.path();
        let hidden = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.'));
        if hidden {
            continue;
        }
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            walk(&path, folder, depth + 1, out, failed);
        } else if is_library_file(&path) {
            out.push(read_track(&path, folder));
        }
    }
}

pub fn is_library_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| LIBRARY_EXTS.iter().any(|want| ext.eq_ignore_ascii_case(want)))
}

/// Reads tags with lofty. A file without tags (or with broken ones) still
/// goes in, titled by its file name.
pub fn read_track(path: &Path, folder: &str) -> Track {
    use lofty::prelude::*;
    let mut track = Track {
        path: path.to_string_lossy().into_owned(),
        folder: folder.to_string(),
        title: crate::media::local_title(path),
        artist: String::new(),
        album: String::new(),
        length: None,
    };
    if let Ok(file) = lofty::read_from_path(path) {
        let secs = file.properties().duration().as_secs();
        if secs > 0 {
            track.length = Some(secs);
        }
        if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
            let clean = |v: Option<std::borrow::Cow<'_, str>>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            if let Some(title) = clean(tag.title()) {
                track.title = title;
            }
            track.artist = clean(tag.artist()).unwrap_or_default();
            track.album = clean(tag.album()).unwrap_or_default();
        }
    }
    track
}

/// Cleans a folder typed or picked in the window. Empty means nothing to add.
pub fn normalize_folder(text: &str) -> String {
    let text = text.trim().trim_matches('"').trim();
    let trimmed = text.trim_end_matches(['\\', '/']);
    if trimmed.is_empty() || trimmed.ends_with(':') {
        // `C:` alone means the current folder on that drive; keep the slash.
        return if text.is_empty() { String::new() } else { format!("{trimmed}\\") };
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_added_during_a_scan_is_noticed() {
        let scanned = vec!["C:\\Music".to_string()];
        assert!(!unscanned(&scanned, &["c:\\music".to_string()]));
        assert!(unscanned(&scanned, &["C:\\Music".to_string(), "D:\\More".to_string()]));
        assert!(!unscanned(&scanned, &[]));
    }

    #[test]
    fn is_under_needs_a_whole_folder_name() {
        assert!(is_under("C:\\Music\\Rock\\a.mp3", Path::new("c:/music/rock")));
        assert!(!is_under("C:\\Music\\Rockabilly\\a.mp3", Path::new("C:\\Music\\Rock")));
    }

    #[cfg(unix)]
    #[test]
    fn a_subfolder_that_fails_mid_scan_keeps_its_old_tracks() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("lantern-lib-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sub = root.join("locked");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join("top.mp3"), b"").unwrap();
        let folder = root.to_string_lossy().into_owned();
        let old = Track {
            path: sub.join("old.mp3").to_string_lossy().into_owned(),
            folder: folder.clone(),
            title: "old".into(),
            artist: String::new(),
            album: String::new(),
            length: None,
        };
        let previous = Index { tracks: vec![old.clone()], problems: HashMap::new() };
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();
        let blocked = std::fs::read_dir(&sub).is_err();
        let index = scan(std::slice::from_ref(&folder), &previous);
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&root);
        if !blocked {
            return; // running as root: permissions can't make a folder unreadable
        }
        assert!(index.tracks.iter().any(|t| t.path == old.path), "lost the tracks under the failed subfolder");
        assert!(index.tracks.iter().any(|t| t.title == "top"));
        assert!(index.problems.contains_key(&folder));
    }

    fn track(path: &str, title: &str, artist: &str, album: &str) -> Track {
        Track {
            path: path.into(),
            folder: "/m".into(),
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            length: None,
        }
    }

    fn sample() -> Index {
        Index {
            tracks: vec![
                track("/m/a.flac", "Hey Jude", "The Beatles", "Past Masters"),
                track("/m/b.mp3", "Jude", "Someone", "Other"),
                track("/m/c.mp3", "Yellow", "Coldplay", "Parachutes"),
            ],
            problems: HashMap::new(),
        }
    }

    #[test]
    fn search_matches_title_artist_and_album_words() {
        let index = sample();
        assert_eq!(index.search("beatles jude", 5)[0].title, "Hey Jude");
        assert_eq!(index.search("parachutes", 5)[0].title, "Yellow");
        assert_eq!(index.search("jude", 5).len(), 2);
        assert_eq!(index.search("jude", 5)[0].title, "Jude", "exact title wins");
        assert!(index.search("nothing here", 5).is_empty());
        assert!(index.search("   ", 5).is_empty());
    }

    #[test]
    fn keys_find_tracks_and_are_not_paths() {
        let index = sample();
        let key = index.tracks[2].key();
        assert!(key.starts_with('#') && !key.contains('/'));
        assert_eq!(index.find(&key).unwrap().title, "Yellow");
        assert_eq!(index.find("coldplay").unwrap().title, "Yellow");
    }

    #[test]
    fn a_path_from_discord_is_just_search_words() {
        let index = sample();
        // A path is not opened; its words don't match any tag, so nothing plays.
        assert!(index.find("C:\\Windows\\win.ini").is_none());
        assert!(index.find("/etc/passwd").is_none());
    }

    #[test]
    fn library_items_are_files_titled_by_tags() {
        let item = sample().tracks[0].item();
        assert_eq!(item.kind, Kind::File);
        assert_eq!(item.title, "The Beatles - Hey Jude");
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lantern-lib-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scan_finds_library_files_and_skips_others() {
        let root = temp("scan");
        std::fs::create_dir_all(root.join("Album")).unwrap();
        std::fs::write(root.join("Album/01 Song.mp3"), b"not really audio").unwrap();
        std::fs::write(root.join("Album/02 Song.FLAC"), b"x").unwrap();
        std::fs::write(root.join("Album/cover.jpg"), b"x").unwrap();
        std::fs::write(root.join(".hidden.mp3"), b"x").unwrap();
        let folder = root.to_string_lossy().into_owned();
        let index = scan(std::slice::from_ref(&folder), &Index::default());
        assert_eq!(index.tracks.len(), 2, "{:?}", index.tracks);
        assert_eq!(index.tracks[0].title, "01 Song", "untagged files use the file name");
        assert!(index.problems.is_empty());
    }

    #[test]
    fn an_unreachable_folder_keeps_its_last_tracks_and_notes_why() {
        let gone = temp("gone").join("missing").to_string_lossy().into_owned();
        let mut previous = Index::default();
        previous.tracks.push(Track { folder: gone.clone(), ..track("/x/old.mp3", "Old", "", "") });
        previous.tracks.push(Track { folder: "/removed".into(), ..track("/removed/r.mp3", "Removed", "", "") });
        let index = scan(std::slice::from_ref(&gone), &previous);
        assert_eq!(index.tracks.len(), 1);
        assert_eq!(index.tracks[0].title, "Old");
        assert!(index.problems.contains_key(&gone));
    }

    #[test]
    fn index_round_trips_through_the_file() {
        let path = temp("save").join(INDEX_FILE);
        save(&sample(), &path).unwrap();
        assert_eq!(load_saved(&path).tracks.len(), 3);
        assert!(load_saved(&path.with_file_name("missing.json")).tracks.is_empty());
    }

    #[test]
    fn folders_are_cleaned() {
        assert_eq!(normalize_folder(r#" "\\nas\music\" "#), r"\\nas\music");
        assert_eq!(normalize_folder("D:\\"), "D:\\");
        assert_eq!(normalize_folder("   "), "");
    }
}
