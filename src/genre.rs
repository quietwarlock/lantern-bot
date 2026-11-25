//! Genre play (1.12): `genre <name>`.
//!
//! 1. The owner's library songs whose genre tag matches (forgiving about case,
//!    spacing, `&`/`and` and small typos), shuffled.
//! 2. Then a genre lock: when the queue runs out Lantern keeps playing that
//!    genre instead of drifting like autoplay can. Recently played songs are skipped.
//! 3. For variety the lock mixes in online songs: Last.fm's top tracks for the
//!    tag (needs a free API key: `LASTFM_API_KEY` or the Last.fm key setting),
//!    found on YouTube with the usual yt-dlp backup. Without a key, or when
//!    Last.fm fails, YouTube searches for the genre are used instead.
//! 4. When nothing at all turns up, Discord offers radio stations for the genre.
//!
//! The lock is per server, kept in memory, and ends with `genre off`, `stop`/`leave`.

use crate::bot::source_key;
use crate::media::{Item, Kind};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Duration;

/// Environment variable for the Last.fm API key. Wins over the setting.
pub const KEY_ENV: &str = "LASTFM_API_KEY";
const LASTFM_API: &str = "https://ws.audioscrobbler.com/2.0/";
/// Last.fm pages of top tracks read per lock (50 each).
pub const LASTFM_PAGES: u32 = 4;
const LASTFM_PER_PAGE: u32 = 50;
/// Songs read from one YouTube mix; half are kept so one artist doesn't take over.
const MIX_SONGS: usize = 24;
/// YouTube searches used without Last.fm. `{}` is the genre.
/// "official music video" finds single songs; plain "songs" searches mostly find hour-long mixes.
pub const YOUTUBE_QUERIES: [&str; 6] = ["{} official music video", "{} official video", "{} official audio", "{} lyric video", "best {} songs", "{} songs"];
/// Online songs longer than this are probably mixes or whole albums.
pub const MAX_ONLINE_SECS: u64 = 12 * 60;
/// Longest genre name accepted.
pub const MAX_NAME: usize = 60;

/// What makes two queue entries the same song: the video ID for YouTube
/// (every watch link looks alike without its `?v=`), else the link without
/// case, query or trailing slash.
pub fn song_key(source: &str) -> String {
    match crate::media::youtube_id(source) {
        Some(id) => format!("yt:{id}"),
        None => source_key(source),
    }
}

/// The Last.fm key: `LASTFM_API_KEY`, else the setting. `None` when neither is set.
pub fn lastfm_key(setting: &str) -> Option<String> {
    let from_env = std::env::var(KEY_ENV).ok().map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
    from_env.or_else(|| Some(setting.trim().to_string()).filter(|k| !k.is_empty()))
}

/// A Last.fm API key is 32 hex digits.
pub fn valid_key(key: &str) -> bool {
    key.len() == 32 && key.chars().all(|c| c.is_ascii_hexdigit())
}

/// ID3v1 genres are stored as numbers, like `(17)` or `17`. Those become names.
pub fn id3v1_name(text: &str) -> String {
    let text = text.trim();
    let number = match text.strip_prefix('(').and_then(|rest| rest.split_once(')')) {
        Some((_, rest)) if !rest.trim().is_empty() => return rest.trim().to_string(),
        Some((n, _)) => n,
        None => text,
    };
    match number.parse::<usize>().ok().and_then(|n| lofty::id3::v1::GENRES.get(n)) {
        Some(name) => (*name).to_string(),
        None => text.to_string(),
    }
}

/// Lower case letters and digits only, with a few spellings made the same.
pub fn normalize(text: &str) -> String {
    let joined: String = text.to_lowercase().replace('&', " and ").chars().filter(|c| c.is_alphanumeric()).collect();
    match joined.as_str() {
        "rb" | "randb" | "rnb" | "rhythmandblues" => "rnb".into(),
        "dnb" | "drumandbass" | "drumnbass" | "drumbass" => "dnb".into(),
        "rocknroll" | "rockandroll" | "rocknrol" => "rocknroll".into(),
        _ => joined,
    }
}

/// Other ways a normalized genre is written inside longer tags.
fn spellings(wanted: &str) -> Vec<&str> {
    match wanted {
        "rnb" => vec!["rnb", "randb", "rhythmandblues"],
        "dnb" => vec!["dnb", "drumandbass", "drumnbass"],
        "rocknroll" => vec!["rocknroll", "rockandroll"],
        other => vec![other],
    }
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(String::from).collect()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = if ca == cb { prev } else { 1 + prev.min(cur).min(row[j]) };
            prev = cur;
        }
    }
    row[b.len()]
}

/// A small typo: one letter off from 5 letters, two from 8.
fn close(have: &str, wanted: &str) -> bool {
    let allowed = match wanted.chars().count() {
        n if n >= 8 => 2,
        n if n >= 5 => 1,
        _ => 0,
    };
    allowed > 0 && levenshtein(have, wanted) <= allowed
}

/// True when a file's genre tag (which may list several, split by `;`, `,`,
/// `/` or `|`) matches the wanted genre. Short names (`rap`, `pop`) must be a
/// whole word, so `rap` doesn't match `trap`; longer ones may sit inside a
/// longer genre (`metal` matches `Nu Metal`) or be a letter or two off.
pub fn tag_matches(tag: &str, wanted: &str) -> bool {
    let want = normalize(wanted);
    if want.is_empty() {
        return false;
    }
    tag.split([';', ',', '/', '|', '\0']).any(|part| {
        let have = normalize(part);
        if have.is_empty() {
            return false;
        }
        if have == want {
            return true;
        }
        let spelled = spellings(&want);
        if want.chars().count() < 4 {
            let part_words = words(part);
            return spelled.iter().any(|s| s.chars().count() >= 4 && have.contains(s)) || part_words.iter().any(|w| w == &want);
        }
        spelled.iter().any(|s| have.contains(s)) || close(&have, &want)
    })
}

/// Library songs whose genre matches, in library order.
pub fn library_matches(index: &crate::library::Index, wanted: &str) -> Vec<Item> {
    index.tracks.iter().filter(|t| !t.genre.is_empty() && tag_matches(&t.genre, wanted)).map(|t| t.item()).collect()
}

/// The genre names in the library, most songs first (for "did you mean").
pub fn library_genres(index: &crate::library::Index, limit: usize) -> Vec<(String, usize)> {
    let mut counts: std::collections::HashMap<String, (String, usize)> = std::collections::HashMap::new();
    for track in &index.tracks {
        for part in track.genre.split([';', ',', '/', '|', '\0']) {
            let name = part.trim();
            let key = normalize(name);
            if key.is_empty() {
                continue;
            }
            counts.entry(key).or_insert_with(|| (name.to_string(), 0)).1 += 1;
        }
    }
    let mut list: Vec<(String, usize)> = counts.into_values().collect();
    list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
    list.truncate(limit);
    list
}

/// Cleans a typed genre name. Empty means none.
pub fn clean_name(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let joined = words.join(" ");
    joined.chars().filter(|c| !c.is_control()).take(MAX_NAME).collect::<String>().trim().to_string()
}

/// One server's genre lock.
#[derive(Clone, Debug, Default)]
pub struct GenreLock {
    pub name: String,
    /// Library songs not queued yet, shuffled.
    pub library: Vec<Item>,
    /// Online songs found but not played yet.
    pub online: Vec<Item>,
    /// Last.fm pages read so far. [`LASTFM_PAGES`] means stop asking.
    pub lastfm_pages: u32,
    /// YouTube searches used so far.
    pub youtube_queries: usize,
    /// Songs this lock already picked (source keys).
    pub played: HashSet<String>,
    /// Picks so far; even turns take a library song when there is one.
    pub turn: u64,
    /// Online lookups found nothing more.
    pub online_done: bool,
    /// Why Last.fm wasn't used, for the log (said once).
    pub lastfm_note: Option<String>,
}

impl GenreLock {
    pub fn new(name: &str, mut library: Vec<Item>) -> Self {
        fastrand::shuffle(&mut library);
        Self { name: name.to_string(), library, ..Default::default() }
    }

    /// Takes up to `count` library songs to queue now.
    pub fn take_library(&mut self, count: usize) -> Vec<Item> {
        let n = count.min(self.library.len());
        let taken: Vec<Item> = self.library.drain(..n).collect();
        self.played.extend(taken.iter().map(|i| song_key(&i.source)));
        taken
    }

    /// The next song from the pools, skipping recent ones. `None` with
    /// `needs_online` true means the online pool is empty and more can be fetched.
    pub fn next(&mut self, recent: &HashSet<String>) -> (Option<Item>, bool) {
        let library_turn = self.turn.is_multiple_of(2);
        let order: [bool; 2] = if library_turn { [true, false] } else { [false, true] };
        for from_library in order {
            let pool = if from_library { &mut self.library } else { &mut self.online };
            while !pool.is_empty() {
                let item = pool.remove(0);
                let key = song_key(&item.source);
                if recent.contains(&key) || self.played.contains(&key) {
                    continue;
                }
                self.played.insert(key);
                self.turn += 1;
                return (Some(item), false);
            }
            if !from_library && !self.online_done {
                // Online's turn (or the library is out): fetch more first.
                return (None, true);
            }
        }
        (None, false)
    }
}

/// Where the next online batch comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetch {
    LastFm { page: u32 },
    YouTube { query: String },
    Done,
}

/// Picks the next online lookup for a lock.
pub fn next_fetch(lock: &GenreLock, have_key: bool) -> Fetch {
    if have_key && lock.lastfm_pages < LASTFM_PAGES {
        return Fetch::LastFm { page: lock.lastfm_pages + 1 };
    }
    match YOUTUBE_QUERIES.get(lock.youtube_queries) {
        Some(pattern) => Fetch::YouTube { query: pattern.replace("{}", &lock.name) },
        None => Fetch::Done,
    }
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("Lantern/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(6))
            .timeout(Duration::from_secs(12))
            .build()
            .unwrap_or_default()
    })
}

/// Last.fm's answer to `tag.gettoptracks`: queue entries found on YouTube when played.
pub fn parse_lastfm(json: &str) -> Result<Vec<Item>, String> {
    let value: Value = serde_json::from_str(json).map_err(|e| format!("Last.fm sent something unexpected ({e})"))?;
    if let Some(code) = value.get("error") {
        let message = value.get("message").and_then(Value::as_str).unwrap_or("error");
        return Err(format!("Last.fm said {message} ({code})"));
    }
    let tracks = value.pointer("/tracks/track").and_then(Value::as_array).ok_or("Last.fm sent no track list")?;
    let mut seen = HashSet::new();
    Ok(tracks
        .iter()
        .filter_map(|t| {
            let title = t.get("name").and_then(Value::as_str)?.trim();
            let artist = t.pointer("/artist/name").and_then(Value::as_str).unwrap_or("").trim();
            if title.is_empty() {
                return None;
            }
            let shown = if artist.is_empty() { title.to_string() } else { format!("{artist} - {title}") };
            if !seen.insert(shown.to_lowercase()) {
                return None;
            }
            let url = t.get("url").and_then(Value::as_str).filter(|u| u.starts_with("https://www.last.fm/")).map(String::from);
            let source = url.unwrap_or_else(|| format!("https://www.last.fm/music/{}/_/{}", urlencoding::encode(artist), urlencoding::encode(title)));
            let length = match t.get("duration") {
                Some(Value::String(s)) => s.parse::<u64>().ok(),
                Some(Value::Number(n)) => n.as_u64(),
                _ => None,
            }
            .filter(|s| *s > 0);
            Some(Item { title: shown.clone(), source, query: Some(shown), kind: Kind::Search, length, allow_private_host: false, requested_by: None })
        })
        .collect())
}

/// One page of Last.fm's top tracks for a tag. The key never appears in errors.
pub async fn lastfm_top_tracks(key: &str, tag: &str, page: u32) -> Result<Vec<Item>, String> {
    let query = [
        ("method", "tag.gettoptracks".to_string()),
        ("tag", tag.to_string()),
        ("api_key", key.to_string()),
        ("format", "json".to_string()),
        ("limit", LASTFM_PER_PAGE.to_string()),
        ("page", page.to_string()),
    ];
    let response = client().get(LASTFM_API).query(&query).send().await.map_err(|e| format!("Last.fm didn't answer ({})", e.without_url()))?;
    let status = response.status();
    let text = response.text().await.map_err(|e| format!("Last.fm didn't answer ({})", e.without_url()))?;
    match parse_lastfm(&text) {
        Ok(items) => Ok(items),
        Err(err) if status.is_success() => Err(err),
        Err(err) => Err(format!("{err} [HTTP {}]", status.as_u16())),
    }
}

/// A single song, not a long mix or a live stream.
pub fn single_song(item: &Item) -> bool {
    item.length.is_some_and(|s| (60..=MAX_ONLINE_SECS).contains(&s))
}

/// YouTube results for a genre search: single songs only (no long mixes), plus
/// YouTube's mix for one of them, which stays close to that song's style.
pub async fn youtube_batch(query: &str) -> Result<Vec<Item>, String> {
    let hits = crate::cache::youtube_hits(query, 8).await?;
    let mut songs: Vec<Item> = hits.into_iter().map(|h| h.item).filter(single_song).collect();
    if songs.is_empty() {
        return Ok(songs);
    }
    let seed = songs.get(fastrand::usize(..songs.len())).and_then(|i| crate::media::youtube_id(&i.source));
    if let Some(seed) = seed {
        if let Ok(mix) = crate::media::youtube_list(&format!("RD{seed}"), Some(&seed), MIX_SONGS).await {
            let have: HashSet<String> = songs.iter().map(|i| song_key(&i.source)).collect();
            songs.extend(mix.into_iter().filter(|i| i.length.is_none_or(|s| (60..=MAX_ONLINE_SECS).contains(&s))).filter(|i| !have.contains(&song_key(&i.source))).take(MIX_SONGS / 2));
        }
    }
    Ok(songs)
}

/// Runs one lookup. Returns the songs (shuffled) and a note when Last.fm failed.
pub async fn fetch(step: &Fetch, key: Option<&str>, genre: &str) -> (Vec<Item>, Option<String>) {
    let mut items = match step {
        Fetch::LastFm { page } => match key {
            Some(key) => match lastfm_top_tracks(key, genre, *page).await {
                Ok(items) => items,
                Err(err) => return (Vec::new(), Some(err)),
            },
            None => Vec::new(),
        },
        Fetch::YouTube { query } => match youtube_batch(query).await {
            Ok(items) => items,
            Err(err) => return (Vec::new(), Some(format!("YouTube search failed: {err}"))),
        },
        Fetch::Done => Vec::new(),
    };
    fastrand::shuffle(&mut items);
    (items, None)
}

/// Applies a lookup's result to the lock. `failed` is the note from [`fetch`].
pub fn absorb(lock: &mut GenreLock, step: &Fetch, items: Vec<Item>, failed: Option<String>) {
    match step {
        Fetch::LastFm { .. } => {
            if failed.is_some() || items.is_empty() {
                // A bad key or an unknown tag: YouTube from here on.
                lock.lastfm_pages = LASTFM_PAGES;
                if lock.lastfm_note.is_none() {
                    lock.lastfm_note = Some(failed.unwrap_or_else(|| "Last.fm has no songs for that tag".into()));
                }
            } else {
                lock.lastfm_pages += 1;
            }
        }
        Fetch::YouTube { .. } => lock.youtube_queries += 1,
        Fetch::Done => lock.online_done = true,
    }
    lock.online.extend(items);
}

/// Songs for the web player's queue: shuffled library matches, topped up
/// with online songs to `want` when the library has fewer.
pub async fn web_batch(genre: &str, key: Option<&str>, want: usize) -> (Vec<Item>, usize) {
    let library = crate::library::with(|index| library_matches(index, genre));
    let mut lock = GenreLock::new(genre, library);
    let mut items = lock.take_library(want);
    let from_library = items.len();
    let mut tries = 0;
    while items.len() < want && tries < YOUTUBE_QUERIES.len() {
        tries += 1;
        let step = next_fetch(&lock, key.is_some());
        if step == Fetch::Done {
            break;
        }
        let (found, failed) = fetch(&step, key, genre).await;
        absorb(&mut lock, &step, found, failed);
        let none = HashSet::new();
        while items.len() < want {
            lock.turn = 1; // online pool only
            match lock.next(&none) {
                (Some(item), _) if item.kind != Kind::File => items.push(item),
                (Some(_), _) => {}
                (None, _) => break,
            }
        }
    }
    (items, from_library)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genre_tags_match_forgivingly() {
        assert!(tag_matches("Rock", "rock"));
        assert!(tag_matches("Hip-Hop", "hip hop"));
        assert!(tag_matches("Hip Hop/Rap", "rap"));
        assert!(!tag_matches("Trap", "rap"), "short names must be a whole word");
        assert!(tag_matches("K-Pop", "pop"));
        assert!(tag_matches("Nu Metal", "metal"));
        assert!(tag_matches("Nu-Metal", "nu metal"));
        assert!(tag_matches("R&B", "rnb"));
        assert!(tag_matches("Contemporary R&B", "r&b"));
        assert!(tag_matches("Rhythm and Blues", "R&B"));
        assert!(tag_matches("Drum & Bass", "dnb"));
        assert!(tag_matches("Electronica", "electronic"), "one letter off");
        assert!(tag_matches("Alternative Rock; Grunge", "grunge"));
        assert!(tag_matches("Progresive Rock", "progressive rock"), "a typo in the tag");
        assert!(!tag_matches("Jazz", "rock"));
        assert!(!tag_matches("", "rock"));
        assert!(!tag_matches("Rock", "  "));
    }

    #[test]
    fn id3v1_numbers_become_names() {
        assert_eq!(id3v1_name("(17)"), "Rock");
        assert_eq!(id3v1_name("9"), "Metal");
        assert_eq!(id3v1_name("(17)Rock & Roll"), "Rock & Roll");
        assert_eq!(id3v1_name("Nu Metal"), "Nu Metal");
        assert_eq!(id3v1_name("(999)"), "(999)");
    }

    fn file(title: &str, genre: &str) -> crate::library::Track {
        crate::library::Track {
            path: format!("/music/{title}.mp3"),
            folder: "/music".into(),
            title: title.into(),
            artist: "A".into(),
            album: String::new(),
            length: Some(200),
            genre: genre.into(),
        }
    }

    #[test]
    fn library_songs_are_picked_by_genre() {
        let index = crate::library::Index {
            tracks: vec![file("one", "Nu Metal"), file("two", "Jazz"), file("three", "Metal; Rock"), file("four", "")],
            ..Default::default()
        };
        let found: Vec<String> = library_matches(&index, "metal").into_iter().map(|i| i.title).collect();
        assert_eq!(found, ["A - one", "A - three"]);
        assert!(library_matches(&index, "polka").is_empty());
        let genres = library_genres(&index, 10);
        assert_eq!(genres.len(), 4);
        assert!(genres.iter().all(|(_, n)| *n == 1));
    }

    fn song(source: &str) -> Item {
        Item::new(source, source, Kind::YouTube)
    }

    #[test]
    fn the_lock_mixes_library_and_online_and_skips_repeats() {
        let mut lock = GenreLock::new("rock", vec![Item::new("L1", "/m/l1.mp3", Kind::File), Item::new("L2", "/m/l2.mp3", Kind::File)]);
        lock.library.sort_by(|a, b| a.title.cmp(&b.title));
        let recent: HashSet<String> = [song_key("https://y/played")].into_iter().collect();
        let (first, _) = lock.next(&recent);
        assert_eq!(first.unwrap().title, "L1", "library first");
        let (none, fetch_more) = lock.next(&recent);
        assert!(none.is_none() && fetch_more, "online's turn with an empty pool: fetch");
        absorb(&mut lock, &Fetch::YouTube { query: "rock songs".into() }, vec![song("https://y/played"), song("https://y/new")], None);
        let (second, _) = lock.next(&recent);
        assert_eq!(second.unwrap().source, "https://y/new", "recently played skipped");
        let (third, _) = lock.next(&recent);
        assert_eq!(third.unwrap().title, "L2");
        absorb(&mut lock, &Fetch::YouTube { query: "x".into() }, vec![song("https://y/new")], None);
        let (again, more) = lock.next(&recent);
        assert!(again.is_none() && more, "a song this lock already played isn't picked again");
        lock.online_done = true;
        assert_eq!(lock.next(&recent), (None, false));
    }

    #[test]
    fn lookups_go_lastfm_then_youtube_then_stop() {
        let mut lock = GenreLock::new("jazz", Vec::new());
        assert_eq!(next_fetch(&lock, false), Fetch::YouTube { query: "jazz official music video".into() }, "no key: YouTube");
        assert_eq!(next_fetch(&lock, true), Fetch::LastFm { page: 1 });
        absorb(&mut lock, &Fetch::LastFm { page: 1 }, Vec::new(), Some("Last.fm said Invalid API key (10)".into()));
        assert_eq!(next_fetch(&lock, true), Fetch::YouTube { query: "jazz official music video".into() }, "a failed key falls back to YouTube");
        assert!(lock.lastfm_note.as_deref().unwrap().contains("Invalid API key"));
        lock.youtube_queries = YOUTUBE_QUERIES.len();
        assert_eq!(next_fetch(&lock, true), Fetch::Done);
        absorb(&mut lock, &Fetch::Done, Vec::new(), None);
        assert!(lock.online_done);
    }

    #[test]
    fn lastfm_top_tracks_are_parsed() {
        let json = r#"{"tracks":{"track":[
            {"name":"Chop Suey!","duration":"210","url":"https://www.last.fm/music/System+of+a+Down/_/Chop+Suey%21","artist":{"name":"System of a Down"}},
            {"name":"chop suey!","duration":"0","artist":{"name":"system of a down"}},
            {"name":"Freak on a Leash","duration":"0","url":"http://evil.example/x","artist":{"name":"Korn"}},
            {"name":"","artist":{"name":"Nobody"}}
        ],"@attr":{"tag":"nu metal","page":"1"}}}"#;
        let items = parse_lastfm(json).unwrap();
        assert_eq!(items.len(), 2, "duplicates and nameless entries dropped");
        assert_eq!(items[0].title, "System of a Down - Chop Suey!");
        assert_eq!(items[0].kind, Kind::Search);
        assert_eq!(items[0].query.as_deref(), Some("System of a Down - Chop Suey!"));
        assert_eq!(items[0].length, Some(210));
        assert_eq!(items[1].length, None);
        assert!(items[1].source.starts_with("https://www.last.fm/music/Korn/"), "only last.fm links are kept");
        let bad = parse_lastfm(r#"{"error":10,"message":"Invalid API key - You must be granted a valid key by last.fm"}"#).unwrap_err();
        assert!(bad.contains("Invalid API key"));
        assert!(parse_lastfm("nope").is_err());
    }

    /// Live network: YouTube genre search, Last.fm refusing a made-up key
    /// (and the fallback to YouTube), and a web batch with no library.
    #[tokio::test]
    #[ignore]
    async fn live_genre_lookups() {
        let hits = crate::cache::youtube_hits("nu metal official music video", 8).await.expect("hits");
        println!("hits: {:?}", hits.iter().map(|h| (&h.item.title, h.item.length)).collect::<Vec<_>>());
        let songs = youtube_batch("nu metal official music video").await.expect("YouTube search");
        println!("batch1 {}: {:?}", songs.len(), songs.iter().map(|i| (&i.title, i.length)).collect::<Vec<_>>());
        let again = youtube_batch("nu metal official music video").await.expect("YouTube search");
        println!("batch2 {}", again.len());
        assert!(songs.len() >= 5, "YouTube found nu metal songs: {}", songs.len());
        assert!(songs.iter().all(|i| i.length.is_none_or(|s| s <= MAX_ONLINE_SECS)));
        let fake = "0123456789abcdef0123456789abcdef";
        let err = lastfm_top_tracks(fake, "nu metal", 1).await.unwrap_err();
        assert!(!err.contains(fake), "the key never shows in an error: {err}");
        let mut lock = GenreLock::new("nu metal", Vec::new());
        let step = next_fetch(&lock, true);
        let (items, failed) = fetch(&step, Some(fake), "nu metal").await;
        absorb(&mut lock, &step, items, failed);
        assert_eq!(next_fetch(&lock, true), Fetch::YouTube { query: "nu metal official music video".into() }, "a refused key falls back to YouTube");
        let (batch, from_library) = web_batch("nu metal", Some(fake), 10).await;
        assert_eq!(from_library, 0);
        assert!(batch.len() >= 5, "topped up online: {}", batch.len());
        let jazz = web_batch("jazz", None, 10).await.0;
        println!("youtube {} songs, first {:?}; lastfm said: {err}; web batch {}: {:?}", songs.len(), songs.first().map(|i| &i.title), batch.len(), batch.iter().map(|i| &i.title).collect::<Vec<_>>());
        println!("jazz batch {}: {:?}", jazz.len(), jazz.iter().map(|i| &i.title).collect::<Vec<_>>());
        assert!(jazz.len() >= 5, "jazz needs songs too");
    }

    #[test]
    fn youtube_songs_are_told_apart_by_video_id() {
        let a = song_key("https://www.youtube.com/watch?v=aaaaaaaaaaa");
        let b = song_key("https://www.youtube.com/watch?v=bbbbbbbbbbb");
        assert_ne!(a, b, "every watch link is the same without ?v=");
        assert_eq!(a, song_key("https://youtu.be/aaaaaaaaaaa"));
        assert_eq!(song_key("https://SoundCloud.com/a/b?x=1"), song_key("https://soundcloud.com/a/b/"));
        let mut lock = GenreLock::new("x", Vec::new());
        lock.online = vec![Item::new("A", "https://www.youtube.com/watch?v=aaaaaaaaaaa", Kind::YouTube), Item::new("B", "https://www.youtube.com/watch?v=bbbbbbbbbbb", Kind::YouTube)];
        lock.turn = 1;
        assert!(lock.next(&HashSet::new()).0.is_some());
        lock.turn = 1;
        assert_eq!(lock.next(&HashSet::new()).0.map(|i| i.title), Some("B".to_string()));
    }

    #[test]
    fn keys_and_names_are_checked() {
        assert!(valid_key("0123456789abcdef0123456789ABCDEF"));
        assert!(!valid_key("short"));
        assert_eq!(clean_name("  nu   metal \n"), "nu metal");
        assert_eq!(clean_name(&"x".repeat(100)).len(), MAX_NAME);
    }
}
