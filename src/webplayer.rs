//! The web player (1.11): a queue of its own that plays in the browser, on the
//! speakers of whatever computer or phone has the page open. It never touches
//! the Discord queue; both can play at once.
//!
//! The server keeps the queue (saved to `lantern-webplayer.json`) and which
//! entry is current. The browser plays `/api/web/stream/{id}` (Ogg Opus from
//! [`crate::transcode`]), keeps its own volume and position, and asks for the
//! next entry when one ends.

use crate::media::{Item, Kind, Playable};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const FILE: &str = "lantern-webplayer.json";
/// The most entries the web queue holds.
pub const MAX_ENTRIES: usize = 2000;
/// Streams open at once (a seek opens a new one while the old one closes).
pub const MAX_STREAMS: usize = 6;
/// A looked-up YouTube or SoundCloud stream is reused for seeks this long.
const REUSE: Duration = Duration::from_secs(20 * 60);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

impl Repeat {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "all" | "queue" => Some(Self::All),
            "one" | "track" => Some(Self::One),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// After everything.
    End,
    /// Right after the current entry.
    Next,
    /// Right after the current entry, and it becomes current.
    Now,
}

impl Placement {
    pub fn parse(text: Option<&str>) -> Self {
        match text.map(|t| t.trim().to_ascii_lowercase()).as_deref() {
            Some("next") => Self::Next,
            Some("now") | Some("play") => Self::Now,
            _ => Self::End,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub item: Item,
    /// Why it last failed to play, for the page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The web player's queue: a list with a cursor, so Previous works and played
/// songs stay listed above the current one.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WebQueue {
    pub entries: Vec<Entry>,
    pub current: Option<u64>,
    #[serde(default)]
    pub repeat: Repeat,
    #[serde(default)]
    next_id: u64,
}

impl WebQueue {
    pub fn position(&self, id: u64) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }

    fn current_index(&self) -> Option<usize> {
        self.current.and_then(|id| self.position(id))
    }

    pub fn entry(&self, id: u64) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn entry_mut(&mut self, id: u64) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    /// Adds items; returns how many went in (the queue is capped at [`MAX_ENTRIES`]).
    pub fn add(&mut self, items: Vec<Item>, placement: Placement) -> usize {
        let room = MAX_ENTRIES.saturating_sub(self.entries.len());
        let mut new: Vec<Entry> = Vec::new();
        for mut item in items.into_iter().take(room) {
            item.requested_by = None;
            self.next_id += 1;
            new.push(Entry { id: self.next_id, item, error: None });
        }
        let added = new.len();
        if added == 0 {
            return 0;
        }
        let first = new[0].id;
        let at = match (placement, self.current_index()) {
            (Placement::End, _) => self.entries.len(),
            (_, Some(i)) => i + 1,
            (_, None) => self.entries.len(),
        };
        self.entries.splice(at..at, new);
        if placement == Placement::Now || self.current.is_none() {
            self.current = Some(first);
        }
        added
    }

    /// Removes an entry. If it was current, the one after it (if any) becomes current.
    pub fn remove(&mut self, id: u64) -> bool {
        let Some(i) = self.position(id) else { return false };
        self.entries.remove(i);
        if self.current == Some(id) {
            self.current = self.entries.get(i).map(|e| e.id);
        }
        true
    }

    /// Moves an entry to index `to` (clamped).
    pub fn move_to(&mut self, id: u64, to: usize) -> bool {
        let Some(from) = self.position(id) else { return false };
        let entry = self.entries.remove(from);
        let to = to.min(self.entries.len());
        self.entries.insert(to, entry);
        true
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.current = None;
    }

    /// Removes everything before the current entry.
    pub fn clear_played(&mut self) {
        if let Some(i) = self.current_index() {
            self.entries.drain(..i);
        }
    }

    pub fn select(&mut self, id: u64) -> bool {
        if self.position(id).is_some() {
            self.current = Some(id);
            true
        } else {
            false
        }
    }

    /// The entry after the current one (wrapping with Repeat all, the same one
    /// with Repeat one when `ended` says the song played out). None at the end.
    pub fn next(&mut self, ended: bool) -> Option<u64> {
        if ended && self.repeat == Repeat::One && self.current_index().is_some() {
            return self.current;
        }
        let next = match self.current_index() {
            Some(i) if i + 1 < self.entries.len() => Some(self.entries[i + 1].id),
            Some(_) if self.repeat != Repeat::Off => self.entries.first().map(|e| e.id),
            Some(_) => None,
            None => self.entries.first().map(|e| e.id),
        };
        self.current = next;
        next
    }

    pub fn previous(&mut self) -> Option<u64> {
        let previous = match self.current_index() {
            Some(0) if self.repeat == Repeat::All => self.entries.last().map(|e| e.id),
            Some(0) => self.current,
            Some(i) => Some(self.entries[i - 1].id),
            None => self.entries.last().map(|e| e.id),
        };
        self.current = previous;
        previous
    }

    /// Shuffles the entries after the current one.
    pub fn shuffle(&mut self) {
        let start = self.current_index().map_or(0, |i| i + 1);
        fastrand::shuffle(&mut self.entries[start..]);
    }
}

pub fn is_live(item: &Item) -> bool {
    item.kind == Kind::Radio
}

/// Where a song comes from, for the page.
pub fn source_label(item: &Item) -> &'static str {
    match item.kind {
        Kind::YouTube | Kind::Search => "YouTube",
        Kind::SoundCloud => "SoundCloud",
        Kind::File => "Library",
        Kind::Radio => "Radio",
        Kind::Direct => "Link",
    }
}

/// The web player: the queue, its file, and the streams in flight.
pub struct WebPlayer {
    queue: Mutex<WebQueue>,
    path: PathBuf,
    found: Mutex<HashMap<u64, (Instant, Playable)>>,
    streams: Arc<AtomicUsize>,
}

/// Counts an open stream until dropped.
pub struct StreamSlot(Arc<AtomicUsize>);

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl WebPlayer {
    /// Loads the saved queue (an unreadable file starts empty and is kept as `.bak`).
    pub fn load(path: PathBuf) -> (Arc<Self>, Option<String>) {
        let mut problem = None;
        let queue = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<WebQueue>(&text) {
                Ok(mut queue) => {
                    let max = queue.entries.iter().map(|e| e.id).max().unwrap_or(0);
                    queue.next_id = queue.next_id.max(max);
                    if queue.current.is_some_and(|id| queue.position(id).is_none()) {
                        queue.current = None;
                    }
                    queue
                }
                Err(err) => {
                    let _ = std::fs::copy(&path, crate::settings::backup_path(&path));
                    problem = Some(format!("Web player: {} couldn't be read ({err}); starting with an empty queue.", path.display()));
                    WebQueue::default()
                }
            },
            Err(_) => WebQueue::default(),
        };
        (Arc::new(Self { queue: Mutex::new(queue), path, found: Mutex::new(HashMap::new()), streams: Arc::new(AtomicUsize::new(0)) }), problem)
    }

    pub fn snapshot(&self) -> WebQueue {
        self.queue.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Changes the queue and saves it.
    pub fn change<T>(&self, f: impl FnOnce(&mut WebQueue) -> T) -> Result<T, String> {
        let (out, text) = {
            let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            let out = f(&mut queue);
            let ids: Vec<u64> = queue.entries.iter().map(|e| e.id).collect();
            self.found.lock().unwrap_or_else(|p| p.into_inner()).retain(|id, _| ids.contains(id));
            (out, serde_json::to_string(&*queue).map_err(|e| e.to_string())?)
        };
        crate::settings::write_atomic(&self.path, &text).map_err(|e| format!("Couldn't save {FILE}: {e}"))?;
        Ok(out)
    }

    /// Takes a stream slot, or None when too many are open.
    pub fn open_slot(&self) -> Option<StreamSlot> {
        let before = self.streams.fetch_add(1, Ordering::AcqRel);
        let slot = StreamSlot(self.streams.clone());
        (before < MAX_STREAMS).then_some(slot)
    }

    /// The stream for an entry, reusing a recent lookup (YouTube links last hours;
    /// a seek shouldn't search again). Radio is looked up fresh every time.
    pub async fn playable(&self, id: u64, item: &Item) -> Result<Playable, String> {
        if let Some((at, found)) = self.found.lock().unwrap_or_else(|p| p.into_inner()).get(&id) {
            if at.elapsed() < REUSE {
                return Ok(found.clone());
            }
        }
        let found = crate::media::materialize(item).await?;
        if !found.live_stream && found.path.is_none() {
            self.found.lock().unwrap_or_else(|p| p.into_inner()).insert(id, (Instant::now(), found.clone()));
        }
        Ok(found)
    }

    /// Forget a lookup that didn't play (a refused YouTube link), so the next try looks again.
    pub fn forget(&self, id: u64) {
        self.found.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
    }

    /// Remember a better lookup (the next YouTube route) for this entry.
    pub fn remember(&self, id: u64, found: &Playable) {
        if !found.live_stream && found.path.is_none() {
            self.found.lock().unwrap_or_else(|p| p.into_inner()).insert(id, (Instant::now(), found.clone()));
        }
    }

    /// Notes what the lookup learned (a search's real title and length) or why it failed.
    pub fn note(&self, id: u64, error: Option<String>, length: Option<u64>) {
        let _ = self.change(|queue| {
            if let Some(entry) = queue.entry_mut(id) {
                entry.error = error;
                if entry.item.length.is_none() && entry.item.kind != Kind::Radio {
                    entry.item.length = length;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(n: u32) -> Item {
        Item::new(format!("Song {n}"), format!("https://example.com/{n}.mp3"), Kind::Direct)
    }

    fn titles(q: &WebQueue) -> Vec<String> {
        q.entries.iter().map(|e| e.item.title.clone()).collect()
    }

    fn current_title(q: &WebQueue) -> Option<String> {
        q.current.and_then(|id| q.entry(id)).map(|e| e.item.title.clone())
    }

    #[test]
    fn the_first_add_becomes_current_and_later_ones_wait() {
        let mut q = WebQueue::default();
        assert_eq!(q.add(vec![item(1), item(2)], Placement::End), 2);
        assert_eq!(current_title(&q).as_deref(), Some("Song 1"));
        q.add(vec![item(3)], Placement::End);
        assert_eq!(current_title(&q).as_deref(), Some("Song 1"));
        assert_eq!(titles(&q), ["Song 1", "Song 2", "Song 3"]);
        // After the queue played out, the next song added is the one to play.
        q.next(true);
        q.next(true);
        q.next(true);
        assert_eq!(q.current, None);
        q.add(vec![item(4)], Placement::End);
        assert_eq!(current_title(&q).as_deref(), Some("Song 4"));
    }

    #[test]
    fn next_and_now_go_right_after_the_current_song() {
        let mut q = WebQueue::default();
        q.add(vec![item(1), item(2), item(3)], Placement::End);
        q.add(vec![item(4)], Placement::Next);
        assert_eq!(titles(&q), ["Song 1", "Song 4", "Song 2", "Song 3"]);
        q.add(vec![item(5)], Placement::Now);
        assert_eq!(titles(&q), ["Song 1", "Song 5", "Song 4", "Song 2", "Song 3"]);
        assert_eq!(current_title(&q).as_deref(), Some("Song 5"));
    }

    #[test]
    fn next_and_previous_walk_the_list_and_stop_at_the_end() {
        let mut q = WebQueue::default();
        q.add(vec![item(1), item(2)], Placement::End);
        assert!(q.next(true).is_some());
        assert_eq!(current_title(&q).as_deref(), Some("Song 2"));
        assert_eq!(q.next(true), None, "the end of the queue stops");
        assert_eq!(q.current, None);
        // Previous from stopped picks the last song again.
        q.previous();
        assert_eq!(current_title(&q).as_deref(), Some("Song 2"));
        q.previous();
        q.previous();
        assert_eq!(current_title(&q).as_deref(), Some("Song 1"), "previous stays on the first song");
    }

    #[test]
    fn repeat_all_wraps_and_repeat_one_replays_only_when_a_song_ends() {
        let mut q = WebQueue::default();
        q.add(vec![item(1), item(2)], Placement::End);
        q.repeat = Repeat::All;
        q.next(true);
        q.next(true);
        assert_eq!(current_title(&q).as_deref(), Some("Song 1"));
        q.repeat = Repeat::One;
        q.next(true);
        assert_eq!(current_title(&q).as_deref(), Some("Song 1"));
        q.next(false); // the Next button still moves on
        assert_eq!(current_title(&q).as_deref(), Some("Song 2"));
    }

    #[test]
    fn removing_the_current_song_moves_to_the_one_after() {
        let mut q = WebQueue::default();
        q.add(vec![item(1), item(2), item(3)], Placement::End);
        let first = q.current.unwrap();
        assert!(q.remove(first));
        assert_eq!(current_title(&q).as_deref(), Some("Song 2"));
        let last = q.entries[1].id;
        q.select(last);
        q.remove(last);
        assert_eq!(q.current, None);
        assert!(!q.remove(999));
    }

    #[test]
    fn move_reorders_and_shuffle_leaves_played_songs_alone() {
        let mut q = WebQueue::default();
        q.add((1..=6).map(item).collect(), Placement::End);
        let six = q.entries[5].id;
        q.move_to(six, 0);
        assert_eq!(titles(&q)[0], "Song 6");
        q.move_to(six, 99);
        assert_eq!(titles(&q)[5], "Song 6");
        q.next(false);
        q.next(false); // current = Song 3
        q.shuffle();
        assert_eq!(&titles(&q)[..3], ["Song 1", "Song 2", "Song 3"]);
        q.clear_played();
        assert_eq!(titles(&q)[0], "Song 3");
        q.clear();
        assert!(q.entries.is_empty() && q.current.is_none());
    }

    #[test]
    fn the_queue_is_capped() {
        let mut q = WebQueue::default();
        assert_eq!(q.add((0..MAX_ENTRIES as u32 + 5).map(item).collect(), Placement::End), MAX_ENTRIES);
        assert_eq!(q.add(vec![item(1)], Placement::End), 0);
    }

    #[test]
    fn the_queue_survives_a_restart_and_a_damaged_file_is_kept() {
        let dir = std::env::temp_dir().join(format!("lantern-webplayer-test-{}", fastrand::u64(..)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);
        let (player, problem) = WebPlayer::load(path.clone());
        assert!(problem.is_none());
        player.change(|q| q.add(vec![item(1), item(2)], Placement::End)).unwrap();
        player.change(|q| q.repeat = Repeat::All).unwrap();
        let (again, _) = WebPlayer::load(path.clone());
        let q = again.snapshot();
        assert_eq!(titles(&q), ["Song 1", "Song 2"]);
        assert_eq!(q.repeat, Repeat::All);
        // New ids never reuse old ones.
        again.change(|q| q.add(vec![item(3)], Placement::End)).unwrap();
        let ids: Vec<u64> = again.snapshot().entries.iter().map(|e| e.id).collect();
        assert_eq!(ids, [1, 2, 3]);
        std::fs::write(&path, "{ not json").unwrap();
        let (broken, problem) = WebPlayer::load(path.clone());
        assert!(problem.is_some());
        assert!(broken.snapshot().entries.is_empty());
        assert!(crate::settings::backup_path(&path).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stream_slots_are_limited_and_given_back() {
        let (player, _) = WebPlayer::load(std::env::temp_dir().join("lantern-webplayer-unused.json"));
        let slots: Vec<_> = (0..MAX_STREAMS).map(|_| player.open_slot().expect("slot")).collect();
        assert!(player.open_slot().is_none());
        drop(slots);
        assert!(player.open_slot().is_some());
    }

    #[test]
    fn words_parse_to_placements_and_repeat_modes() {
        assert_eq!(Placement::parse(Some("now")), Placement::Now);
        assert_eq!(Placement::parse(Some("next")), Placement::Next);
        assert_eq!(Placement::parse(None), Placement::End);
        assert_eq!(Repeat::parse("track"), Some(Repeat::One));
        assert_eq!(Repeat::parse("x"), None);
    }
}
