//! Lantern 1.6 additions that work on a running [`Core`].
//!
//! Playlist import/export: the window reads and writes `.m3u`, `.m3u8`,
//! `.pls` and `.xspf` files with real paths. Discord can import a playlist
//! file too, but only links and songs already in the owner's library play
//! from it, and Discord exports never contain paths.

use crate::bot::Core;
use crate::media::{self, Item, Kind};
use crate::playlist_file::{self, Format, Pending};
use serenity::all::GuildId;
use std::path::Path;
use std::sync::Arc;

/// What an import found, for the message afterwards.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub queued: usize,
    pub missing: usize,
    pub failed_links: usize,
}

impl ImportReport {
    /// `"Queued 12 tracks from Road Trip.m3u. 2 files weren't found."`
    pub fn sentence(&self, name: &str) -> String {
        let mut text = format!("Queued {} track{} from {name}.", self.queued, if self.queued == 1 { "" } else { "s" });
        if self.missing > 0 {
            text.push_str(&format!(
                " Skipped {} file{} that {} found.",
                self.missing,
                if self.missing == 1 { "" } else { "s" },
                if self.missing == 1 { "wasn't" } else { "weren't" }
            ));
        }
        if self.failed_links > 0 {
            text.push_str(&format!(
                " {} link{} wouldn't load.",
                self.failed_links,
                if self.failed_links == 1 { "" } else { "s" }
            ));
        }
        text
    }
}

/// Reads playlist text into items. `path_ok` decides what a path line may
/// play. Links are looked up like `play <link>`, one track each.
pub async fn items_from_text(
    text: &str,
    format: Format,
    base: Option<&Path>,
    path_ok: impl Fn(&Path) -> Option<Item>,
    allow_private: bool,
) -> (Vec<Item>, ImportReport) {
    let entries = playlist_file::parse(text, format, base);
    let (pending, missing) = playlist_file::split_entries(entries, path_ok);
    let mut report = ImportReport { missing, ..Default::default() };
    let mut items = Vec::new();
    for entry in pending {
        match entry {
            Pending::Ready(item) => items.push(item),
            Pending::Link(url, title) => match media::resolve(&url, 1, allow_private).await {
                Ok(mut found) if !found.is_empty() => {
                    let mut item = found.remove(0);
                    // A bare stream link has no better name than the playlist's.
                    if let Some(title) = title {
                        if matches!(item.kind, Kind::Direct | Kind::Radio) {
                            item.title = title;
                        }
                    }
                    items.push(item);
                }
                _ => report.failed_links += 1,
            },
        }
    }
    report.queued = items.len();
    (items, report)
}

/// A path line from a playlist sent in Discord: it plays only if that file
/// is in the owner's music library.
pub fn library_file(path: &Path) -> Option<Item> {
    let text = path.to_string_lossy();
    crate::library::with(|index| index.by_path(&text).map(|t| t.item()))
}

/// Reads a playlist file picked in the window, with a size limit.
pub fn read_playlist_file(path: &Path) -> Result<(String, Format), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("Couldn't open that playlist: {e}"))?;
    if meta.len() > playlist_file::MAX_FILE_BYTES {
        return Err("That playlist file is too big (over 4 MB).".into());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read that playlist: {e}"))?;
    let text = decode(&bytes);
    let format = Format::from_path(path).unwrap_or_else(|| playlist_file::sniff(&text));
    Ok((text, format))
}

/// UTF-8, or Windows-1252 as old Winamp lists use (Latin-1 is close enough).
pub fn decode(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => bytes.iter().map(|b| *b as char).collect(),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "the playlist".into())
}

impl Core {
    /// The song that's playing plus everything queued after it.
    pub(crate) async fn queue_items(&self, guild: GuildId) -> Vec<Item> {
        let guilds = self.guilds.lock().await;
        match guilds.get(&guild.get()) {
            Some(state) => state.now.iter().map(|n| n.item.clone()).chain(state.queue.upcoming.iter().cloned()).collect(),
            None => Vec::new(),
        }
    }

    /// Window: queue a playlist file. Paths play from disk or a share.
    pub(crate) async fn import_file(self: &Arc<Self>, guild: GuildId, path: &Path) -> Result<String, String> {
        let (text, format) = read_playlist_file(path)?;
        let (items, report) = items_from_text(&text, format, path.parent(), playlist_file::local_file, true).await;
        let name = file_name(path);
        if items.is_empty() {
            return Err(format!("Nothing in {name} could be played. {}", report.sentence(&name)));
        }
        self.ensure_voice_pub(guild).await?;
        self.push(guild, items, None).await;
        let note = report.sentence(&name);
        if crate::settings::routine_channel_post(self.ui.settings().quiet) {
            let channel = self.text_channel(guild).await;
            self.say(channel, &crate::bot::escape(&note)).await;
        }
        self.ui.log(format!("Imported {name}: {} queued, {} missing, {} links failed", report.queued, report.missing, report.failed_links));
        Ok(note)
    }

    /// Window: save what's playing and the queue to a playlist file.
    pub(crate) async fn export_file(&self, guild: GuildId, path: &Path) -> Result<String, String> {
        let items = self.queue_items(guild).await;
        if items.is_empty() {
            return Err("The queue is empty, so there's nothing to export.".into());
        }
        let format = Format::from_path(path).unwrap_or(Format::M3u8);
        let (text, left_out) = playlist_file::write(&items, format, true);
        crate::settings::write_atomic(path, &text).map_err(|e| format!("Couldn't save the playlist: {e}"))?;
        let mut note = format!("Exported {} track{} to {}.", items.len() - left_out, if items.len() - left_out == 1 { "" } else { "s" }, file_name(path));
        if left_out > 0 {
            note.push_str(&format!(" {left_out} without a link or file were left out."));
        }
        Ok(note)
    }
}

/// What `sleep` was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum SleepRequest {
    Show,
    Off,
    EndOfSong,
    After(std::time::Duration),
}

/// Longest sleep timer.
pub const SLEEP_MAX_MINUTES: u64 = 12 * 60;

/// `30m`, `45` (minutes), `1h`, `1h30m`, `1:30` (h:mm), `end`, `off`.
pub fn parse_sleep(arg: &str) -> Option<SleepRequest> {
    let text = arg.trim().to_ascii_lowercase().replace(' ', "");
    match text.as_str() {
        "" => return Some(SleepRequest::Show),
        "off" | "cancel" | "stop" | "no" => return Some(SleepRequest::Off),
        "end" | "song" | "endofsong" | "aftersong" | "afterthis" => return Some(SleepRequest::EndOfSong),
        _ => {}
    }
    let minutes = if let Some((h, m)) = text.split_once(':') {
        h.parse::<u64>().ok()?.checked_mul(60)?.checked_add(m.parse::<u64>().ok().filter(|m| *m < 60)?)?
    } else if text.chars().all(|c| c.is_ascii_digit()) {
        text.parse::<u64>().ok()?
    } else {
        let mut total = 0u64;
        let mut number = String::new();
        let mut any = false;
        let text = text.replace("hours", "h").replace("hour", "h").replace("hrs", "h").replace("hr", "h")
            .replace("minutes", "m").replace("minute", "m").replace("mins", "m").replace("min", "m");
        for c in text.chars() {
            if c.is_ascii_digit() {
                number.push(c);
            } else {
                let n: u64 = number.parse().ok()?;
                number.clear();
                let add = match c {
                    'h' => n.checked_mul(60)?,
                    'm' => n,
                    _ => return None,
                };
                total = total.checked_add(add)?;
                any = true;
            }
        }
        if !number.is_empty() || !any {
            return None;
        }
        total
    };
    (1..=SLEEP_MAX_MINUTES).contains(&minutes).then(|| SleepRequest::After(std::time::Duration::from_secs(minutes * 60)))
}

/// `1 h 5 min`, `25 min`.
pub fn minutes_text(minutes: u64) -> String {
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

impl Core {
    /// The `sleep` command.
    pub(crate) async fn sleep_timer(self: &Arc<Self>, guild: GuildId, request: SleepRequest, channel: Option<serenity::all::ChannelId>) -> Result<String, String> {
        let mut guilds = self.guilds.lock().await;
        let state = guilds.entry(guild.get()).or_default();
        match request {
            SleepRequest::Show => Ok(match (state.sleep_at, state.sleep_after_song) {
                (_, true) => "I'll stop and leave when this song ends. `sleep off` cancels.".into(),
                (Some(at), _) => {
                    let left = at.saturating_duration_since(std::time::Instant::now()).as_secs().div_ceil(60).max(1);
                    format!("I'll stop and leave in about {}. `sleep off` cancels.", minutes_text(left))
                }
                _ => "No sleep timer. Try `sleep 30m`, `sleep 1h`, or `sleep end` for after this song.".into(),
            }),
            SleepRequest::Off => {
                let had = state.sleep_at.is_some() || state.sleep_after_song;
                state.sleep_token += 1;
                state.sleep_at = None;
                state.sleep_after_song = false;
                Ok(if had { "Sleep timer off." } else { "There was no sleep timer." }.into())
            }
            SleepRequest::EndOfSong => {
                if state.now.is_none() {
                    return Err("Nothing is playing.".into());
                }
                state.sleep_token += 1;
                state.sleep_at = None;
                state.sleep_after_song = true;
                Ok("I'll stop and leave when this song ends.".into())
            }
            SleepRequest::After(wait) => {
                if self.songbird.get(guild).is_none() {
                    return Err("I'm not in a voice channel.".into());
                }
                state.sleep_token += 1;
                state.sleep_after_song = false;
                state.sleep_at = Some(std::time::Instant::now() + wait);
                let token = state.sleep_token;
                let channel = channel.or(state.text_channel);
                let core = self.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(wait).await;
                    let current = core.guilds.lock().await.get(&guild.get()).is_some_and(|s| s.sleep_token == token);
                    if current {
                        core.sleep_now(guild, channel).await;
                    }
                });
                Ok(format!("Sleep timer set: I'll stop and leave in {}.", minutes_text(wait.as_secs() / 60)))
            }
        }
    }

    /// Hearts the song that's playing for this person.
    pub(crate) async fn favorite_now(&self, guild: GuildId, user: u64) -> Result<String, String> {
        let item = {
            let guilds = self.guilds.lock().await;
            let now = guilds.get(&guild.get()).and_then(|s| s.now.as_ref()).ok_or("Nothing is playing.")?;
            let mut item = now.item.clone();
            item.title = now.title.clone();
            item
        };
        self.store.add_favorite(guild.get(), user, &item)
    }

    pub(crate) async fn sleep_now(&self, guild: GuildId, channel: Option<serenity::all::ChannelId>) {
        if self.songbird.get(guild).is_none() {
            return;
        }
        self.leave(guild).await;
        self.ui.log("Sleep timer: stopped and left");
        self.say(channel, "Sleep timer: goodnight. Stopped and left the voice channel.").await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_times() {
        use std::time::Duration;
        let mins = |m: u64| Some(SleepRequest::After(Duration::from_secs(m * 60)));
        assert_eq!(parse_sleep("30m"), mins(30));
        // Huge numbers are refused instead of overflowing.
        assert_eq!(parse_sleep("999999999999999999h"), None);
        assert_eq!(parse_sleep("999999999999999999:00"), None);
        assert_eq!(parse_sleep("45"), mins(45));
        assert_eq!(parse_sleep("1h"), mins(60));
        assert_eq!(parse_sleep("1h 30m"), mins(90));
        assert_eq!(parse_sleep("2 hours"), mins(120));
        assert_eq!(parse_sleep("1:15"), mins(75));
        assert_eq!(parse_sleep("end"), Some(SleepRequest::EndOfSong));
        assert_eq!(parse_sleep("off"), Some(SleepRequest::Off));
        assert_eq!(parse_sleep(""), Some(SleepRequest::Show));
        assert_eq!(parse_sleep("0"), None);
        assert_eq!(parse_sleep("13h"), None);
        assert_eq!(parse_sleep("soon"), None);
        assert_eq!(parse_sleep("1:75"), None);
        assert_eq!(minutes_text(65), "1 h 5 min");
        assert_eq!(minutes_text(25), "25 min");
    }

    #[test]
    fn report_sentences() {
        let r = ImportReport { queued: 3, missing: 2, failed_links: 1 };
        assert_eq!(r.sentence("a.m3u"), "Queued 3 tracks from a.m3u. Skipped 2 files that weren't found. 1 link wouldn't load.");
        let r = ImportReport { queued: 1, missing: 1, failed_links: 0 };
        assert_eq!(r.sentence("b.pls"), "Queued 1 track from b.pls. Skipped 1 file that wasn't found.");
    }

    #[test]
    fn latin1_playlists_still_read() {
        assert_eq!(decode(b"caf\xe9"), "café");
        assert_eq!(decode("café".as_bytes()), "café");
    }

    #[tokio::test]
    async fn discord_imports_never_play_paths_outside_the_library() {
        let text = "#EXTM3U\nC:\\Windows\\win.ini\n/etc/passwd\n";
        let (items, report) = items_from_text(text, Format::M3u, None, library_file, false).await;
        assert!(items.is_empty());
        assert_eq!(report.missing, 2);
    }
}
