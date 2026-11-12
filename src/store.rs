//! Per-server data saved as `lantern-data.json` beside the executable:
//! volume, DJ role, autoplay, saved playlists, and play history.
//!
//! Every change is written straight away (through a temporary file). A file
//! that can't be read is copied to `lantern-data.json.bak` first.

use crate::media::Item;
use crate::settings::{app_dir, damaged_note, read_text, write_atomic};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

pub const HISTORY_LIMIT: usize = 50;
pub const PLAYLIST_LIMIT: usize = 50;
pub const PLAYLIST_TRACK_LIMIT: usize = 200;
pub const FAVORITES_LIMIT: usize = 200;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GuildData {
    /// 0–200. `None` uses the default volume from the window.
    pub volume: Option<i32>,
    /// Members with this role get full control in this server.
    pub dj_role: Option<u64>,
    pub autoplay: bool,
    /// Autoplay picks related SoundCloud tracks instead of YouTube ones (1.8).
    pub soundcloud_autoplay: bool,
    pub playlists: BTreeMap<String, Vec<Item>>,
    /// Newest last.
    pub history: VecDeque<Played>,
    /// Each person's favorites (Discord user ID → songs, oldest first).
    pub favorites: BTreeMap<String, Vec<Item>>,
    /// The current song and queue, kept so `restore` can put them back
    /// after Lantern restarts (1.7). Cleared by stop and idle leave.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub saved_queue: Vec<Item>,
    /// Where `play` was last used, for the restart offer.
    pub saved_text: Option<u64>,
}

/// Most songs kept for `restore`.
pub const SAVED_QUEUE_LIMIT: usize = 500;

/// What `restore` would put back: the song that was playing, then the queue.
pub fn queue_snapshot<'a>(now: Option<&'a Item>, upcoming: impl IntoIterator<Item = &'a Item>) -> Vec<Item> {
    now.into_iter().chain(upcoming).take(SAVED_QUEUE_LIMIT).map(|item| Item { requested_by: None, ..item.clone() }).collect()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Played {
    pub item: Item,
    /// Unix seconds.
    pub at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Data {
    guilds: BTreeMap<String, GuildData>,
}

pub struct Store {
    path: Option<PathBuf>,
    data: Mutex<Data>,
}

impl Store {
    pub fn path() -> PathBuf {
        app_dir().join("lantern-data.json")
    }

    /// Opens the file. The second value is a warning for the activity log.
    pub fn open(path: PathBuf) -> (Self, Option<String>) {
        let (data, warning) = match read_text(&path) {
            Ok(None) => (Data::default(), None),
            Ok(Some(text)) => match serde_json::from_str(&text) {
                Ok(data) => (data, None),
                Err(err) => (Data::default(), Some(damaged_note(&path, &err.to_string()))),
            },
            // The file is there but couldn't be read (locked, no permission, not
            // text). Saving would overwrite every playlist in it, so this session
            // keeps changes in memory only.
            Err(err) => {
                let note = format!(
                    "{} Changes from Discord won't be saved until Lantern restarts, so the file isn't overwritten.",
                    damaged_note(&path, &err.to_string()).replace(" Starting fresh.", "")
                );
                return (Self::memory(), Some(note));
            }
        };
        (Self { path: Some(path), data: Mutex::new(data) }, warning)
    }

    /// A store that never touches the disk.
    pub fn memory() -> Self {
        Self { path: None, data: Mutex::new(Data::default()) }
    }

    fn lock(&self) -> MutexGuard<'_, Data> {
        self.data.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn guild(&self, guild: u64) -> GuildData {
        self.lock().guilds.get(&guild.to_string()).cloned().unwrap_or_default()
    }

    /// Changes one server's data and saves the file. The second value is a save
    /// error when the in-memory change succeeded but the file could not be written.
    pub fn update<R>(&self, guild: u64, change: impl FnOnce(&mut GuildData) -> R) -> (R, Option<String>) {
        let mut data = self.lock();
        let result = change(data.guilds.entry(guild.to_string()).or_default());
        let err = self.path.as_ref().and_then(|path| persist(&data, path));
        (result, err)
    }

    fn attach_save_error(result: Result<String, String>, save_err: Option<String>) -> Result<String, String> {
        match (result, save_err) {
            (Ok(msg), None) => Ok(msg),
            (Ok(msg), Some(err)) => Err(format!("{msg} Couldn't save lantern-data.json: {err}")),
            (Err(err), _) => Err(err),
        }
    }

    pub fn save_playlist(&self, guild: u64, name: &str, items: Vec<Item>) -> Result<String, String> {
        let key = playlist_key(name)?;
        if items.is_empty() {
            return Err("Nothing to save. Play something first.".into());
        }
        let count = items.len().min(PLAYLIST_TRACK_LIMIT);
        let (result, save_err) = self.update(guild, |data| {
            if !data.playlists.contains_key(&key) && data.playlists.len() >= PLAYLIST_LIMIT {
                return Err(format!("This server already has {PLAYLIST_LIMIT} playlists. Delete one first."));
            }
            let replaced = data
                .playlists
                .insert(key.clone(), items.into_iter().take(PLAYLIST_TRACK_LIMIT).collect())
                .is_some();
            Ok(format!(
                "{} playlist **{key}** with {count} track{}.",
                if replaced { "Updated" } else { "Saved" },
                if count == 1 { "" } else { "s" }
            ))
        });
        Self::attach_save_error(result, save_err)
    }

    pub fn load_playlist(&self, guild: u64, name: &str) -> Result<Vec<Item>, String> {
        let key = playlist_key(name)?;
        self.guild(guild)
            .playlists
            .get(&key)
            .filter(|items| !items.is_empty())
            .cloned()
            .ok_or_else(|| format!("No playlist called **{key}**. See `playlists`."))
    }

    pub fn delete_playlist(&self, guild: u64, name: &str) -> Result<String, String> {
        let key = playlist_key(name)?;
        let (result, save_err) = self.update(guild, |data| match data.playlists.remove(&key) {
            Some(_) => Ok(format!("Deleted playlist **{key}**.")),
            None => Err(format!("No playlist called **{key}**.")),
        });
        Self::attach_save_error(result, save_err)
    }

    /// Name and track count, sorted by name.
    pub fn playlists(&self, guild: u64) -> Vec<(String, usize)> {
        self.guild(guild).playlists.iter().map(|(k, v)| (k.clone(), v.len())).collect()
    }

    pub fn add_history(&self, guild: u64, item: &Item) -> Option<String> {
        let at = chrono::Utc::now().timestamp();
        let (_, save_err) = self.update(guild, |data| {
            data.history.push_back(Played { item: item.clone(), at });
            while data.history.len() > HISTORY_LIMIT {
                data.history.pop_front();
            }
        });
        save_err
    }

    /// Hearts a song for one person. Saving the same song twice is a no-op.
    pub fn add_favorite(&self, guild: u64, user: u64, item: &Item) -> Result<String, String> {
        let (result, save_err) = self.update(guild, |data| {
            let list = data.favorites.entry(user.to_string()).or_default();
            if list.iter().any(|i| i.source == item.source) {
                return Ok(format!("**{}** is already in your favorites.", crate::bot::escape(&item.title)));
            }
            if list.len() >= FAVORITES_LIMIT {
                return Err(format!("You have {FAVORITES_LIMIT} favorites. Remove one with `fav remove` first."));
            }
            list.push(item.clone());
            Ok(format!("Added **{}** to your favorites ({}).", crate::bot::escape(&item.title), list.len()))
        });
        Self::attach_save_error(result, save_err)
    }

    pub fn favorites(&self, guild: u64, user: u64) -> Vec<Item> {
        self.guild(guild).favorites.get(&user.to_string()).cloned().unwrap_or_default()
    }

    /// `position` is 1-based, as `fav list` shows.
    pub fn remove_favorite(&self, guild: u64, user: u64, position: usize) -> Result<String, String> {
        let (result, save_err) = self.update(guild, |data| {
            let list = data.favorites.entry(user.to_string()).or_default();
            if position == 0 || position > list.len() {
                return Err("Say which one from `fav list`, like `fav remove 2`.".to_string());
            }
            let item = list.remove(position - 1);
            Ok(format!("Removed **{}** from your favorites.", crate::bot::escape(&item.title)))
        });
        Self::attach_save_error(result, save_err)
    }

    /// Newest first.
    /// Keeps the queue for `restore`. Writes only when it changed.
    pub fn set_saved_queue(&self, guild: u64, items: Vec<Item>, text: Option<u64>) -> Option<String> {
        let current = self.guild(guild);
        if current.saved_queue == items && (text.is_none() || current.saved_text == text) {
            return None;
        }
        self.update(guild, |d| {
            d.saved_queue = items;
            if text.is_some() {
                d.saved_text = text;
            }
        })
        .1
    }

    /// Takes the kept queue (it's forgotten once restored).
    pub fn take_saved_queue(&self, guild: u64) -> (Vec<Item>, Option<String>) {
        self.update(guild, |d| std::mem::take(&mut d.saved_queue))
    }

    /// Servers with a kept queue: (server, songs, text channel).
    pub fn saved_queues(&self) -> Vec<(u64, usize, Option<u64>)> {
        let data = self.lock();
        data.guilds
            .iter()
            .filter(|(_, g)| !g.saved_queue.is_empty())
            .filter_map(|(id, g)| Some((id.parse().ok()?, g.saved_queue.len(), g.saved_text)))
            .collect()
    }

    pub fn history(&self, guild: u64) -> Vec<Played> {
        self.guild(guild).history.into_iter().rev().collect()
    }
}

/// Playlist names are case-insensitive, 1–32 characters, letters, numbers, spaces, `-` and `_`.
pub fn playlist_key(name: &str) -> Result<String, String> {
    let key = name.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    if key.is_empty() {
        return Err("Give the playlist a name, like `save chill`.".into());
    }
    if key.chars().count() > 32 {
        return Err("Keep playlist names to 32 characters.".into());
    }
    if !key.chars().all(|c| c.is_alphanumeric() || c == ' ' || c == '-' || c == '_') {
        return Err("Use letters, numbers, spaces, `-` or `_` in playlist names.".into());
    }
    Ok(key)
}

fn persist(data: &Data, path: &Path) -> Option<String> {
    match serde_json::to_string_pretty(data) {
        Err(err) => Some(err.to_string()),
        Ok(text) => write_atomic(path, &text).err(),
    }
}

#[cfg(test)]
mod favorite_tests {
    use super::*;
    use crate::media::Kind;

    #[test]
    fn the_queue_is_kept_for_restore() {
        let store = Store::memory();
        let mut now = song(1);
        now.requested_by = Some(5);
        let snap = queue_snapshot(Some(&now), [song(2), song(3)].iter());
        assert_eq!(snap.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), ["Song 1", "Song 2", "Song 3"]);
        assert!(snap.iter().all(|i| i.requested_by.is_none()));
        assert!(store.set_saved_queue(9, snap.clone(), Some(77)).is_none());
        assert_eq!(store.saved_queues(), vec![(9, 3, Some(77))]);
        let (back, _) = store.take_saved_queue(9);
        assert_eq!(back, snap);
        assert!(store.saved_queues().is_empty());
        assert!(queue_snapshot(None, std::iter::empty()).is_empty());
    }

    fn song(n: u32) -> Item {
        Item { title: format!("Song {n}"), source: format!("https://youtu.be/{n}"), query: None, kind: Kind::YouTube, length: None, allow_private_host: false, requested_by: None }
    }

    #[test]
    fn favorites_are_per_person_and_not_duplicated() {
        let store = Store::memory();
        assert!(store.add_favorite(1, 10, &song(1)).is_ok());
        assert!(store.add_favorite(1, 10, &song(1)).unwrap().contains("already"));
        assert!(store.add_favorite(1, 10, &song(2)).is_ok());
        assert!(store.add_favorite(1, 20, &song(3)).is_ok());
        assert_eq!(store.favorites(1, 10).len(), 2);
        assert_eq!(store.favorites(1, 20).len(), 1);
        assert!(store.remove_favorite(1, 10, 3).is_err());
        assert!(store.remove_favorite(1, 10, 1).is_ok());
        assert_eq!(store.favorites(1, 10)[0].title, "Song 2");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{Item, Kind};
    use crate::settings::backup_path;

    fn item(t: &str) -> Item {
        Item { title: t.into(), source: format!("https://youtu.be/{t}"), query: None, kind: Kind::YouTube, length: Some(100), allow_private_host: false, requested_by: None }
    }

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "lantern-test-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ))
    }

    #[test]
    fn a_file_that_cannot_be_read_is_never_overwritten() {
        let path = temp_path();
        // Not UTF-8, so reading it as text fails the way a locked file does.
        let original = vec![0xff, 0xfe, 0x00, 0x81, 0x7b];
        std::fs::write(&path, &original).unwrap();
        let (store, warning) = Store::open(path.clone());
        assert!(warning.is_some());
        let result = store.save_playlist(1, "mix", vec![item("a")]);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read(&path).unwrap(), original, "the unreadable file was overwritten");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn playlist_key_normalizes_and_validates() {
        assert_eq!(playlist_key("  Chill   Vibes ").unwrap(), "chill vibes");
        assert!(playlist_key("").is_err());
        assert!(playlist_key("   ").is_err());
        assert!(playlist_key(&"a".repeat(33)).is_err());
        assert!(playlist_key("bad/name").is_err());
    }

    #[test]
    fn save_load_delete_playlist() {
        let s = Store::memory();
        let msg = s.save_playlist(1, "Chill", vec![item("a"), item("b")]).unwrap();
        assert!(msg.starts_with("Saved"));
        assert_eq!(s.load_playlist(1, "chill").unwrap().len(), 2);

        let msg = s.save_playlist(1, "chill", vec![item("c")]).unwrap();
        assert!(msg.starts_with("Updated"));
        assert_eq!(s.load_playlist(1, "chill").unwrap().len(), 1);

        assert!(s.save_playlist(1, "empty", vec![]).is_err());
        assert!(s.load_playlist(1, "missing").is_err());

        assert!(s.delete_playlist(1, "chill").is_ok());
        assert!(s.delete_playlist(1, "chill").is_err());
    }

    #[test]
    fn an_empty_playlist_in_the_file_is_not_loadable() {
        let s = Store::memory();
        let _ = s.update(1, |d| {
            d.playlists.insert("hollow".into(), Vec::new());
        });
        assert!(s.load_playlist(1, "hollow").is_err());
    }

    #[test]
    fn playlists_sorted_with_counts() {
        let s = Store::memory();
        s.save_playlist(1, "beta", vec![item("a")]).unwrap();
        s.save_playlist(1, "alpha", vec![item("a"), item("b")]).unwrap();
        assert_eq!(s.playlists(1), vec![("alpha".into(), 2), ("beta".into(), 1)]);
    }

    #[test]
    fn playlist_track_limit() {
        let s = Store::memory();
        let items: Vec<Item> = (0..250).map(|i| item(&format!("t{i}"))).collect();
        s.save_playlist(1, "big", items).unwrap();
        assert_eq!(s.load_playlist(1, "big").unwrap().len(), PLAYLIST_TRACK_LIMIT);
    }

    #[test]
    fn history_newest_first_and_capped() {
        let s = Store::memory();
        for i in 0..60 {
            s.add_history(1, &item(&format!("t{i}")));
        }
        let h = s.history(1);
        assert_eq!(h.len(), HISTORY_LIMIT);
        assert_eq!(h[0].item.title, "t59");
        assert_eq!(h.last().unwrap().item.title, "t10");
    }

    #[test]
    fn guild_isolation() {
        let s = Store::memory();
        s.save_playlist(1, "mix", vec![item("a")]).unwrap();
        s.save_playlist(2, "mix", vec![item("b"), item("c")]).unwrap();
        assert_eq!(s.load_playlist(1, "mix").unwrap().len(), 1);
        assert_eq!(s.load_playlist(2, "mix").unwrap().len(), 2);
        let _ = s.update(1, |d| d.volume = Some(10));
        let _ = s.update(2, |d| d.volume = Some(90));
        assert_eq!(s.guild(1).volume, Some(10));
        assert_eq!(s.guild(2).volume, Some(90));
    }

    #[test]
    fn update_returns_value() {
        let s = Store::memory();
        let (r, err) = s.update(1, |d| {
            d.volume = Some(30);
            42
        });
        assert_eq!(r, 42);
        assert!(err.is_none());
    }

    #[test]
    fn persists_across_reopen() {
        let path = temp_path();
        {
            let (s, w) = Store::open(path.clone());
            assert!(w.is_none());
            let _ = s.update(1, |d| d.volume = Some(77));
            let _ = s.update(1, |d| d.dj_role = Some(999));
            s.save_playlist(1, "set", vec![item("a"), item("b")]).unwrap();
        }
        let (s, w) = Store::open(path.clone());
        assert!(w.is_none());
        let g = s.guild(1);
        assert_eq!(g.volume, Some(77));
        assert_eq!(g.dj_role, Some(999));
        assert_eq!(g.playlists.get("set").unwrap().len(), 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn file_saved_by_notepad_with_a_byte_order_mark_loads() {
        let path = temp_path();
        std::fs::write(&path, "\u{feff}{\"guilds\":{\"1\":{\"volume\":42}}}").unwrap();
        let (s, w) = Store::open(path.clone());
        assert!(w.is_none(), "{w:?}");
        assert_eq!(s.guild(1).volume, Some(42));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn unreadable_text_is_backed_up_before_it_is_overwritten() {
        let path = temp_path();
        std::fs::write(&path, b"{\"guilds\":{\"caf\xe9\":{}}}").unwrap();
        let (s, w) = Store::open(path.clone());
        assert!(w.is_some());
        let _ = s.update(1, |d| d.volume = Some(5));
        let backup = backup_path(&path);
        assert_eq!(std::fs::read(&backup).unwrap(), b"{\"guilds\":{\"caf\xe9\":{}}}");
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(backup);
    }

    #[test]
    fn a_missing_file_is_not_a_warning() {
        let (_s, w) = Store::open(temp_path());
        assert!(w.is_none());
    }

    #[test]
    fn updates_from_many_threads_all_land_in_the_file() {
        let path = temp_path();
        let (store, _) = Store::open(path.clone());
        let store = std::sync::Arc::new(store);
        let threads: Vec<_> = (1..=8u64)
            .map(|guild| {
                let store = store.clone();
                std::thread::spawn(move || {
                    for level in 0..25 {
                        let _ = store.update(guild, |d| d.volume = Some(level));
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let (again, warning) = Store::open(path.clone());
        assert!(warning.is_none(), "{warning:?}");
        for guild in 1..=8 {
            assert_eq!(again.guild(guild).volume, Some(24), "server {guild}");
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn corrupt_file_backs_up_and_warns() {
        let path = temp_path();
        std::fs::write(&path, b"not json at all {").unwrap();
        let (_s, w) = Store::open(path.clone());
        assert!(w.is_some());
        let backup = backup_path(&path);
        assert!(backup.exists());
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(backup);
    }

    #[test]
    fn update_reports_when_the_data_file_cannot_be_written() {
        let dir = std::env::temp_dir().join(format!("lantern-store-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lantern-data.json");
        let (store, _) = Store::open(path);
        // Replace the folder with a plain file, so the save can't create anything
        // there. Unlike a read-only folder, this also fails for root (buildbox runs as root).
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::write(&dir, b"not a folder").unwrap();
        let (_, err) = store.update(1, |d| d.volume = Some(9));
        assert!(err.is_some(), "{err:?}");
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn save_playlist_surfaces_a_disk_error() {
        let dir = std::env::temp_dir().join(format!("lantern-save-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lantern-data.json");
        let (store, _) = Store::open(path);
        // Replace the folder with a plain file, so the save can't create anything
        // there. Unlike a read-only folder, this also fails for root (buildbox runs as root).
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::write(&dir, b"not a folder").unwrap();
        let result = store.save_playlist(1, "mix", vec![item("a")]);
        let err = result.expect_err("save should fail");
        assert!(err.contains("lantern-data.json"), "{err}");
        let _ = std::fs::remove_file(&dir);
    }
}

