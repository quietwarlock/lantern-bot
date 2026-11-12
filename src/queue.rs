//! The upcoming list for one server and the pure rules around it:
//! remove, move, clear, shuffle, loop, and what happens after a track ends.
//! No Discord or audio code lives here, so all of it is unit tested.

use crate::media::Item;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoopMode {
    #[default]
    Off,
    Track,
    Queue,
}

impl LoopMode {
    /// Accepts off/track/queue and a few obvious synonyms.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "no" | "0" | "false" => Some(Self::Off),
            "track" | "song" | "one" | "this" | "1" => Some(Self::Track),
            "queue" | "all" | "list" | "on" | "true" => Some(Self::Queue),
            _ => None,
        }
    }

    /// Off → Track → Queue → Off, for the loop button.
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Track,
            Self::Track => Self::Queue,
            Self::Queue => Self::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Track => "track",
            Self::Queue => "queue",
        }
    }
}

/// How a track stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// Played to the end.
    Finished,
    /// Someone skipped it.
    Skipped,
    /// It couldn't be loaded or the stream broke.
    Failed,
}

#[derive(Clone, Debug, Default)]
pub struct Queue {
    pub upcoming: VecDeque<Item>,
    pub loop_mode: LoopMode,
}

impl Queue {
    pub fn len(&self) -> usize {
        self.upcoming.len()
    }

    pub fn is_empty(&self) -> bool {
        self.upcoming.is_empty()
    }

    pub fn push(&mut self, items: impl IntoIterator<Item = Item>) {
        self.upcoming.extend(items);
    }

    pub fn pop_next(&mut self) -> Option<Item> {
        self.upcoming.pop_front()
    }

    /// Removes the track at a 1-based position, as shown by `queue`.
    pub fn remove(&mut self, position: usize) -> Result<Item, String> {
        self.check(position)?;
        Ok(self.upcoming.remove(position - 1).expect("checked"))
    }

    /// Moves a track from one 1-based position to another.
    pub fn move_item(&mut self, from: usize, to: usize) -> Result<Item, String> {
        self.check(from)?;
        self.check(to)?;
        let item = self.upcoming.remove(from - 1).expect("checked");
        self.upcoming.insert(to - 1, item.clone());
        Ok(item)
    }

    /// Empties the upcoming list. Returns how many tracks were removed.
    pub fn clear(&mut self) -> usize {
        let count = self.upcoming.len();
        self.upcoming.clear();
        count
    }

    pub fn shuffle(&mut self) {
        fastrand::shuffle(self.upcoming.make_contiguous());
    }

    /// Applies the loop mode once a track stops. A failed track is never
    /// repeated, so a broken link can't loop forever.
    pub fn after_track(&mut self, item: Item, ended: Ended) {
        match (self.loop_mode, ended) {
            (_, Ended::Failed) | (LoopMode::Off, _) | (LoopMode::Track, Ended::Skipped) => {}
            (LoopMode::Track, Ended::Finished) => self.upcoming.push_front(item),
            (LoopMode::Queue, _) => self.upcoming.push_back(item),
        }
    }

    fn check(&self, position: usize) -> Result<(), String> {
        if self.upcoming.is_empty() {
            return Err("The queue is empty.".into());
        }
        if position == 0 || position > self.upcoming.len() {
            return Err(format!("Pick a number from 1 to {}.", self.upcoming.len()));
        }
        Ok(())
    }
}

/// Reads `1:23`, `01:02:03`, `90`, `90s`, `2m`, or `1m30s` as seconds.
pub fn parse_time(text: &str) -> Option<u64> {
    let text = text.trim().to_ascii_lowercase();
    if text.is_empty() {
        return None;
    }
    if text.contains(':') {
        let parts: Vec<&str> = text.split(':').collect();
        if parts.len() > 3 || parts.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit())) {
            return None;
        }
        let mut total = 0u64;
        for (index, part) in parts.iter().enumerate() {
            let value: u64 = part.parse().ok()?;
            if index > 0 && value >= 60 {
                return None;
            }
            total = total.checked_mul(60)?.checked_add(value)?;
        }
        return Some(total);
    }
    if text.chars().all(|c| c.is_ascii_digit()) {
        return text.parse().ok();
    }
    let mut total = 0u64;
    let mut number = String::new();
    let mut seen_unit = false;
    for c in text.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let scale = match c {
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => return None,
        };
        let value: u64 = number.parse().ok()?;
        total = total.checked_add(value.checked_mul(scale)?)?;
        number.clear();
        seen_unit = true;
    }
    if !number.is_empty() || !seen_unit {
        return None;
    }
    Some(total)
}

/// Formats seconds as `m:ss` or `h:mm:ss`.
pub fn clock(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes}:{secs:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{Item, Kind};

    fn item(t: &str) -> Item {
        Item {
            title: t.into(),
            source: format!("https://youtu.be/{t}"),
            query: None,
            kind: Kind::YouTube,
            length: None,
            allow_private_host: false,
            requested_by: None,
        }
    }

    fn q(ts: &[&str]) -> Queue {
        let mut q = Queue::default();
        q.push(ts.iter().copied().map(item));
        q
    }

    fn titles(q: &Queue) -> Vec<String> {
        q.upcoming.iter().map(|i| i.title.clone()).collect()
    }

    fn assert_titles(q: &Queue, expected: &[&str]) {
        assert_eq!(
            titles(q),
            expected.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn remove_works() {
        let mut q = q(&["a", "b", "c"]);
        assert_eq!(q.remove(2).unwrap().title, "b");
        assert_titles(&q, &["a", "c"]);
        assert_eq!(q.remove(0).err().unwrap(), "Pick a number from 1 to 2.");
        assert_eq!(q.remove(3).err().unwrap(), "Pick a number from 1 to 2.");
        assert_eq!(
            Queue::default().remove(1).err().unwrap(),
            "The queue is empty."
        );
    }

    #[test]
    fn move_item_works() {
        let mut q = q(&["a", "b", "c", "d"]);
        assert_eq!(q.move_item(1, 3).unwrap().title, "a");
        assert_titles(&q, &["b", "c", "a", "d"]);
        assert_eq!(q.move_item(4, 1).unwrap().title, "d");
        assert_titles(&q, &["d", "b", "c", "a"]);
        assert_eq!(q.move_item(2, 2).unwrap().title, "b");
        assert_titles(&q, &["d", "b", "c", "a"]);
        assert_eq!(q.move_item(0, 1).err().unwrap(), "Pick a number from 1 to 4.");
        assert_eq!(q.move_item(1, 5).err().unwrap(), "Pick a number from 1 to 4.");
    }

    #[test]
    fn clear_works() {
        let mut q = q(&["a", "b"]);
        assert_eq!(q.clear(), 2);
        assert!(q.is_empty());
        assert_eq!(q.clear(), 0);
    }

    #[test]
    fn shuffle_keeps_multiset() {
        let mut q = Queue::default();
        for i in 0..20 {
            q.push(std::iter::once(item(&format!("t{i}"))));
        }
        let mut before = titles(&q);
        before.sort();
        q.shuffle();
        let mut after = titles(&q);
        after.sort();
        assert_eq!(before, after);
    }

    #[test]
    fn after_track_all_modes() {
        for mode in [LoopMode::Off, LoopMode::Track, LoopMode::Queue] {
            for ended in [Ended::Finished, Ended::Skipped, Ended::Failed] {
                let mut q = q(&["a", "b"]);
                q.loop_mode = mode;
                q.after_track(item("x"), ended);
                let got = titles(&q);
                let expected: Vec<String> = match (mode, ended) {
                    (LoopMode::Track, Ended::Finished) => vec!["x".into(), "a".into(), "b".into()],
                    (LoopMode::Queue, Ended::Finished) | (LoopMode::Queue, Ended::Skipped) => {
                        vec!["a".into(), "b".into(), "x".into()]
                    }
                    _ => vec!["a".into(), "b".into()],
                };
                assert_eq!(got, expected, "{mode:?} {ended:?}");
            }
        }
    }

    #[test]
    fn loop_mode_parse_and_next() {
        for (s, m) in [
            ("off", LoopMode::Off),
            ("none", LoopMode::Off),
            ("no", LoopMode::Off),
            ("0", LoopMode::Off),
            ("false", LoopMode::Off),
            ("track", LoopMode::Track),
            ("song", LoopMode::Track),
            ("one", LoopMode::Track),
            ("this", LoopMode::Track),
            ("1", LoopMode::Track),
            ("queue", LoopMode::Queue),
            ("all", LoopMode::Queue),
            ("list", LoopMode::Queue),
            ("on", LoopMode::Queue),
            ("true", LoopMode::Queue),
        ] {
            assert_eq!(LoopMode::parse(s), Some(m), "{s}");
        }
        assert_eq!(LoopMode::parse(" OFF "), Some(LoopMode::Off));
        assert_eq!(LoopMode::parse("nope"), None);
        assert_eq!(LoopMode::Off.next(), LoopMode::Track);
        assert_eq!(LoopMode::Track.next(), LoopMode::Queue);
        assert_eq!(LoopMode::Queue.next(), LoopMode::Off);
    }

    #[test]
    fn parse_time_works() {
        for (s, n) in [
            ("1:23", 83),
            ("01:02:03", 3723),
            ("90", 90),
            ("90s", 90),
            ("2m", 120),
            ("1m30s", 90),
            ("1h2m3s", 3723),
        ] {
            assert_eq!(parse_time(s), Some(n), "{s}");
        }
        for s in ["", "1:60", "abc", "1::2", "5x", "1:2:3:4", "m"] {
            assert_eq!(parse_time(s), None, "{s}");
        }
    }

    #[test]
    fn clock_works() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(83), "1:23");
        assert_eq!(clock(3723), "1:02:03");
    }
}
