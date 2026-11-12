//! Finds audio and turns it into something Songbird can play.
//!
//! [`resolve`] turns what someone typed into queue items. It is cheap: links
//! that expire (YouTube, SoundCloud) are only looked up when the track is
//! about to play, in [`materialize`]. Every web request has a timeout, so a
//! host that never answers can't stall the queue.
//!
//! YouTube is tried through each Innertube client in turn, then through
//! yt-dlp ([`crate::ytdlp`]) if that is switched on. A client whose link
//! YouTube refuses counts as failed, so the next route is used.

use crate::stream::RangeSource;
use regex::Regex;
use crate::ytdlp;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, LOCATION, ORIGIN, RANGE, REFERER, USER_AGENT};
use rusty_ytdl::search::{SearchOptions, SearchResult, SearchType, YouTube};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Longest wait for one lookup request.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Longest wait for a server to accept a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// An audio stream that sends nothing for this long is reconnected.
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// A Spotify or Apple Music track matches a YouTube result this close in length.
pub const MATCH_TOLERANCE: u64 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// `source` is a YouTube watch link.
    YouTube,
    /// `source` is a SoundCloud track page. The stream link is fetched at play time.
    SoundCloud,
    /// `source` is an audio file URL.
    Direct,
    /// `source` is a path on this PC. Discord is only ever shown `title`.
    File,
    /// `source` is a live stream. It can't seek.
    Radio,
    /// Spotify or Apple Music: `query` is searched on YouTube at play time.
    Search,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub title: String,
    pub source: String,
    #[serde(default)]
    pub query: Option<String>,
    pub kind: Kind,
    /// Seconds, when known.
    #[serde(default)]
    pub length: Option<u64>,
    /// Set when the owner queued a link to a private or local address.
    #[serde(default)]
    pub allow_private_host: bool,
    /// Discord user who queued it (for the per-user queue limit). Not part of
    /// saved playlists or favorites.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<u64>,
}

impl Item {
    pub(crate) fn new(title: impl Into<String>, source: impl Into<String>, kind: Kind) -> Self {
        Self { title: title.into(), source: source.into(), query: None, kind, length: None, allow_private_host: false, requested_by: None }
    }

    /// A link people can click in Discord.
    pub fn link(&self) -> Option<&str> {
        (self.source.starts_with("https://") || self.source.starts_with("http://")).then_some(self.source.as_str())
    }
}

/// A stream ready for Songbird.
#[derive(Clone)]
pub struct Playable {
    /// Volume leveling: multiply the server's volume by this (1.0 = as is).
    pub gain: f32,
    pub url: String,
    /// Set for a file on this PC. `url` is unused then.
    pub path: Option<String>,
    /// Audio already downloaded in full (SoundCloud HLS), with its file
    /// extension for the decoder. `url` is unused then.
    pub memory: Option<(std::sync::Arc<Vec<u8>>, &'static str)>,
    pub title: String,
    pub headers: HeaderMap,
    pub length: Option<u64>,
    /// Set for YouTube, so autoplay can find related songs.
    pub youtube_id: Option<String>,
    /// File size for YouTube, which is fetched in `&range=` pieces.
    pub size: Option<u64>,
    /// For YouTube, the way the stream was found: a client name or `yt-dlp`.
    pub via: Option<&'static str>,
    /// For YouTube, why the routes tried before `via` were passed over.
    pub notes: Vec<String>,
    /// Internet radio may reconnect after a drop; other streams may not.
    pub live_stream: bool,
    /// `url` is a live HLS playlist (1.10.1), read by [`crate::hls::HlsSource`].
    pub hls: bool,
    /// `url` is plain ADTS AAC (1.10.1): its frame headers are rewritten for the decoder.
    pub adts: bool,
    /// The owner's LAN station: private addresses may be fetched.
    pub private_ok: bool,
}

impl Playable {
    pub fn input(&self) -> songbird::input::Input {
        if let Some(path) = &self.path {
            return songbird::input::File::new(path.clone()).into();
        }
        if let Some((bytes, _)) = &self.memory {
            return crate::stream::MemorySource::new(bytes.clone()).into();
        }
        if self.hls {
            return crate::hls::HlsSource::new(self.url.clone(), self.private_ok).into();
        }
        let source = RangeSource::new(stream_client().clone(), self.url.clone(), self.headers.clone())
            .with_query_range(self.size)
            .with_live_stream(self.live_stream);
        if self.adts {
            return crate::hls::AdtsSource::new(source).into();
        }
        source.into()
    }

    /// For the activity log: ` via Android VR`, plus what failed before it.
    pub fn via_note(&self) -> String {
        match (self.via, self.notes.is_empty()) {
            (None, _) => String::new(),
            (Some(via), true) => format!(" via {via}"),
            (Some(via), false) => format!(" via {via} ({})", self.notes.join("; ")),
        }
    }
}

/// Shared client for lookups. Every request times out.
fn web() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(WEB_UA)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_default()
    })
}

/// User-supplied URLs: redirects are followed manually so each hop is checked.
fn user_web() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(WEB_UA)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default()
    })
}

/// Shared client for audio. Songs are long, so there is no overall limit,
/// but a stalled connection is dropped and reconnected.
fn stream_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(STREAM_READ_TIMEOUT)
            .build()
            .unwrap_or_default()
    })
}

async fn timed<T>(what: &str, future: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    match tokio::time::timeout(REQUEST_TIMEOUT, future).await {
        Ok(result) => result,
        Err(_) => Err(format!("{what} took too long to answer")),
    }
}

pub async fn resolve(query: &str, limit: usize, allow_private_hosts: bool) -> Result<Vec<Item>, String> {
    let cleaned = query.trim().trim_start_matches('<').trim_end_matches('>');
    let lower = cleaned.to_ascii_lowercase();
    if spotify_id(cleaned).is_some() {
        return spotify(cleaned, limit).await;
    }
    if lower.contains("music.apple.com") || lower.contains("itunes.apple.com") {
        return apple(cleaned, limit).await;
    }
    if let Some(words) = soundcloud_search_words(cleaned) {
        return soundcloud_search(words).await;
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        let path = lower.split(['?', '#']).next().unwrap_or(&lower);
        if path.ends_with(".pls") || path.ends_with(".m3u") || path.ends_with(".m3u8") {
            return playlist_file(cleaned, limit, allow_private_hosts).await;
        }
        if is_audio_ext(path) {
            let private = ensure_fetch_allowed(cleaned, false).await.is_err();
            ensure_fetch_allowed(cleaned, allow_private_hosts).await?;
            let name = path.rsplit('/').next().unwrap_or("Audio");
            let title = urlencoding::decode(name).map(|c| c.into_owned()).unwrap_or_else(|_| name.to_string());
            let mut item = Item::new(title, cleaned, Kind::Direct);
            item.allow_private_host = private && allow_private_hosts;
            return Ok(vec![item]);
        }
        if lower.contains("soundcloud.com") {
            return soundcloud_url(cleaned, limit).await;
        }
        if lower.contains("youtube.com") || lower.contains("youtu.be") {
            return youtube_link(cleaned, limit).await;
        }
        if let Some(item) = probe_stream(cleaned, allow_private_hosts).await? {
            return Ok(vec![item]);
        }
        return Err("That link isn't audio Lantern can play.".into());
    }
    youtube_search(cleaned).await
}

/// Looks up the stream for an item right before it plays.
pub async fn materialize(item: &Item) -> Result<Playable, String> {
    match item.kind {
        Kind::Direct | Kind::Radio => {
            ensure_fetch_allowed(&item.source, item.allow_private_host).await?;
            let (url, hls, adts) = if item.kind == Kind::Radio {
                match radio_target(&item.source, item.allow_private_host).await? {
                    RadioTarget::Stream(url) => (url, false, false),
                    RadioTarget::Adts(url) => (url, false, true),
                    RadioTarget::Hls(url) => (url, true, false),
                }
            } else {
                (item.source.clone(), false, false)
            };
            Ok(Playable {
                url,
                path: None,
                memory: None,
                title: item.title.clone(),
                headers: plain_headers(),
                length: item.length,
                youtube_id: None,
                size: None,
                via: None,
                notes: Vec::new(),
                live_stream: item.kind == Kind::Radio,
                hls,
                adts,
                private_ok: item.allow_private_host,
                gain: 1.0,
            })
        }
        Kind::File => {
            if !std::path::Path::new(&item.source).is_file() {
                return Err("that file isn't there anymore".into());
            }
            let path = item.source.clone();
            let gain = tokio::task::spawn_blocking(move || replaygain_db(std::path::Path::new(&path)))
                .await
                .ok()
                .flatten()
                .map_or(1.0, replaygain_gain);
            Ok(Playable {
                url: String::new(),
                path: Some(item.source.clone()),
                memory: None,
                title: item.title.clone(),
                headers: HeaderMap::new(),
                length: item.length,
                youtube_id: None,
                size: None,
                via: None,
                notes: Vec::new(),
                live_stream: false,
                hls: false,
                private_ok: false,
                adts: false,
                gain,
            })
        }
        Kind::SoundCloud => soundcloud_playable(item).await,
        Kind::YouTube => {
            let id = youtube_id(&item.source).ok_or("That isn't a YouTube link")?;
            youtube_playable(&id, &item.title, None).await
        }
        Kind::Search => {
            let query = item.query.clone().unwrap_or_else(|| item.title.clone());
            let hits = youtube_hits(&query, 5).await?;
            let lengths: Vec<Option<u64>> = hits.iter().map(|h| h.item.length).collect();
            let hit = &hits[best_match(&lengths, item.length)];
            let id = youtube_id(&hit.item.source).ok_or("YouTube result had no video")?;
            youtube_playable(&id, &item.title, None).await
        }
    }
}

/// `after` is the route that just failed to play: only the routes behind it are tried.
async fn youtube_playable(id: &str, fallback_title: &str, after: Option<&str>) -> Result<Playable, String> {
    let Found { via, stream, notes } = youtube_stream(id, after).await?;
    let gain = stream.loudness_db.map_or(1.0, youtube_gain);
    Ok(Playable {
        url: stream.url,
        path: None,
        memory: None,
        title: if stream.title.is_empty() { fallback_title.to_string() } else { stream.title },
        headers: stream.headers,
        length: stream.length,
        youtube_id: Some(id.to_string()),
        size: stream.size,
        via: Some(via),
        live_stream: false,
        hls: false,
        private_ok: false,
        adts: false,
        gain,
        notes,
    })
}

/// A YouTube stream was found but wouldn't start. The same song by the next route, or why there isn't one.
pub async fn retry_youtube(failed: &Playable) -> Result<Playable, String> {
    let (Some(id), Some(via)) = (failed.youtube_id.as_deref(), failed.via) else {
        return Err("there is no other way to play this".into());
    };
    youtube_playable(id, &failed.title, Some(via)).await
}

/// Index of the result to play. The first result within [`MATCH_TOLERANCE`]
/// seconds of the source track wins; otherwise the closest within 15 seconds;
/// otherwise the top result.
pub fn best_match(lengths: &[Option<u64>], target: Option<u64>) -> usize {
    let Some(target) = target.filter(|t| *t > 0) else { return 0 };
    if let Some(index) = lengths.iter().position(|l| l.is_some_and(|l| l.abs_diff(target) <= MATCH_TOLERANCE)) {
        return index;
    }
    lengths
        .iter()
        .enumerate()
        .filter_map(|(i, l)| l.map(|l| (i, l.abs_diff(target))))
        .filter(|(_, diff)| *diff <= 15)
        .min_by_key(|(_, diff)| *diff)
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn plain_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(WEB_UA));
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
    headers
}

// ---------------------------------------------------------------- YouTube

pub fn youtube_id(input: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"(?:youtu\.be/|youtube\.com/(?:watch\?(?:[^#]*&)?v=|embed/|shorts/|live/|v/)|music\.youtube\.com/watch\?(?:[^#]*&)?v=)([A-Za-z0-9_-]{11})",
        )
        .expect("regex")
    });
    re.captures(input).map(|caps| caps[1].to_string()).or_else(|| {
        (input.len() == 11 && input.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .then(|| input.to_string())
    })
}

/// The playlist id of a YouTube link: `list=`, or `/browse/VL…` from the playlist page.
pub fn youtube_list_id(input: &str) -> Option<String> {
    static LIST: OnceLock<Regex> = OnceLock::new();
    static BROWSE: OnceLock<Regex> = OnceLock::new();
    let list = LIST.get_or_init(|| Regex::new(r"[?&]list=([A-Za-z0-9_-]{10,})").expect("regex"));
    if let Some(id) = list.captures(input).map(|caps| caps[1].to_string()) {
        return Some(id);
    }
    let browse = BROWSE.get_or_init(|| Regex::new(r"/browse/VL([A-Za-z0-9_-]{10,})").expect("regex"));
    browse.captures(input).map(|caps| caps[1].to_string())
}

/// A video link plays that video. A playlist link (or a video inside a
/// normal playlist) queues the playlist, starting at that video.
/// Auto-generated mixes (`RD…`) are ignored when a video is named, because
/// people share songs from them by accident.
async fn youtube_link(url: &str, limit: usize) -> Result<Vec<Item>, String> {
    let video = youtube_id(url);
    let list = youtube_list_id(url).filter(|list| !(video.is_some() && is_mix(list)));
    if let Some(list) = list {
        match youtube_list(&list, video.as_deref(), limit).await {
            Ok(items) if !items.is_empty() => return Ok(items),
            Err(err) if video.is_none() => return Err(err),
            Ok(_) if video.is_none() => return Err("That playlist is empty or private.".into()),
            _ => {}
        }
    }
    let id = video.ok_or("That isn't a YouTube video or playlist link.")?;
    let title = youtube_oembed_title(&id).await.unwrap_or_else(|| id.clone());
    Ok(vec![Item::new(title, watch_url(&id), Kind::YouTube)])
}

fn is_mix(list: &str) -> bool {
    list.starts_with("RD")
}

fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

/// The watch-page side panel returns about this many videos and no further page.
const WATCH_PANEL_CAP: usize = 200;
/// How far down a long playlist to look for the linked song before giving up.
const PLAYLIST_SCAN_CAP: usize = 1000;
const PLAYLIST_PAGE_CAP: usize = 15;

/// Songs from a YouTube playlist or mix, up to `limit`, starting at `video`
/// when that song is in the list. `RD<videoId>` is the radio mix for that
/// video, which is what autoplay uses.
///
/// A normal playlist is read from the playlist page, which continues past the
/// first hundred. The watch panel is the fallback (and the only Innertube
/// source for a mix, whose playlist page is empty). yt-dlp is last.
pub async fn youtube_list(list: &str, video: Option<&str>, limit: usize) -> Result<Vec<Item>, String> {
    let limit = limit.max(1);
    // The panel always starts at the top, so ask for the whole window when
    // the linked song may sit further down than `limit`.
    let window = if video.is_some() { limit.max(WATCH_PANEL_CAP) } else { limit };
    if !is_mix(list) {
        if let Ok(items) = browse_playlist(list, video, limit).await {
            if !items.is_empty() {
                return Ok(items);
            }
        }
    }
    let panel = innertube_list(list, video, window).await.map(|items| take_from_video(items, video, limit));
    or_backup(panel, ytdlp::enabled(), || async {
        ytdlp_list(list, video, window).await.map(|items| take_from_video(items, video, limit))
    })
    .await
}

fn browse_id(list: &str) -> String {
    if list.starts_with("VL") && list.len() > 2 { list.to_string() } else { format!("VL{list}") }
}

fn youtube_context() -> Value {
    serde_json::json!({ "clientName": "WEB", "clientVersion": WEB_VERSION, "hl": "en" })
}

/// Tracks from the playlist page (`/browse`), following the continuation that
/// sits with those tracks until `limit` songs from `video` (or the start) are in.
async fn browse_playlist(list: &str, video: Option<&str>, limit: usize) -> Result<Vec<Item>, String> {
    let context = youtube_context();
    let mut body = serde_json::json!({
        "context": { "client": context.clone() },
        "browseId": browse_id(list),
    });
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    let scan_cap = if video.is_some() { PLAYLIST_SCAN_CAP.max(limit) } else { limit };
    for _ in 0..PLAYLIST_PAGE_CAP {
        let value = post_json("https://www.youtube.com/youtubei/v1/browse?prettyPrint=false", &body, &[]).await?;
        let (page, token) = parse_browse_playlist(&value);
        if page.is_empty() {
            break;
        }
        items.extend(page);
        if playlist_filled(&items, video, limit) || items.len() >= scan_cap {
            break;
        }
        let Some(token) = token else { break };
        if !seen.insert(token.clone()) {
            break;
        }
        body = serde_json::json!({
            "context": { "client": context.clone() },
            "continuation": token,
        });
    }
    let items = take_from_video(items, video, limit);
    if items.is_empty() {
        Err("YouTube didn't return that playlist. It may be private.".into())
    } else {
        Ok(items)
    }
}

/// True once `limit` tracks are available from the linked video, or from the start.
fn playlist_filled(items: &[Item], video: Option<&str>, limit: usize) -> bool {
    let start = if let Some(id) = video {
        let Some(index) = start_index(items, Some(id)) else { return false };
        index
    } else {
        0
    };
    items.len().saturating_sub(start) >= limit.max(1)
}

fn start_index(items: &[Item], video: Option<&str>) -> Option<usize> {
    let id = video?;
    items.iter().position(|item| youtube_id(&item.source).as_deref() == Some(id))
}

/// From the linked video when it was found, otherwise from the start. Capped at `limit`.
fn take_from_video(items: Vec<Item>, video: Option<&str>, limit: usize) -> Vec<Item> {
    let start = start_index(&items, video).unwrap_or(0);
    items.into_iter().skip(start).take(limit.max(1)).collect()
}

async fn innertube_list(list: &str, video: Option<&str>, limit: usize) -> Result<Vec<Item>, String> {
    let mut body = serde_json::json!({
        "context": { "client": youtube_context() },
        "playlistId": list,
    });
    if let Some(video) = video {
        body["videoId"] = Value::String(video.to_string());
    }
    let value = post_json("https://www.youtube.com/youtubei/v1/next?prettyPrint=false", &body, &[]).await?;
    let items = watch_panel_items(&value, limit);
    if items.is_empty() {
        Err("YouTube didn't return that playlist. It may be private.".into())
    } else {
        Ok(items)
    }
}

/// Videos in the watch page's playlist panel, in order. The panel is capped
/// by YouTube at about [`WATCH_PANEL_CAP`] and does not continue.
fn watch_panel_items(value: &Value, limit: usize) -> Vec<Item> {
    let Some(rows) = value["contents"]["twoColumnWatchNextResults"]["playlist"]["playlist"]["contents"].as_array() else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for row in rows {
        let Some(item) = row_item(row) else { continue };
        items.push(item);
        if items.len() >= limit {
            break;
        }
    }
    items
}

/// The track list on a playlist page, plus the continuation that loads the next part of *that* list.
/// Related shelves elsewhere on the page are left alone.
fn parse_browse_playlist(value: &Value) -> (Vec<Item>, Option<String>) {
    let mut found = Vec::new();
    collect_playlist_lists(value, &mut found);
    found.into_iter().max_by_key(|(items, _)| items.len()).unwrap_or_default()
}

fn collect_playlist_lists(value: &Value, found: &mut Vec<(Vec<Item>, Option<String>)>) {
    match value {
        Value::Object(map) => {
            if let Some(rows) = map.get("itemSectionRenderer").and_then(|v| v.get("contents")).and_then(|v| v.as_array()) {
                found.push(parse_row_list(rows));
            }
            if let Some(rows) = map.get("playlistVideoListRenderer").and_then(|v| v.get("contents")).and_then(|v| v.as_array()) {
                found.push(parse_row_list(rows));
            }
            if let Some(rows) = map.get("appendContinuationItemsAction").and_then(|v| v.get("continuationItems")).and_then(|v| v.as_array()) {
                found.push(parse_row_list(rows));
            }
            for child in map.values() {
                collect_playlist_lists(child, found);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_playlist_lists(child, found);
            }
        }
        _ => {}
    }
}

fn parse_row_list(rows: &[Value]) -> (Vec<Item>, Option<String>) {
    let mut items = Vec::new();
    let mut continuation = None;
    for row in rows {
        if let Some(item) = row_item(row) {
            items.push(item);
        } else if continuation.is_none() {
            continuation = continuation_token(row);
        }
    }
    (items, continuation)
}

fn row_item(row: &Value) -> Option<Item> {
    if let Some(lock) = row.get("lockupViewModel") {
        return lockup_item(lock);
    }
    if let Some(video) = row.get("playlistVideoRenderer").or_else(|| row.get("playlistPanelVideoRenderer")) {
        return rendered_video(video);
    }
    row.pointer("/playlistPanelVideoWrapperRenderer/primaryRenderer/playlistPanelVideoRenderer").and_then(rendered_video)
}

fn lockup_item(lock: &Value) -> Option<Item> {
    if lock["contentType"].as_str().is_some_and(|kind| kind != "LOCKUP_CONTENT_TYPE_VIDEO") {
        return None;
    }
    let id = youtube_id(lock["contentId"].as_str()?)?;
    let title = lock
        .pointer("/metadata/lockupMetadataViewModel/title/content")
        .and_then(|v| v.as_str())
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| id.clone());
    if is_unavailable(&title) {
        return None;
    }
    Some(youtube_item(&id, title, lockup_length(lock)))
}

fn lockup_length(lock: &Value) -> Option<u64> {
    let overlays = lock.pointer("/contentImage/thumbnailViewModel/overlays")?.as_array()?;
    for overlay in overlays {
        let Some(badges) = overlay.pointer("/thumbnailBottomOverlayViewModel/badges").and_then(|v| v.as_array()) else { continue };
        for badge in badges {
            let Some(text) = badge.pointer("/thumbnailBadgeViewModel/text").and_then(|v| v.as_str()) else { continue };
            if let Some(seconds) = crate::queue::parse_time(text) {
                return Some(seconds);
            }
        }
    }
    None
}

fn rendered_video(video: &Value) -> Option<Item> {
    let id = youtube_id(video["videoId"].as_str()?)?;
    let title = video["title"]["simpleText"]
        .as_str()
        .map(str::to_string)
        .or_else(|| runs_text(&video["title"]))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| id.clone());
    if is_unavailable(&title) {
        return None;
    }
    let length = video["lengthText"]["simpleText"].as_str().and_then(crate::queue::parse_time);
    Some(youtube_item(&id, title, length))
}

fn continuation_token(row: &Value) -> Option<String> {
    let token = row
        .pointer("/continuationItemViewModel/continuationCommand/innertubeCommand/continuationCommand/token")
        .or_else(|| row.pointer("/continuationItemRenderer/continuationEndpoint/continuationCommand/token"))
        .and_then(|v| v.as_str())
        .filter(|token| !token.is_empty())?;
    Some(token.to_string())
}

fn is_unavailable(title: &str) -> bool {
    matches!(title, "[Private video]" | "[Deleted video]")
}

fn youtube_item(id: &str, title: String, length: Option<u64>) -> Item {
    Item { title, source: watch_url(id), query: None, kind: Kind::YouTube, length, allow_private_host: false, requested_by: None }
}

async fn ytdlp_list(list: &str, video: Option<&str>, limit: usize) -> Result<Vec<Item>, String> {
    let page = match video {
        Some(video) => format!("https://www.youtube.com/watch?v={video}&list={list}"),
        None => format!("https://www.youtube.com/playlist?list={list}"),
    };
    Ok(ytdlp::playlist(&page, limit).await?.into_iter().filter_map(entry_item).collect())
}

fn entry_item(entry: ytdlp::Entry) -> Option<Item> {
    let id = youtube_id(&entry.id)?;
    Some(Item { title: entry.title, source: watch_url(&id), query: None, kind: Kind::YouTube, length: entry.length, allow_private_host: false, requested_by: None })
}

/// The backup's answer when the first one failed or came back empty. Both reasons are kept if both fail.
async fn or_backup<T, F, Fut>(first: Result<Vec<T>, String>, backup_on: bool, backup: F) -> Result<Vec<T>, String>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Vec<T>, String>>,
{
    let why = match first {
        Ok(found) if !found.is_empty() => return Ok(found),
        Ok(_) => "YouTube returned nothing".to_string(),
        Err(why) => why,
    };
    if !backup_on {
        return Err(why);
    }
    match backup().await {
        Ok(found) if !found.is_empty() => Ok(found),
        Ok(_) => Err(why),
        Err(backup) => Err(format!("{why} (yt-dlp: {backup})")),
    }
}

fn runs_text(value: &Value) -> Option<String> {
    let runs = value["runs"].as_array()?;
    let text: String = runs.iter().filter_map(|r| r["text"].as_str()).collect();
    (!text.is_empty()).then_some(text)
}

async fn youtube_oembed_title(id: &str) -> Option<String> {
    let url = format!("https://www.youtube.com/oembed?url=https://www.youtube.com/watch?v={id}&format=json");
    let value: Value = serde_json::from_str(&get_text(&url).await.ok()?).ok()?;
    value["title"].as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

pub(crate) const WEB_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36";
const WEB_VERSION: &str = "2.20260708.00.00";
/// Public key embedded in YouTube's own web player (same for every visitor); not a secret.
const INNERTUBE_KEY: &str = "AIzaSyAO_FJ2SlqU8Q4STEHLGCilw_Y9_11qcW8";

/// One Innertube client Lantern can pretend to be. If YouTube refuses the
/// first, the next is tried.
struct Client {
    /// What the activity log calls it.
    label: &'static str,
    name: &'static str,
    id: &'static str,
    version: &'static str,
    user_agent: &'static str,
    extra: fn() -> Value,
}

const CLIENTS: &[Client] = &[
    Client {
        label: "Vision",
        name: "VISIONOS",
        id: "101",
        version: "1.02",
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15",
        extra: || {
            serde_json::json!({
                "deviceMake": "Apple", "deviceModel": "RealityDevice17,1",
                "osName": "visionOS", "osVersion": "26.5.23O471",
            })
        },
    },
    Client {
        label: "Android VR",
        name: "ANDROID_VR",
        id: "28",
        version: "1.65.10",
        user_agent: "com.google.android.apps.youtube.vr.oculus/1.65.10 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip",
        extra: || {
            serde_json::json!({
                "deviceMake": "Oculus", "deviceModel": "Quest 3",
                "androidSdkVersion": 32, "osName": "Android", "osVersion": "12L",
            })
        },
    },
    Client {
        label: "iOS",
        name: "IOS",
        id: "5",
        version: "20.10.4",
        user_agent: "com.google.ios.youtube/20.10.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)",
        extra: || {
            serde_json::json!({
                "deviceMake": "Apple", "deviceModel": "iPhone16,2",
                "osName": "iPhone", "osVersion": "18.3.2.22D82",
            })
        },
    },
];

struct YouTubeStream {
    /// How much louder than YouTube's reference this video is, in dB, when YouTube says.
    loudness_db: Option<f64>,
    title: String,
    url: String,
    length: Option<u64>,
    size: Option<u64>,
    headers: HeaderMap,
}

/// A stream and how it was found.
struct Found {
    via: &'static str,
    stream: YouTubeStream,
    notes: Vec<String>,
}

/// The last route, used when the Innertube clients all fail.
const BACKUP: &str = "yt-dlp";
/// One client gets this long to answer and to start serving audio.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(15);

/// The routes to a YouTube stream in the order they are tried: each Innertube
/// client, then yt-dlp if it is on. `after` skips everything up to and
/// including a route that already failed to play.
fn routes(backup: bool, after: Option<&str>) -> Vec<&'static str> {
    let mut all: Vec<&'static str> = CLIENTS.iter().map(|c| c.label).chain([BACKUP]).collect();
    let from = after.and_then(|a| all.iter().position(|r| *r == a)).map_or(0, |i| i + 1);
    all.drain(..from);
    if !backup {
        all.retain(|r| *r != BACKUP);
    }
    all
}

/// Tries each route until one works. Returns it with a note for every route passed over.
async fn first_route<T, F, Fut>(routes: &[&'static str], mut attempt: F) -> Result<(&'static str, T, Vec<String>), String>
where
    F: FnMut(&'static str) -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let mut failed: Vec<(&'static str, String)> = Vec::new();
    for &route in routes {
        match attempt(route).await {
            Ok(found) => {
                let notes = failed.iter().map(|(r, why)| format!("{r}: {}", why.chars().take(80).collect::<String>())).collect();
                return Ok((route, found, notes));
            }
            Err(why) => failed.push((route, why)),
        }
    }
    Err(all_failed(&failed))
}

/// The first reason, plus yt-dlp's when it was the last resort.
fn all_failed(failed: &[(&str, String)]) -> String {
    match failed {
        [] => "YouTube refused this video".into(),
        [(BACKUP, why)] => format!("yt-dlp: {why}"),
        [(_, first), .., (BACKUP, last)] => format!("{first} (yt-dlp: {last})"),
        [(_, first), ..] => first.clone(),
    }
}

async fn youtube_stream(id: &str, after: Option<&str>) -> Result<Found, String> {
    let routes = routes(ytdlp::enabled(), after);
    if routes.is_empty() {
        return Err("YouTube refused this video, and there is no other way to try".into());
    }
    let (via, stream, notes) = first_route(&routes, |route| try_route(route, id)).await?;
    Ok(Found { via, stream, notes })
}

/// Looks the stream up one way, then opens it for a moment. A link YouTube
/// refuses (403, 410, ...) counts as a failure of that route, so the next one is tried.
async fn try_route(route: &'static str, id: &str) -> Result<YouTubeStream, String> {
    let work = async {
        let stream = if route == BACKUP {
            ytdlp_stream(id).await?
        } else {
            let client = CLIENTS.iter().find(|c| c.label == route).ok_or("unknown YouTube client")?;
            youtube_stream_with(client, id).await?
        };
        probe(stream_client(), &stream).await?;
        Ok(stream)
    };
    if route == BACKUP {
        // yt-dlp has its own limits, and the first run may be downloading it.
        return work.await;
    }
    match tokio::time::timeout(CLIENT_TIMEOUT, work).await {
        Ok(result) => result,
        Err(_) => Err(format!("{route} took too long")),
    }
}

async fn ytdlp_stream(id: &str) -> Result<YouTubeStream, String> {
    let audio = ytdlp::audio(id).await?;
    // The title isn't in the link. The queue already has it.
    Ok(YouTubeStream { loudness_db: None, title: String::new(), url: audio.url, length: audio.length, size: audio.size, headers: youtube_headers(WEB_UA) })
}

fn youtube_headers(user_agent: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(agent) = HeaderValue::from_str(user_agent) {
        headers.insert(USER_AGENT, agent);
    }
    headers.insert(REFERER, HeaderValue::from_static("https://www.youtube.com/"));
    headers.insert(ORIGIN, HeaderValue::from_static("https://www.youtube.com"));
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
    headers
}

/// Asks for the first two bytes the way the player will.
async fn probe(client: &reqwest::Client, stream: &YouTubeStream) -> Result<(), String> {
    let request = if stream.size.is_some_and(|s| s > 0) {
        let sep = if stream.url.contains('?') { '&' } else { '?' };
        client.get(format!("{}{sep}range=0-1", stream.url))
    } else {
        client.get(&stream.url).header(RANGE, "bytes=0-1")
    };
    let response = request
        .headers(stream.headers.clone())
        .send()
        .await
        .map_err(|e| format!("couldn't open the audio: {}", describe(&e)))?;
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("YouTube refused the audio link ({status})"))
    }
}

async fn youtube_stream_with(client: &Client, id: &str) -> Result<YouTubeStream, String> {
    let (visitor, key) = youtube_session().await?;
    let mut context = serde_json::json!({
        "clientName": client.name,
        "clientVersion": client.version,
        "userAgent": client.user_agent,
        "hl": "en",
        "timeZone": "UTC",
        "utcOffsetMinutes": 0,
        "visitorData": visitor,
    });
    if let (Some(target), Value::Object(extra)) = (context.as_object_mut(), (client.extra)()) {
        target.extend(extra);
    }
    let body = serde_json::json!({
        "context": { "client": context },
        "videoId": id,
        "contentCheckOk": true,
        "racyCheckOk": true,
        "playbackContext": { "contentPlaybackContext": { "html5Preference": "HTML5_PREF_WANTS" } }
    });
    let value = post_json(
        &format!("https://www.youtube.com/youtubei/v1/player?prettyPrint=false&key={key}"),
        &body,
        &[
            ("User-Agent", client.user_agent),
            ("X-YouTube-Client-Name", client.id),
            ("X-YouTube-Client-Version", client.version),
            ("X-Goog-Visitor-Id", &visitor),
            ("Origin", "https://www.youtube.com"),
        ],
    )
    .await?;
    let (title, url, length, size) = pick_audio(&value)?;
    let loudness_db = youtube_loudness(&value);
    Ok(YouTubeStream { loudness_db, title, url, length, size, headers: youtube_headers(client.user_agent) })
}

static SESSION: Mutex<Option<(Instant, String, String)>> = Mutex::new(None);

/// Drops the cached visitor data, so the next YouTube lookup starts a fresh
/// session. Used when a stream dies right after it started.
pub fn forget_youtube_session() {
    *SESSION.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

/// Visitor data and API key, reused for an hour instead of loading a watch page per song.
async fn youtube_session() -> Result<(String, String), String> {
    const CACHE: &Mutex<Option<(Instant, String, String)>> = &SESSION;
    if let Some((at, visitor, key)) = CACHE.lock().unwrap_or_else(|p| p.into_inner()).clone() {
        if at.elapsed() < Duration::from_secs(3600) {
            return Ok((visitor, key));
        }
    }
    let fresh = new_youtube_session().await?;
    *CACHE.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now(), fresh.0.clone(), fresh.1.clone()));
    Ok(fresh)
}

async fn new_youtube_session() -> Result<(String, String), String> {
    let body = serde_json::json!({
        "context": { "client": { "clientName": "WEB", "clientVersion": WEB_VERSION, "hl": "en" } }
    });
    if let Ok(value) = post_json("https://www.youtube.com/youtubei/v1/visitor_id?prettyPrint=false", &body, &[]).await {
        if let Some(visitor) = value["responseContext"]["visitorData"].as_str() {
            return Ok((visitor.to_string(), INNERTUBE_KEY.to_string()));
        }
    }
    let page = web()
        .get("https://www.youtube.com/?hl=en")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Cookie", "SOCS=CAI")
        .send()
        .await
        .map_err(|e| format!("Couldn't reach YouTube: {e}"))?
        .text()
        .await
        .map_err(|e| format!("Couldn't reach YouTube: {e}"))?;
    let find = |pattern: &str| Regex::new(pattern).ok().and_then(|re| re.captures(&page).map(|c| c[1].to_string()));
    let visitor = find(r#""visitorData"\s*:\s*"([^"]+)""#).ok_or("Could not start a YouTube session")?;
    let key = find(r#""INNERTUBE_API_KEY"\s*:\s*"([^"]+)""#).unwrap_or_else(|| INNERTUBE_KEY.to_string());
    Ok((visitor, key))
}

async fn post_json(url: &str, body: &Value, headers: &[(&str, &str)]) -> Result<Value, String> {
    let mut request = web().post(url).header("Content-Type", "application/json").json(body);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.send().await.map_err(|e| describe(&e))?;
    let response = response.error_for_status().map_err(|e| describe(&e))?;
    response.json::<Value>().await.map_err(|e| describe(&e))
}

fn describe(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "the server took too long to answer".into()
    } else if err.is_connect() {
        "couldn't connect to the server".into()
    } else {
        err.to_string()
    }
}

/// YouTube's loudness figure for a video: positive means louder than its
/// reference level, so YouTube itself would turn it down by that much.
fn youtube_loudness(value: &Value) -> Option<f64> {
    value["playerConfig"]["audioConfig"]["loudnessDb"]
        .as_f64()
        .or_else(|| value["streamingData"]["adaptiveFormats"].as_array()?.iter().find_map(|f| f["loudnessDb"].as_f64()))
        .filter(|db| db.is_finite())
}

/// Turns a loud YouTube video down to the reference level, like YouTube's own
/// player. Quiet ones are left alone rather than boosted (boosting can clip).
pub fn youtube_gain(loudness_db: f64) -> f32 {
    let cut = loudness_db.clamp(0.0, 20.0);
    10f64.powf(-cut / 20.0) as f32
}

/// The track's ReplayGain (dB) from its tags, or the album's when there is no track value.
pub fn replaygain_db(path: &Path) -> Option<f64> {
    use lofty::prelude::*;
    let file = lofty::read_from_path(path).ok()?;
    let tag = file.primary_tag().or_else(|| file.first_tag())?;
    [ItemKey::ReplayGainTrackGain, ItemKey::ReplayGainAlbumGain]
        .iter()
        .find_map(|key| tag.get_string(*key).and_then(parse_db))
}

/// `"-7.23 dB"` → -7.23.
pub fn parse_db(text: &str) -> Option<f64> {
    let number = text.trim().trim_end_matches(|c: char| c.is_alphabetic() || c.is_whitespace());
    number.trim().trim_start_matches('+').parse::<f64>().ok().filter(|db| db.is_finite())
}

/// ReplayGain as a volume multiplier. Boosts are capped at +6 dB, cuts at -20 dB.
pub fn replaygain_gain(db: f64) -> f32 {
    10f64.powf(db.clamp(-20.0, 6.0) / 20.0) as f32
}

/// Title, stream link, length in seconds, and file size in bytes.
fn pick_audio(value: &Value) -> Result<(String, String, Option<u64>, Option<u64>), String> {
    let status = value["playabilityStatus"]["status"].as_str().unwrap_or("");
    if status != "OK" {
        let reason = value["playabilityStatus"]["reason"].as_str().unwrap_or("YouTube refused this video");
        return Err(reason.to_string());
    }
    let title = value["videoDetails"]["title"].as_str().unwrap_or("").to_string();
    let length = value["videoDetails"]["lengthSeconds"].as_str().and_then(|s| s.parse().ok());
    let mut formats = Vec::new();
    if let Some(list) = value["streamingData"]["adaptiveFormats"].as_array() {
        formats.extend(list);
    }
    if let Some(list) = value["streamingData"]["formats"].as_array() {
        formats.extend(list);
    }
    let mut best: Option<&Value> = None;
    let mut best_score: i64 = i64::MIN;
    for format in formats {
        let url = format["url"].as_str().unwrap_or("");
        if url.is_empty() {
            continue;
        }
        let mime = format["mimeType"].as_str().unwrap_or("");
        if !mime.starts_with("audio/") && !mime.contains("mp4a") {
            continue;
        }
        let itag = format["itag"].as_i64().unwrap_or(0);
        let quality: i64 = match itag {
            140 => 100,
            141 => 90,
            _ if mime.contains("audio/mp4") || mime.contains("mp4a") => 80,
            139 => 70,
            _ => 10,
        };
        // Several itag 140s can exist. The first is often a non-English autodub.
        let score = quality + language_score(format);
        if score > best_score {
            best_score = score;
            best = Some(format);
        }
    }
    let chosen = best.ok_or("YouTube did not return an audio stream")?;
    let url = chosen["url"].as_str().unwrap_or("").to_string();
    let size = chosen["contentLength"].as_str().and_then(|s| s.parse().ok());
    Ok((title, url, length, size))
}

/// Prefer English, then the original track, and keep autodubs and audio-description tracks behind those.
fn language_score(format: &Value) -> i64 {
    let (lang, content, drc, name) = audio_labels(format);
    let english = lang == "en" || lang.starts_with("en-") || lang.starts_with("en_") || name.starts_with("english");
    let known = !lang.is_empty() || name.starts_with("english");
    let mut score = 0;
    if english {
        score += 1000;
    } else if known {
        score -= 1000;
    }
    if content == "original" || name.contains("original") {
        score += 400;
    }
    if content == "dubbed" || content == "dubbed-auto" || name.contains("dub") {
        score -= 800;
    }
    if content == "descriptive" || name.contains("descriptive") {
        score -= 600;
    }
    if drc {
        score -= 30;
    }
    score
}

fn audio_labels(format: &Value) -> (String, String, bool, String) {
    let url = format["url"].as_str().unwrap_or("");
    let decoded = urlencoding::decode(url).unwrap_or(std::borrow::Cow::Borrowed(url));
    let mut lang = String::new();
    let mut content = String::new();
    let mut drc = format["isDrc"].as_bool().unwrap_or(false);
    for blob in [decoded.as_ref(), url] {
        let Some(start) = blob.find("xtags=") else { continue };
        let raw = blob[start + 6..].split(['&', '"']).next().unwrap_or("");
        let value = urlencoding::decode(raw).unwrap_or(std::borrow::Cow::Borrowed(raw));
        for part in value.split(':') {
            let Some((key, val)) = part.split_once('=') else { continue };
            match key {
                "lang" if lang.is_empty() => lang = val.to_ascii_lowercase(),
                "acont" if content.is_empty() => content = val.to_ascii_lowercase(),
                "drc" if val == "1" => drc = true,
                _ => {}
            }
        }
    }
    let id = format["audioTrack"]["id"].as_str().unwrap_or("");
    if lang.is_empty() && !id.is_empty() {
        lang = id.split('.').next().unwrap_or("").to_ascii_lowercase();
    }
    let name = format["audioTrack"]["displayName"].as_str().unwrap_or("").to_ascii_lowercase();
    (lang, content, drc, name)
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub item: Item,
    pub channel: String,
}

/// Top YouTube results for a text search. Used by the picker and by Spotify/Apple matching.
pub async fn youtube_hits(query: &str, limit: usize) -> Result<Vec<Hit>, String> {
    let first = rusty_hits(query, limit).await;
    or_backup(first, ytdlp::enabled(), || ytdlp_hits(query, limit)).await
}

async fn ytdlp_hits(query: &str, limit: usize) -> Result<Vec<Hit>, String> {
    let entries = ytdlp::search(query, limit.clamp(1, 8)).await?;
    let hits = entries.into_iter().map(|e| (e.channel.clone(), entry_item(e)));
    Ok(hits.filter_map(|(channel, item)| Some(Hit { item: item?, channel })).take(limit).collect())
}

async fn rusty_hits(query: &str, limit: usize) -> Result<Vec<Hit>, String> {
    let options = SearchOptions { limit: limit.clamp(1, 8) as u64, search_type: SearchType::Video, safe_search: false };
    let results = timed("YouTube search", async {
        let youtube = YouTube::new().map_err(|e| e.to_string())?;
        youtube.search(query, Some(&options)).await.map_err(|e| e.to_string())
    })
    .await?;
    let mut hits = Vec::new();
    for result in results {
        let SearchResult::Video(video) = result else { continue };
        if video.id.is_empty() {
            continue;
        }
        hits.push(Hit {
            item: Item {
                title: video.title,
                source: watch_url(&video.id),
                query: None,
                kind: Kind::YouTube,
                length: seconds_from_ms(video.duration),
                allow_private_host: false,
                requested_by: None,
            },
            channel: video.channel.name,
        });
        if hits.len() >= limit {
            break;
        }
    }
    if hits.is_empty() {
        return Err("No YouTube result".into());
    }
    Ok(hits)
}

/// rusty_ytdl reports lengths in milliseconds.
fn seconds_from_ms(ms: u64) -> Option<u64> {
    (ms > 0).then_some(ms / 1000)
}

async fn youtube_search(query: &str) -> Result<Vec<Item>, String> {
    Ok(youtube_hits(query, 1).await?.into_iter().map(|hit| hit.item).collect())
}

// ------------------------------------------------------ Spotify and Apple

fn spotify_id(url: &str) -> Option<(String, String)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?:open\.spotify\.com/(?:intl-[a-z]+/)?|spotify:)(track|album|playlist|episode)[/:]([A-Za-z0-9]+)")
            .expect("regex")
    });
    let caps = re.captures(url)?;
    Some((caps[1].to_string(), caps[2].to_string()))
}

fn search_item(title: String, artist: String, source: &str, length: Option<u64>) -> Item {
    let query = if artist.is_empty() { title.clone() } else { format!("{artist} - {title}") };
    let shown = if artist.is_empty() { title } else { format!("{artist} - {title}") };
    Item { title: shown, source: source.to_string(), query: Some(query), kind: Kind::Search, length, allow_private_host: false, requested_by: None }
}

async fn spotify(url: &str, limit: usize) -> Result<Vec<Item>, String> {
    let (kind, id) = spotify_id(url).ok_or("Not a Spotify link")?;
    let html = get_text(&format!("https://open.spotify.com/embed/{kind}/{id}")).await?;
    let re = Regex::new(r#"<script id="__NEXT_DATA__" type="application/json">(.*?)</script>"#).expect("regex");
    let json = re.captures(&html).and_then(|c| c.get(1)).ok_or("Spotify did not return track data")?.as_str();
    let value: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let entity = &value["props"]["pageProps"]["state"]["data"]["entity"];
    let ms = |v: &Value| v.as_u64().and_then(seconds_from_ms);
    let mut items = Vec::new();
    if let Some(rows) = entity["trackList"].as_array() {
        for row in rows.iter().take(limit) {
            let title = clean(row["title"].as_str().unwrap_or(""));
            if title.is_empty() {
                continue;
            }
            let artist = clean(row["subtitle"].as_str().unwrap_or(""));
            items.push(search_item(title, artist, url, ms(&row["duration"])));
        }
    } else {
        let title = clean(entity["title"].as_str().or_else(|| entity["name"].as_str()).unwrap_or(""));
        if title.is_empty() {
            return Err("Spotify track had no title".into());
        }
        let artist = entity["artists"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|a| a["name"].as_str())
                    .map(clean)
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        items.push(search_item(title, artist, url, ms(&entity["duration"])));
    }
    if items.is_empty() {
        Err("Spotify returned no songs".into())
    } else {
        Ok(items)
    }
}

async fn apple(url: &str, limit: usize) -> Result<Vec<Item>, String> {
    let re = Regex::new(r"(?:music|itunes)\.apple\.com/(?:[a-z]{2}/)?(song|album|playlist)/[^/]+/([^/?#]+)").expect("regex");
    let caps = re.captures(url).ok_or("Not an Apple Music link")?;
    let kind = caps[1].to_string();
    let ident = urlencoding::decode(&caps[2]).map(|c| c.into_owned()).unwrap_or_else(|_| caps[2].to_string());
    let song_id = url.split("i=").nth(1).and_then(|rest| rest.split('&').next()).map(str::to_string);
    let results = if kind == "song" || song_id.is_some() {
        itunes_lookup(&[song_id.unwrap_or(ident)], "").await?
    } else if kind == "album" && ident.chars().all(|c| c.is_ascii_digit()) {
        itunes_lookup(&[ident], "song").await?
    } else {
        let html = get_text(url).await?;
        let block = Regex::new(r#"(?s)<script[^>]*type="application/ld\+json"[^>]*>(.*?)</script>"#)
            .expect("regex")
            .captures(&html)
            .map(|c| c[1].to_string())
            .unwrap_or_default();
        let value: Value = serde_json::from_str(&block).unwrap_or(Value::Null);
        let id_re = Regex::new(r"/(\d+)(?:\?|$)").expect("regex");
        let ids: Vec<String> = value["track"]
            .as_array()
            .map(|tracks| {
                tracks
                    .iter()
                    .take(limit)
                    .filter_map(|t| t["url"].as_str())
                    .filter_map(|link| id_re.captures(link).map(|c| c[1].to_string()))
                    .collect()
            })
            .unwrap_or_default();
        itunes_lookup(&ids, "").await?
    };
    let mut items = Vec::new();
    for item in results {
        if item["wrapperType"].as_str() == Some("collection") {
            continue;
        }
        let title = clean(item["trackName"].as_str().unwrap_or(""));
        if title.is_empty() {
            continue;
        }
        let artist = clean(item["artistName"].as_str().unwrap_or(""));
        let length = item["trackTimeMillis"].as_u64().and_then(seconds_from_ms);
        items.push(search_item(title, artist, item["trackViewUrl"].as_str().unwrap_or(url), length));
        if items.len() >= limit {
            break;
        }
    }
    if items.is_empty() {
        Err("Apple Music returned no songs".into())
    } else {
        Ok(items)
    }
}

async fn itunes_lookup(ids: &[String], entity: &str) -> Result<Vec<Value>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let extra = if entity.is_empty() { String::new() } else { format!("&entity={entity}") };
    let url = format!("https://itunes.apple.com/lookup?id={}{}&limit=200", ids.join(","), extra);
    let value: Value = serde_json::from_str(&get_text(&url).await?).map_err(|e| e.to_string())?;
    Ok(value["results"].as_array().cloned().unwrap_or_default())
}

// ------------------------------------------------------- Radio and files

/// Where a station's audio really is (1.10.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioTarget {
    /// Plain audio over HTTP (MP3, AAC, Ogg, ICY).
    Stream(String),
    /// Plain AAC in ADTS frames, which may carry headers Symphonia skips over.
    Adts(String),
    /// A live HLS playlist.
    Hls(String),
}

/// What a station link answered with, from its first bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioAnswer {
    Audio,
    Hls,
    /// A `.pls` / `.m3u` playlist: the first stream link in it.
    Playlist(String),
    /// A playlist with no stream link, or a web page.
    Nothing,
}

/// Reads the start of a station's answer. `content_type` is the server's.
pub fn classify_radio(content_type: &str, head: &[u8]) -> RadioAnswer {
    let text = String::from_utf8_lossy(&head[..head.len().min(64 * 1024)]);
    let start = text.trim_start_matches('\u{feff}').trim_start();
    let ct = content_type.to_ascii_lowercase();
    if start.starts_with("#EXTM3U") && text.contains("#EXT-X-") {
        return RadioAnswer::Hls;
    }
    let playlist_type = ct.contains("mpegurl") || ct.contains("scpls") || ct.contains("x-ms-asf") || ct.starts_with("text/");
    // A bare list of links (an `.m3u` without a header) counts too.
    let bare_links = looks_like_text(head) && (start.starts_with("http://") || start.starts_with("https://"));
    if playlist_type || bare_links || start.starts_with("#EXTM3U") || start.to_ascii_lowercase().starts_with("[playlist]") {
        let pls = Regex::new(r"(?im)^\s*File\d+\s*=\s*(https?://\S+)").expect("regex");
        if let Some(c) = pls.captures(&text) {
            return RadioAnswer::Playlist(c[1].to_string());
        }
        let link = text.lines().map(str::trim).find(|l| l.starts_with("http://") || l.starts_with("https://"));
        if let Some(link) = link {
            return RadioAnswer::Playlist(link.to_string());
        }
        if playlist_type || start.starts_with('#') || start.starts_with('[') {
            return RadioAnswer::Nothing;
        }
    }
    RadioAnswer::Audio
}

/// Follows a station link to its audio: unwraps `.pls`/`.m3u` playlists and
/// spots HLS (1.10.1). A server that won't answer plain HTTP (old Shoutcast)
/// is left to the player, which handles `ICY 200 OK`.
pub async fn radio_target(url: &str, allow_private: bool) -> Result<RadioTarget, String> {
    let mut current = url.to_string();
    for _ in 0..4 {
        let response = match user_request(&current, allow_private, |req| req.header("Icy-MetaData", "0")).await {
            Ok(response) => response,
            Err(err) if err == PRIVATE_HOST_MSG => return Err(err),
            Err(_) => return Ok(RadioTarget::Stream(current)),
        };
        if !response.status().is_success() {
            return Err(format!("the station answered {}", response.status()));
        }
        let content_type = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let head = radio_head(response).await;
        match classify_radio(&content_type, &head) {
            RadioAnswer::Audio if is_adts(&content_type, &head) => return Ok(RadioTarget::Adts(current)),
            RadioAnswer::Audio => return Ok(RadioTarget::Stream(current)),
            // The link itself, not where it redirected: some hand out short-lived tokens.
            RadioAnswer::Hls => return Ok(RadioTarget::Hls(current)),
            RadioAnswer::Playlist(next) => {
                ensure_fetch_allowed(&next, allow_private).await?;
                current = next;
            }
            RadioAnswer::Nothing => return Err("the station's link doesn't lead to audio".into()),
        }
    }
    Err("the station's playlists point to each other".into())
}

/// The first bytes of an answer: up to 64 KB, for at most 8 seconds.
async fn radio_head(response: reqwest::Response) -> Vec<u8> {
    use futures::StreamExt;
    let mut head = Vec::new();
    let mut body = response.bytes_stream();
    let read = async {
        while let Some(Ok(chunk)) = body.next().await {
            head.extend_from_slice(&chunk);
            // Audio is recognised from its first bytes; a playlist is small.
            if head.len() >= 64 * 1024 || (head.len() >= 16 && !looks_like_text(&head)) {
                break;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(8), read).await;
    head
}

/// AAC in ADTS frames: said by the answer's type, or seen as two frames in a row.
/// (MP3 frames have layer bits set, so `0xFFF0`-style syncs are AAC.)
fn is_adts(content_type: &str, head: &[u8]) -> bool {
    let kind = content_type.to_ascii_lowercase();
    if kind.contains("aac") {
        return true;
    }
    if kind.contains("mpeg") || kind.contains("ogg") || kind.contains("opus") {
        return false;
    }
    let sync = |i: usize| head.len() > i + 1 && head[i] == 0xff && head[i + 1] & 0xf6 == 0xf0;
    (0..head.len().saturating_sub(7)).any(|i| {
        let len = (usize::from(head[i + 3] & 3) << 11) | (usize::from(head[i + 4]) << 3) | usize::from(head[i + 5] >> 5);
        sync(i) && len > 7 && sync(i + len)
    })
}

/// Playlists are text; audio has zero bytes or MPEG sync bytes early on.
fn looks_like_text(bytes: &[u8]) -> bool {
    !bytes.iter().take(512).any(|&b| b == 0 || b == 0xff || b < 0x09)
}

async fn playlist_file(url: &str, limit: usize, allow_private_hosts: bool) -> Result<Vec<Item>, String> {
    let text = get_text_user(url, allow_private_hosts).await?;
    if text.contains("#EXT-X-") {
        // A live HLS station (1.10.1): played by following the playlist.
        let private = ensure_fetch_allowed(url, false).await.is_err();
        let mut item = Item::new("Radio", url, Kind::Radio);
        item.allow_private_host = private && allow_private_hosts;
        return Ok(vec![item]);
    }
    let mut items = Vec::new();
    if url.to_ascii_lowercase().contains(".pls") {
        let files = Regex::new(r"(?im)^File(\d+)\s*=\s*(.+)$").expect("regex");
        let titles = Regex::new(r"(?im)^Title(\d+)\s*=\s*(.+)$").expect("regex");
        let names: std::collections::HashMap<String, String> =
            titles.captures_iter(&text).map(|c| (c[1].to_string(), c[2].trim().to_string())).collect();
        for caps in files.captures_iter(&text) {
            let target = caps[2].trim();
            if target.starts_with("http") {
                let private = ensure_fetch_allowed(target, false).await.is_err();
                ensure_fetch_allowed(target, allow_private_hosts).await?;
                let title = names.get(&caps[1]).cloned().unwrap_or_else(|| "Radio".into());
                let mut item = Item::new(title, target, Kind::Radio);
                item.allow_private_host = private && allow_private_hosts;
                items.push(item);
            }
        }
    } else {
        let mut pending = None;
        for line in text.lines().map(str::trim) {
            if line.to_ascii_uppercase().starts_with("#EXTINF") {
                pending = line.split_once(',').map(|(_, t)| t.trim().to_string());
            } else if line.starts_with("http") {
                let private = ensure_fetch_allowed(line, false).await.is_err();
                ensure_fetch_allowed(line, allow_private_hosts).await?;
                let title = pending.take().unwrap_or_else(|| "Radio".into());
                let mut item = Item::new(title, line, Kind::Radio);
                item.allow_private_host = private && allow_private_hosts;
                items.push(item);
            }
        }
    }
    items.truncate(limit);
    if items.is_empty() {
        Err("Playlist had no stream links".into())
    } else {
        Ok(items)
    }
}

/// An unknown link plays if the server says it's audio. No length means live radio.
async fn probe_stream(url: &str, allow_private_hosts: bool) -> Result<Option<Item>, String> {
    let private = ensure_fetch_allowed(url, false).await.is_err();
    ensure_fetch_allowed(url, allow_private_hosts).await?;
    let mut map = HeaderMap::new();
    match user_request(url, allow_private_hosts, |req| req.header("Icy-MetaData", "0")).await {
        Ok(response) => map = response.headers().clone(),
        Err(err) => {
            // Old Shoutcast servers answer `ICY 200 OK`, which the HTTP client rejects.
            if !url.to_ascii_lowercase().starts_with("http://") {
                return Err(err);
            }
            let (found, _) = crate::stream::icy_open(url, &HeaderMap::new()).await.map_err(|_| err)?;
            for (name, value) in found {
                if let (Ok(name), Ok(value)) = (reqwest::header::HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(&value)) {
                    map.insert(name, value);
                }
            }
            map.remove("content-length");
            if map.get("content-type").is_none() && map.get("icy-name").is_none() {
                map.insert("icy-name", HeaderValue::from_static("Radio"));
            }
        }
    }
    let headers = &map;
    let text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    let icy = text("icy-name").or_else(|| text("ice-name")).filter(|s| !s.trim().is_empty());
    let ctype = text("content-type").unwrap_or_default().to_ascii_lowercase();
    let sized = headers.get("content-length").is_some();
    if ctype.contains("mpegurl") {
        return Ok(None);
    }
    if icy.is_none() && !ctype.starts_with("audio/") && ctype != "application/ogg" {
        return Ok(None);
    }
    let kind = if icy.is_some() || !sized { Kind::Radio } else { Kind::Direct };
    let title = icy.unwrap_or_else(|| url.rsplit('/').next().unwrap_or("Radio").to_string());
    let mut item = Item::new(title, url, kind);
    item.allow_private_host = private && allow_private_hosts;
    Ok(Some(item))
}

fn is_audio_ext(path: &str) -> bool {
    [".mp3", ".ogg", ".opus", ".flac", ".wav", ".m4a", ".aac", ".webm"].iter().any(|ext| path.ends_with(ext))
}

/// How many local tracks one Add can put on the queue.
pub const LOCAL_TRACK_LIMIT: usize = 1000;

const AUDIO_EXTS: &[&str] = &["mp3", "ogg", "opus", "flac", "wav", "m4a", "aac", "webm"];

/// File name without the extension. Never the folder.
pub fn local_title(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Audio".into())
}

pub fn local_item(path: &Path) -> Item {
    Item {
        title: local_title(path),
        source: path.to_string_lossy().into_owned(),
        query: None,
        kind: Kind::File,
        length: None,
        allow_private_host: false,
        requested_by: None,
    }
}

/// An audio file Lantern can play, judged by its extension.
pub fn is_audio_path(path: &Path) -> bool {
    is_audio_file(path)
}

pub(crate) fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| AUDIO_EXTS.iter().any(|want| ext.eq_ignore_ascii_case(want)))
}

/// Audio files from the paths the window picked. A directory includes the
/// directories inside it. Hidden names are skipped unless the user picked
/// that file itself. The bool is set when there were more than [`LOCAL_TRACK_LIMIT`].
pub fn collect_local_audio(roots: &[PathBuf]) -> (Vec<Item>, bool) {
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    let mut truncated = false;
    for root in roots {
        if walk_audio(root, 0, &mut seen, &mut items) {
            truncated = true;
            break;
        }
    }
    items.sort_by(|a, b| natural_cmp(&a.source, &b.source));
    (items, truncated)
}

/// True when the limit was hit with something still left to add.
fn walk_audio(path: &Path, depth: usize, seen: &mut HashSet<PathBuf>, items: &mut Vec<Item>) -> bool {
    if items.len() >= LOCAL_TRACK_LIMIT || depth > 32 {
        return items.len() >= LOCAL_TRACK_LIMIT;
    }
    let Ok(canon) = path.canonicalize() else { return false };
    if !seen.insert(canon) {
        return false;
    }
    if path.is_dir() {
        let Ok(read) = std::fs::read_dir(path) else { return false };
        let mut kids: Vec<PathBuf> = read
            .flatten()
            .map(|entry| entry.path())
            .filter(|child| child.file_name().and_then(|n| n.to_str()).is_none_or(|n| !n.starts_with('.')))
            .collect();
        kids.sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
        for (index, child) in kids.iter().enumerate() {
            if walk_audio(child, depth + 1, seen, items) {
                return true;
            }
            if items.len() >= LOCAL_TRACK_LIMIT && index + 1 < kids.len() {
                return true;
            }
        }
        return false;
    }
    if is_audio_file(path) {
        items.push(local_item(path));
    }
    false
}

/// `2` before `10`, and case doesn't matter. Used so album folders stay in order.
pub(crate) fn natural_cmp(a: &str, b: &str) -> Ordering {
    let a = a.to_lowercase();
    let b = b.to_lowercase();
    let (mut ia, mut ib) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ia.peek().copied(), ib.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) if ca.is_ascii_digit() && cb.is_ascii_digit() => {
                let na = take_digits(&mut ia);
                let nb = take_digits(&mut ib);
                let cmp = na.len().cmp(&nb.len()).then_with(|| na.cmp(&nb));
                if cmp != Ordering::Equal {
                    return cmp;
                }
            }
            (Some(ca), Some(cb)) => {
                ia.next();
                ib.next();
                if ca != cb {
                    return ca.cmp(&cb);
                }
            }
        }
    }
}

fn take_digits(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut out = String::new();
    while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
        out.push(chars.next().expect("peeked"));
    }
    let trimmed = out.trim_start_matches('0');
    if trimmed.is_empty() { "0".into() } else { trimmed.to_string() }
}

// ------------------------------------------------------------ SoundCloud

async fn soundcloud_url(url: &str, limit: usize) -> Result<Vec<Item>, String> {
    let id = soundcloud_client_id().await?;
    let resolve = format!("https://api-v2.soundcloud.com/resolve?url={}&client_id={id}", urlencoding::encode(url));
    let value: Value = serde_json::from_str(&get_text(&resolve).await?).map_err(|e| e.to_string())?;
    if value["kind"].as_str() == Some("playlist") {
        return soundcloud_set(&value, &id, limit).await;
    }
    Ok(vec![soundcloud_item(&value, url)])
}

fn soundcloud_item(track: &Value, fallback: &str) -> Item {
    Item {
        title: track["title"].as_str().unwrap_or("SoundCloud").to_string(),
        source: track["permalink_url"].as_str().unwrap_or(fallback).to_string(),
        query: None,
        kind: Kind::SoundCloud,
        length: track["duration"].as_u64().and_then(seconds_from_ms),
        allow_private_host: false,
        requested_by: None,
    }
}

/// SoundCloud sends full details for the first few tracks of a set and only
/// IDs for the rest, so the rest are fetched in one request.
async fn soundcloud_set(set: &Value, client_id: &str, limit: usize) -> Result<Vec<Item>, String> {
    let tracks: Vec<&Value> = set["tracks"].as_array().map(|t| t.iter().take(limit).collect()).unwrap_or_default();
    let missing: Vec<String> = tracks
        .iter()
        .filter(|t| t["permalink_url"].as_str().is_none())
        .filter_map(|t| t["id"].as_u64().map(|id| id.to_string()))
        .collect();
    let mut extra: std::collections::HashMap<u64, Value> = std::collections::HashMap::new();
    if !missing.is_empty() {
        let url = format!("https://api-v2.soundcloud.com/tracks?ids={}&client_id={client_id}", missing.join(","));
        if let Ok(Value::Array(list)) = get_text(&url).await.and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string())) {
            for track in list {
                if let Some(id) = track["id"].as_u64() {
                    extra.insert(id, track);
                }
            }
        }
    }
    let items: Vec<Item> = tracks
        .iter()
        .filter_map(|t| {
            let full = if t["permalink_url"].is_string() { Some(*t) } else { t["id"].as_u64().and_then(|id| extra.get(&id)) };
            full.map(|track| soundcloud_item(track, ""))
        })
        .filter(|item| !item.source.is_empty())
        .collect();
    if items.is_empty() {
        Err("That SoundCloud set is empty or private.".into())
    } else {
        Ok(items)
    }
}

/// Words after `sc:`, `soundcloud:` or `scsearch:`, which search SoundCloud instead of YouTube.
pub fn soundcloud_search_words(query: &str) -> Option<&str> {
    let lower = query.to_ascii_lowercase();
    ["scsearch:", "soundcloud:", "sc:"]
        .iter()
        .find(|p| lower.starts_with(*p))
        .map(|p| query[p.len()..].trim())
        .filter(|w| !w.is_empty())
}

/// SoundCloud Go+ tracks only play a 30-second preview to people without
/// the subscription, and blocked tracks don't play at all.
fn soundcloud_full(track: &Value) -> bool {
    !matches!(track["policy"].as_str(), Some("SNIP") | Some("BLOCK"))
}

async fn soundcloud_search(query: &str) -> Result<Vec<Item>, String> {
    let hits = soundcloud_hits(query, 10).await?;
    hits.into_iter().next().map(|i| vec![i]).ok_or_else(|| "No SoundCloud result that plays in full (Go+ songs only play a preview).".into())
}

/// Full-length SoundCloud tracks for a search, best first.
pub async fn soundcloud_hits(query: &str, limit: usize) -> Result<Vec<Item>, String> {
    let id = soundcloud_client_id().await?;
    let url = format!(
        "https://api-v2.soundcloud.com/search/tracks?q={}&client_id={id}&limit={}",
        urlencoding::encode(query),
        limit.clamp(1, 50)
    );
    let value: Value = serde_json::from_str(&get_text(&url).await?).map_err(|e| e.to_string())?;
    Ok(soundcloud_collection(&value))
}

fn soundcloud_collection(value: &Value) -> Vec<Item> {
    value["collection"]
        .as_array()
        .map(|list| list.iter().filter(|t| soundcloud_full(t)).map(|t| soundcloud_item(t, "")).filter(|i| !i.source.is_empty()).collect())
        .unwrap_or_default()
}

/// SoundCloud's "related tracks" for a track page, for autoplay. Falls back
/// to yt-dlp's `/recommended` list when SoundCloud's own API won't answer.
pub async fn soundcloud_related(page: &str, limit: usize) -> Result<Vec<Item>, String> {
    let api = async {
        let id = soundcloud_client_id().await?;
        let resolve = format!("https://api-v2.soundcloud.com/resolve?url={}&client_id={id}", urlencoding::encode(page));
        let track: Value = serde_json::from_str(&get_text(&resolve).await?).map_err(|e| e.to_string())?;
        let track_id = track["id"].as_u64().ok_or("SoundCloud didn't find that track")?;
        let url = format!("https://api-v2.soundcloud.com/tracks/{track_id}/related?client_id={id}&limit={}", limit.clamp(1, 50));
        let value: Value = serde_json::from_str(&get_text(&url).await?).map_err(|e| e.to_string())?;
        Ok::<_, String>(soundcloud_collection(&value))
    };
    match api.await {
        Ok(items) if !items.is_empty() => Ok(items),
        first => {
            if !ytdlp::enabled() {
                return first;
            }
            let base = page.split(['?', '#']).next().unwrap_or(page).trim_end_matches('/');
            let entries = ytdlp::playlist(&format!("{base}/recommended"), limit).await?;
            Ok(entries.into_iter().filter_map(soundcloud_entry).collect())
        }
    }
}

/// A row of yt-dlp's flat SoundCloud list. Its `url` is the track page.
fn soundcloud_entry(entry: ytdlp::Entry) -> Option<Item> {
    let page = entry.url.filter(|u| u.contains("soundcloud.com/") && !u.contains("api.soundcloud.com"))?;
    let mut item = Item::new(entry.title, page, Kind::SoundCloud);
    item.length = entry.length;
    Some(item)
}

/// Where a SoundCloud track's audio is, looked up right before it plays.
#[derive(Debug, PartialEq)]
enum ScAudio {
    /// A plain MP3 link (older tracks).
    File(String),
    /// An HLS playlist; `ext` is what the joined pieces are (`mp4` or `mp3`).
    Hls { playlist: String, ext: &'static str },
}

/// Picks the best way to fetch a track from SoundCloud's `transcodings`:
/// a plain MP3, then AAC in HLS, then MP3 in HLS. Opus-in-HLS is skipped.
fn pick_transcoding(transcodings: &[Value]) -> Option<(&str, bool, &'static str)> {
    let protocol = |t: &Value| t["format"]["protocol"].as_str().unwrap_or("").to_string();
    let mime = |t: &Value| t["format"]["mime_type"].as_str().unwrap_or("").to_ascii_lowercase();
    let pick = |want: &dyn Fn(&Value) -> bool| transcodings.iter().find(|t| want(t) && t["url"].is_string());
    if let Some(t) = pick(&|t| protocol(t) == "progressive") {
        return Some((t["url"].as_str()?, false, "mp3"));
    }
    if let Some(t) = pick(&|t| protocol(t) == "hls" && (mime(t).contains("mp4") || mime(t).contains("aac"))) {
        return Some((t["url"].as_str()?, true, "mp4"));
    }
    if let Some(t) = pick(&|t| protocol(t) == "hls" && mime(t).contains("mpeg")) {
        return Some((t["url"].as_str()?, true, "mp3"));
    }
    None
}

async fn soundcloud_audio_api(page: &str) -> Result<(ScAudio, Option<u64>), String> {
    let id = soundcloud_client_id().await?;
    let resolve = format!("https://api-v2.soundcloud.com/resolve?url={}&client_id={id}", urlencoding::encode(page));
    let track: Value = serde_json::from_str(&get_text(&resolve).await?).map_err(|e| e.to_string())?;
    if !soundcloud_full(&track) {
        return Err(SC_PREVIEW_ONLY.into());
    }
    let transcodings = track["media"]["transcodings"].as_array().ok_or("SoundCloud has no audio for this track")?;
    let (endpoint, hls, ext) = pick_transcoding(transcodings).ok_or("SoundCloud only has this track in a format Lantern can't play")?;
    let sep = if endpoint.contains('?') { '&' } else { '?' };
    let value: Value = serde_json::from_str(&get_text(&format!("{endpoint}{sep}client_id={id}")).await?).map_err(|e| e.to_string())?;
    let url = value["url"].as_str().ok_or("SoundCloud stream was empty")?.to_string();
    let length = track["duration"].as_u64().and_then(seconds_from_ms);
    Ok((if hls { ScAudio::Hls { playlist: url, ext } } else { ScAudio::File(url) }, length))
}

const SC_PREVIEW_ONLY: &str = "SoundCloud only plays a 30-second preview of this song (SoundCloud Go+), so it was skipped";
/// A SoundCloud track is downloaded whole before it plays; longer than this is refused.
const SC_MAX_BYTES: usize = 300 * 1024 * 1024;

async fn soundcloud_playable(item: &Item) -> Result<Playable, String> {
    let (audio, length, via) = match soundcloud_audio_api(&item.source).await {
        Ok((audio, length)) => (audio, length, "SoundCloud"),
        Err(err) if err == SC_PREVIEW_ONLY || !ytdlp::enabled() => return Err(err),
        Err(err) => {
            let url = ytdlp::soundcloud_audio(&item.source).await.map_err(|why| format!("{err} (yt-dlp: {why})"))?;
            let audio = if url.contains(".m3u8") || url.contains("playlist.m3u") {
                let ext = if url.contains("mp3") { "mp3" } else { "mp4" };
                ScAudio::Hls { playlist: url, ext }
            } else {
                ScAudio::File(url)
            };
            (audio, None, "yt-dlp")
        }
    };
    let base = Playable {
        url: String::new(),
        path: None,
        memory: None,
        title: item.title.clone(),
        headers: plain_headers(),
        length: length.or(item.length),
        youtube_id: None,
        size: None,
        via: Some(via),
        notes: Vec::new(),
        live_stream: false,
        hls: false,
        private_ok: false,
        adts: false,
        gain: 1.0,
    };
    match audio {
        ScAudio::File(url) => Ok(Playable { url, ..base }),
        ScAudio::Hls { playlist, ext } => {
            let bytes = hls_download(&playlist).await?;
            Ok(Playable { memory: Some((std::sync::Arc::new(bytes), ext)), ..base })
        }
    }
}

/// The pieces of an HLS playlist: the `EXT-X-MAP` header piece (if any),
/// then each media piece, as absolute links.
pub fn hls_pieces(playlist_url: &str, text: &str) -> Result<Vec<String>, String> {
    if !text.trim_start().starts_with("#EXTM3U") {
        return Err("SoundCloud's stream list wasn't readable".into());
    }
    if text.contains("#EXT-X-KEY") && !text.contains("METHOD=NONE") {
        return Err("SoundCloud encrypted this track, so Lantern can't play it".into());
    }
    let absolute = |link: &str| -> String {
        if link.starts_with("http://") || link.starts_with("https://") {
            link.to_string()
        } else {
            let base = playlist_url.split('?').next().unwrap_or(playlist_url);
            let dir = base.rsplit_once('/').map_or(base, |(d, _)| d);
            format!("{dir}/{}", link.trim_start_matches('/'))
        }
    };
    let map = Regex::new(r#"#EXT-X-MAP:.*URI="([^"]+)""#).expect("regex");
    let mut pieces = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(caps) = map.captures(line) {
            pieces.push(absolute(&caps[1]));
        } else if !line.is_empty() && !line.starts_with('#') {
            pieces.push(absolute(line));
        }
    }
    if pieces.is_empty() {
        return Err("SoundCloud's stream list was empty".into());
    }
    Ok(pieces)
}

/// Downloads every piece of an HLS track and joins them into one file.
async fn hls_download(playlist: &str) -> Result<Vec<u8>, String> {
    let text = get_text(playlist).await?;
    let mut out = Vec::new();
    for piece in hls_pieces(playlist, &text)? {
        let response = web().get(&piece).send().await.map_err(|e| describe(&e))?;
        let response = response.error_for_status().map_err(|e| describe(&e))?;
        let bytes = response.bytes().await.map_err(|e| describe(&e))?;
        out.extend_from_slice(&bytes);
        if out.len() > SC_MAX_BYTES {
            return Err("that SoundCloud track is too long to load".into());
        }
    }
    Ok(out)
}

async fn soundcloud_client_id() -> Result<String, String> {
    static CACHE: Mutex<Option<(Instant, String)>> = Mutex::new(None);
    if let Some((at, id)) = CACHE.lock().unwrap_or_else(|p| p.into_inner()).clone() {
        if at.elapsed() < Duration::from_secs(6 * 3600) {
            return Ok(id);
        }
    }
    let html = get_text("https://soundcloud.com").await?;
    let script = Regex::new(r#"https://a-v2\.sndcdn\.com/assets/[^"']+\.js"#).expect("regex");
    let id_re = Regex::new(r#"client_id:"([A-Za-z0-9]+)""#).expect("regex");
    // The id lives in the app bundle, which is usually the last script.
    let scripts: Vec<&str> = script.find_iter(&html).map(|m| m.as_str()).collect();
    for found in scripts.iter().rev().take(12) {
        if let Ok(js) = get_text(found).await {
            if let Some(caps) = id_re.captures(&js) {
                let id = caps[1].to_string();
                *CACHE.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now(), id.clone()));
                return Ok(id);
            }
        }
    }
    Err("SoundCloud client id was not found".into())
}

fn clean(value: &str) -> String {
    value.replace('\u{00a0}', " ").trim().to_string()
}

const PRIVATE_HOST_MSG: &str =
    "That link points at a private or local address on your PC. Only the owner can play LAN links.";

/// Refuses loopback, LAN, link-local, and CGNAT targets unless `allow_private` is set.
pub async fn ensure_fetch_allowed(url: &str, allow_private: bool) -> Result<(), String> {
    if allow_private {
        return Ok(());
    }
    let host = host_from_http_url(url)?;
    if host.eq_ignore_ascii_case("localhost") {
        return Err(PRIVATE_HOST_MSG.into());
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if blocked_address(ip) {
            return Err(PRIVATE_HOST_MSG.into());
        }
        return Ok(());
    }
    let addrs: Vec<_> = tokio::net::lookup_host((host.as_str(), 0)).await.map_err(|e| e.to_string())?.collect();
    if addrs.is_empty() {
        return Err(format!("Couldn't resolve {host}"));
    }
    for addr in addrs {
        if blocked_address(addr.ip()) {
            return Err(PRIVATE_HOST_MSG.into());
        }
    }
    Ok(())
}

pub(crate) fn blocked_address(ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    match ip {
        IpAddr::V4(v) => v.is_loopback() || v.is_private() || v.is_link_local() || v.is_unspecified() || cgnat(v),
        IpAddr::V6(v) => {
            v.is_loopback()
                || v.is_unspecified()
                || v.is_unicast_link_local()
                || (v.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

fn cgnat(v: Ipv4Addr) -> bool {
    let o = v.octets();
    o[0] == 100 && (o[1] & 0xc0) == 0x40
}

pub(crate) fn host_from_http_url(url: &str) -> Result<String, String> {
    let rest = url
        .trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))
        .ok_or("Only http:// and https:// links are supported")?;
    let authority = rest.split(&['/', '?', '#'][..]).next().unwrap_or(rest);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host).trim();
    if host.is_empty() {
        return Err("That link has no host".into());
    }
    Ok(host.to_string())
}

fn resolve_redirect_url(base: &str, location: &str) -> Result<String, String> {
    let location = location.trim();
    if location.starts_with("http://") || location.starts_with("https://") {
        return Ok(location.to_string());
    }
    if !location.starts_with('/') {
        return Err("redirect to an unsupported location".into());
    }
    let base_lower = base.to_ascii_lowercase();
    let scheme = if base_lower.starts_with("https://") { "https://" } else { "http://" };
    let rest = base.strip_prefix("https://").or_else(|| base.strip_prefix("http://")).unwrap_or(base);
    let authority = rest.split(&['/', '?', '#'][..]).next().unwrap_or(rest);
    Ok(format!("{scheme}{authority}{location}"))
}

async fn user_get(url: &str, allow_private: bool) -> Result<reqwest::Response, String> {
    user_request(url, allow_private, |req| req).await
}

async fn user_request(
    url: &str,
    allow_private: bool,
    build: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, String> {
    let mut current = url.to_string();
    for _ in 0..16 {
        ensure_fetch_allowed(&current, allow_private).await?;
        let response = build(user_web().get(&current))
            .send()
            .await
            .map_err(|e| describe(&e))?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or("redirect without a location")?;
            current = resolve_redirect_url(&current, location)?;
            continue;
        }
        return Ok(response);
    }
    Err("too many redirects".into())
}

async fn get_text_user(url: &str, allow_private: bool) -> Result<String, String> {
    let response = user_get(url, allow_private).await?;
    let response = response.error_for_status().map_err(|e| describe(&e))?;
    response.text().await.map_err(|e| describe(&e))
}

async fn get_text(url: &str) -> Result<String, String> {
    let response = web().get(url).send().await.map_err(|e| describe(&e))?;
    let response = response.error_for_status().map_err(|e| describe(&e))?;
    response.text().await.map_err(|e| describe(&e))
}

/// Items from saved playlists that need the owner to play.
pub fn ensure_items_fetchable(items: &[Item], allow_private_hosts: bool) -> Result<(), String> {
    for item in items {
        if matches!(item.kind, Kind::Direct | Kind::Radio) {
            if item.allow_private_host && !allow_private_hosts {
                return Err("That playlist has local links only the owner can play.".into());
            }
            let host = host_from_http_url(&item.source)?;
            if host.parse::<IpAddr>().is_ok() || host.eq_ignore_ascii_case("localhost") {
                ensure_literal_host_allowed(&host, item.allow_private_host || allow_private_hosts)?;
            }
        }
    }
    Ok(())
}

fn ensure_literal_host_allowed(host: &str, allow_private: bool) -> Result<(), String> {
    if allow_private {
        return Ok(());
    }
    if host.eq_ignore_ascii_case("localhost") {
        return Err(PRIVATE_HOST_MSG.into());
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if blocked_address(ip) {
            return Err(PRIVATE_HOST_MSG.into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod leveling_tests {
    use super::*;

    #[test]
    fn replaygain_text_and_gain() {
        assert_eq!(parse_db("-7.23 dB"), Some(-7.23));
        assert_eq!(parse_db("+2.5 dB"), Some(2.5));
        assert_eq!(parse_db("3"), Some(3.0));
        assert_eq!(parse_db("loud"), None);
        assert!((replaygain_gain(-6.0) - 0.501).abs() < 0.01);
        assert!((replaygain_gain(30.0) - 1.995).abs() < 0.01, "boost capped at +6 dB");
    }

    #[test]
    fn youtube_only_turns_loud_videos_down() {
        assert_eq!(youtube_gain(-5.0), 1.0);
        assert!((youtube_gain(6.0) - 0.501).abs() < 0.01);
        let value = serde_json::json!({ "playerConfig": { "audioConfig": { "loudnessDb": 4.2 } } });
        assert_eq!(youtube_loudness(&value), Some(4.2));
        let value = serde_json::json!({ "streamingData": { "adaptiveFormats": [ { "itag": 140, "loudnessDb": -1.5 } ] } });
        assert_eq!(youtube_loudness(&value), Some(-1.5));
        assert_eq!(youtube_loudness(&serde_json::json!({})), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn aac_stations_are_told_from_mp3_and_ogg() {
        assert!(is_adts("audio/aacp", &[]));
        assert!(is_adts("audio/AAC", &[]));
        assert!(!is_adts("audio/mpeg", &[0xff, 0xfb, 0x90, 0x64]));
        assert!(!is_adts("application/ogg", b"OggS"));
        // No type: two ADTS frames in a row (MPEG-2 id, 9 bytes each).
        let frame = [0xff, 0xf9, 0x50, 0x80, 0x01, 0x3f, 0xfc, 0, 0];
        assert!(is_adts("", &[frame, frame].concat()));
        // MP3 sync words have layer bits set.
        assert!(!is_adts("", &[0xff, 0xfb, 0x90, 0x64, 0, 0, 0, 0, 0xff, 0xfb, 0x90, 0x64]));
    }

    #[test]
    fn station_links_are_classified_from_their_first_bytes() {
        let hls = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=24000,CODECS=\"mp4a.40.2\"\nhttps://x/2/playlist.m3u8\n";
        assert_eq!(classify_radio("application/vnd.apple.mpegurl", hls.as_bytes()), RadioAnswer::Hls);
        let pls = "[playlist]\nNumberOfEntries=2\nFile1=https://ice6.somafm.com/indiepop-128-aac\nTitle1=SomaFM\nFile2=https://ice2.somafm.com/x\n";
        assert_eq!(classify_radio("audio/x-scpls", pls.as_bytes()), RadioAnswer::Playlist("https://ice6.somafm.com/indiepop-128-aac".into()));
        let m3u = "#EXTM3U\n#EXTINF:-1,Hard Rock Heaven\nhttp://hydra.cdnstream.com/1521_128\n";
        assert_eq!(classify_radio("audio/x-mpegurl", m3u.as_bytes()), RadioAnswer::Playlist("http://hydra.cdnstream.com/1521_128".into()));
        // A bare link in a text answer, with no type given.
        assert_eq!(classify_radio("", b"http://live-icy.dr.dk/A/A05H.mp3\r\n"), RadioAnswer::Playlist("http://live-icy.dr.dk/A/A05H.mp3".into()));
        assert_eq!(classify_radio("text/plain", b"http://live-icy.dr.dk/A/A05H.mp3\r\n"), RadioAnswer::Playlist("http://live-icy.dr.dk/A/A05H.mp3".into()));
        assert_eq!(classify_radio("text/html", b"<html><body>Not found</body></html>"), RadioAnswer::Nothing);
        // Audio, even when the server calls it something odd.
        assert_eq!(classify_radio("audio/mpeg", &[0xff, 0xfb, 0x90, 0x64, 0, 0, 0, 0]), RadioAnswer::Audio);
        assert_eq!(classify_radio("audio/aacp", &[0xff, 0xf1, 0x50, 0x80, 0x2e, 0x7f, 0xfc]), RadioAnswer::Audio);
        assert_eq!(classify_radio("application/ogg", b"OggS\0\x02\0\0"), RadioAnswer::Audio);
        assert!(looks_like_text(b"#EXTM3U\n"));
        assert!(!looks_like_text(b"ID3\x04\x00\x00"));
    }

    /// Plays `url` as a radio item through the same input Lantern hands Songbird,
    /// decodes `want` seconds of audio, and returns (how, codec, seconds, peak, bytes).
    async fn decode_station(url: &str, want: f64) -> Result<String, String> {
        use songbird::input::codecs::{get_codec_registry, get_probe};
        use songbird::input::LiveInput;
        use symphonia::core::audio::SampleBuffer;
        let item = Item::new("test", url, Kind::Radio);
        let started = Instant::now();
        let playable = materialize(&item).await?;
        let how = if playable.hls { "HLS" } else { "stream" };
        let resolved = playable.url.clone();
        let input = playable.input().make_playable_async(get_codec_registry(), get_probe()).await.map_err(|e| format!("couldn't open: {e}"))?;
        let songbird::input::Input::Live(LiveInput::Parsed(mut parsed), _) = input else { return Err("not parsed".into()) };
        let codec = parsed.decoder.codec_params().codec;
        let codec = symphonia::default::get_codecs().get_codec(codec).map_or_else(|| "opus".to_string(), |d| d.short_name.to_string());
        tokio::task::spawn_blocking(move || {
            let (mut seconds, mut peak, mut bytes, mut frames) = (0f64, 0f32, 0usize, 0u64);
            while seconds < want {
                if started.elapsed() > Duration::from_secs((want as u64) * 3 + 30) {
                    return Err(format!("only {seconds:.1}s decoded in {:?}", started.elapsed()));
                }
                let packet = match parsed.format.next_packet() {
                    Ok(p) => p,
                    Err(e) => return Err(format!("stream stopped after {seconds:.1}s: {e}")),
                };
                if packet.track_id() != parsed.track_id {
                    continue;
                }
                bytes += packet.data.len();
                let Ok(decoded) = parsed.decoder.decode(&packet) else { continue };
                let spec = *decoded.spec();
                frames += decoded.frames() as u64;
                seconds += decoded.frames() as f64 / f64::from(spec.rate);
                let mut samples = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                samples.copy_interleaved_ref(decoded);
                peak = samples.samples().iter().fold(peak, |m, s| m.max(s.abs()));
            }
            if peak < 0.001 {
                return Err(format!("{seconds:.1}s decoded but silent"));
            }
            Ok(format!("{how} {codec}: {seconds:.1}s audio ({frames} frames, {bytes} bytes in) in {:.1}s, peak {peak:.2} [{resolved}]", started.elapsed().as_secs_f64()))
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// What 1.10.0 did with an HLS station: the plain stream reader got the
    /// playlist text, so Songbird couldn't open it (the track failed silently).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "live internet radio"]
    async fn hls_through_the_plain_stream_reader_does_not_decode() {
        use songbird::input::codecs::{get_codec_registry, get_probe};
        let source = RangeSource::new(stream_client().clone(), "https://stream.revma.ihrhls.com/zc9483/hls.m3u8".into(), plain_headers()).with_live_stream(true);
        let input: songbird::input::Input = source.into();
        let err = input.make_playable_async(get_codec_registry(), get_probe()).await.err().expect("1.10.0's path can't open HLS");
        eprintln!("RADIO 1.10.0 path on Nu Metal Radio: {err}");
    }

    /// Real stations of each kind through the real player input (1.10.1).
    /// `LANTERN_RADIO_URLS` (space separated) replaces the list.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "plays live internet radio for about a minute"]
    async fn live_radio_stations_decode() {
        let default = [
            "https://stream.revma.ihrhls.com/zc9483/hls.m3u8",
            "https://listen.powerapp.com.tr/powerpop/128/chunks.m3u8",
            "https://rtvelivestream.rtve.es/rtvesec/rne/rne_r3_main.m3u8",
            "https://streams.radiobob.de/numetal/mp3-192/",
            "https://streams.radiobob.de/numetal/aac-64/",
            "http://79.111.14.76:8004/numetal",
            "https://somafm.com/indiepop130.pls",
            "http://www.181.fm/stream/pls/181-kickincountry.pls",
            "http://listen.cdnstream.com/hrhradio/1521_128.m3u",
            "https://icecast.walmradio.com:8443/classic",
            "https://icecast.walmradio.com:8443/classic_opus",
        ];
        let list: Vec<String> = match std::env::var("LANTERN_RADIO_URLS") {
            Ok(v) if !v.trim().is_empty() => v.split_whitespace().map(String::from).collect(),
            _ => default.iter().map(|s| s.to_string()).collect(),
        };
        let runs = list.iter().map(|url| async move {
            let result = match tokio::time::timeout(Duration::from_secs(120), decode_station(url, 22.0)).await {
                Ok(result) => result,
                Err(_) => Err("no answer in 120s".to_string()),
            };
            match &result {
                Ok(report) => eprintln!("RADIO OK   {url}: {report}"),
                Err(err) => eprintln!("RADIO FAIL {url}: {err}"),
            }
            result.is_err()
        });
        let failed = futures::future::join_all(runs).await.into_iter().filter(|f| *f).count();
        // A reader stuck in a blocking read would keep the runtime from
        // shutting down; the verdict is already printed.
        if failed > 0 {
            eprintln!("RADIO {failed} station(s) failed");
            std::process::exit(1);
        }
    }

    fn audio(url: &str, name: &str, id: &str) -> serde_json::Value {
        json!({
            "itag": 140,
            "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
            "url": url,
            "audioTrack": { "displayName": name, "id": id, "audioIsDefault": false }
        })
    }

    #[test]
    fn prefers_english_original_over_a_translation() {
        let value = json!({
            "playabilityStatus": { "status": "OK" },
            "videoDetails": { "title": "Song", "lengthSeconds": "212" },
            "streamingData": { "adaptiveFormats": [
                audio("https://example.com/es?xtags=acont%3Ddubbed-auto%3Alang%3Des", "Spanish", "es.3"),
                audio("https://example.com/en?xtags=acont%3Doriginal%3Alang%3Den", "English original", "en.4")
            ]}
        });
        let (_, url, length, _) = pick_audio(&value).expect("audio");
        assert!(url.contains("/en?"), "{url}");
        assert_eq!(length, Some(212));
    }

    #[test]
    fn prefers_original_english_over_an_english_autodub() {
        let original = audio("https://example.com/orig?xtags=acont=original:lang=en", "English original", "en.4");
        let dub = audio("https://example.com/dub?xtags=acont=dubbed-auto:lang=en-US", "English", "en-US.4");
        assert!(language_score(&original) > language_score(&dub));
    }

    #[test]
    fn single_track_without_a_language_still_plays() {
        let value = json!({
            "playabilityStatus": { "status": "OK" },
            "videoDetails": { "title": "Song" },
            "streamingData": { "adaptiveFormats": [
                { "itag": 140, "mimeType": "audio/mp4", "url": "https://example.com/only" }
            ]}
        });
        let (_, url, _, _) = pick_audio(&value).expect("audio");
        assert_eq!(url, "https://example.com/only");
    }

    #[test]
    fn refused_video_reports_the_reason() {
        let value = json!({ "playabilityStatus": { "status": "LOGIN_REQUIRED", "reason": "Sign in to confirm your age" } });
        assert_eq!(pick_audio(&value).unwrap_err(), "Sign in to confirm your age");
    }

    #[test]
    fn duration_matching_prefers_a_close_length() {
        // Top result is a 10-minute music video; the third is the album cut.
        let lengths = [Some(600), Some(250), Some(214), None];
        assert_eq!(best_match(&lengths, Some(212)), 2);
        // Nothing within 5 s: the closest within 15 s wins.
        assert_eq!(best_match(&[Some(300), Some(225), Some(200)], Some(212)), 2);
        // Nothing close at all: keep YouTube's top result.
        assert_eq!(best_match(&[Some(600), Some(30)], Some(212)), 0);
        // Unknown source length: top result.
        assert_eq!(best_match(&lengths, None), 0);
        // The first close match wins over a closer one further down.
        assert_eq!(best_match(&[Some(216), Some(212)], Some(212)), 0);
        assert_eq!(best_match(&[], Some(212)), 0);
    }

    #[test]
    fn youtube_links_are_recognised() {
        assert_eq!(youtube_id("https://youtu.be/0tdyU_gW6WE?t=3").as_deref(), Some("0tdyU_gW6WE"));
        assert_eq!(youtube_id("https://www.youtube.com/watch?list=PL1&v=0tdyU_gW6WE").as_deref(), Some("0tdyU_gW6WE"));
        assert_eq!(youtube_id("https://www.youtube.com/shorts/0tdyU_gW6WE").as_deref(), Some("0tdyU_gW6WE"));
        assert_eq!(youtube_id("https://www.youtube.com/playlist?list=PLx"), None);
        assert_eq!(
            youtube_list_id("https://www.youtube.com/playlist?list=PLrAXtmErZgOeiKm4sgNOknGvNjby9efdf").as_deref(),
            Some("PLrAXtmErZgOeiKm4sgNOknGvNjby9efdf")
        );
        assert!(is_mix("RD0tdyU_gW6WE"));
        assert!(!is_mix("PLrAXtmErZgOeiKm4sgNOknGvNjby9efdf"));
        assert_eq!(
            youtube_list_id("https://music.youtube.com/browse/VLPLMC9KNkIncKtPzgY-5rmhvj7fax8fdxoj").as_deref(),
            Some("PLMC9KNkIncKtPzgY-5rmhvj7fax8fdxoj")
        );
        assert_eq!(
            youtube_list_id("https://www.youtube.com/browse/VLOLAK5uy_m2pWyg-txDMGdNNiHtjorZVRbrVg_Ihxo").as_deref(),
            Some("OLAK5uy_m2pWyg-txDMGdNNiHtjorZVRbrVg_Ihxo")
        );
        assert_eq!(browse_id("PLMC9KNkIncKtPzgY-5rmhvj7fax8fdxoj"), "VLPLMC9KNkIncKtPzgY-5rmhvj7fax8fdxoj");
        assert_eq!(browse_id("VLPLMC9KNkIncKtPzgY-5rmhvj7fax8fdxoj"), "VLPLMC9KNkIncKtPzgY-5rmhvj7fax8fdxoj");
    }

    fn lockup(id: &str, title: &str, length: &str) -> Value {
        json!({
            "lockupViewModel": {
                "contentId": id,
                "contentType": "LOCKUP_CONTENT_TYPE_VIDEO",
                "metadata": { "lockupMetadataViewModel": { "title": { "content": title } } },
                "contentImage": {
                    "thumbnailViewModel": {
                        "overlays": [{
                            "thumbnailBottomOverlayViewModel": {
                                "badges": [{ "thumbnailBadgeViewModel": { "text": length } }]
                            }
                        }]
                    }
                }
            }
        })
    }

    #[test]
    fn playlist_page_reads_tracks_in_order_and_keeps_its_continuation() {
        let page = json!({
            "contents": {
                "twoColumnBrowseResultsRenderer": {
                    "tabs": [{
                        "tabRenderer": {
                            "content": {
                                "sectionListRenderer": {
                                    "contents": [{
                                        "itemSectionRenderer": {
                                            "contents": [
                                                lockup("aaaaaaaaaaa", "First", "3:10"),
                                                lockup("bbbbbbbbbbb", "Second", "4:01"),
                                                { "lockupViewModel": { "contentId": "PLnotavideo", "contentType": "LOCKUP_CONTENT_TYPE_PLAYLIST" } },
                                                { "continuationItemViewModel": { "continuationCommand": { "innertubeCommand": { "continuationCommand": { "token": "NEXT" } } } } }
                                            ]
                                        }
                                    }]
                                }
                            }
                        }
                    }]
                }
            },
            "related": {
                "items": [lockup("ccccccccccc", "Not in the playlist", "1:00"), lockup("ddddddddddd", "Also not", "1:00"), lockup("eeeeeeeeeee", "Nor this", "1:00")]
            }
        });
        let (items, token) = parse_browse_playlist(&page);
        assert_eq!(items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), ["First", "Second"]);
        assert_eq!(items[0].length, Some(190));
        assert_eq!(items[0].source, "https://www.youtube.com/watch?v=aaaaaaaaaaa");
        assert_eq!(token.as_deref(), Some("NEXT"));
    }

    #[test]
    fn playlist_continuation_and_legacy_rows_parse() {
        let page = json!({
            "onResponseReceivedActions": [{
                "appendContinuationItemsAction": {
                    "continuationItems": [
                        lockup("ccccccccccc", "Third", "2:00"),
                        { "playlistVideoRenderer": { "videoId": "ddddddddddd", "title": { "runs": [{ "text": "Fourth" }] }, "lengthText": { "simpleText": "1:02:03" } } },
                        { "playlistVideoRenderer": { "videoId": "eeeeeeeeeee", "title": { "simpleText": "[Deleted video]" } } }
                    ]
                }
            }]
        });
        let (items, token) = parse_browse_playlist(&page);
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].title, "Fourth");
        assert_eq!(items[1].length, Some(3723));
        assert!(token.is_none());
    }

    #[test]
    fn watch_panel_includes_a_wrapped_row_and_skips_notes() {
        let page = json!({
            "contents": {
                "twoColumnWatchNextResults": {
                    "playlist": {
                        "playlist": {
                            "contents": [
                                { "playlistPanelVideoRenderer": { "videoId": "aaaaaaaaaaa", "title": { "simpleText": "First" }, "lengthText": { "simpleText": "3:10" } } },
                                { "playlistPanelVideoWrapperRenderer": { "primaryRenderer": { "playlistPanelVideoRenderer": { "videoId": "bbbbbbbbbbb", "title": { "simpleText": "Second" }, "lengthText": { "simpleText": "4:00" } } } } },
                                { "messageRenderer": { "text": { "simpleText": "1 unavailable video is hidden" } } },
                                { "playlistPanelVideoRenderer": { "videoId": "ccccccccccc", "title": { "simpleText": "[Private video]" } } }
                            ]
                        }
                    }
                }
            }
        });
        let items = watch_panel_items(&page, 10);
        assert_eq!(items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), ["First", "Second"]);
        assert_eq!(watch_panel_items(&page, 1).len(), 1);
    }

    #[test]
    fn linked_video_is_where_the_playlist_starts() {
        let items = vec![
            youtube_item("aaaaaaaaaaa", "First".into(), Some(1)),
            youtube_item("bbbbbbbbbbb", "Second".into(), Some(2)),
            youtube_item("ccccccccccc", "Third".into(), Some(3)),
            youtube_item("ddddddddddd", "Fourth".into(), Some(4)),
        ];
        assert!(!playlist_filled(&items[..2], Some("ccccccccccc"), 2));
        assert!(playlist_filled(&items, Some("ccccccccccc"), 2));
        assert!(playlist_filled(&items, None, 4));
        assert!(!playlist_filled(&items, None, 5));
        let from_third = take_from_video(items.clone(), Some("ccccccccccc"), 2);
        assert_eq!(from_third.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), ["Third", "Fourth"]);
        let missing = take_from_video(items, Some("zzzzzzzzzzz"), 2);
        assert_eq!(missing.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), ["First", "Second"]);
    }

    #[test]
    fn saved_items_round_trip_as_json() {
        let item = search_item("Song".into(), "Artist".into(), "https://open.spotify.com/track/x", Some(200));
        let text = serde_json::to_string(&item).unwrap();
        assert!(text.contains("\"kind\":\"search\""), "{text}");
        let back: Item = serde_json::from_str(&text).unwrap();
        assert_eq!(back, item);
        assert_eq!(back.query.as_deref(), Some("Artist - Song"));
    }

    // ------------------------------------------------ YouTube routes and the backup

    fn fake_stream(url: &str) -> YouTubeStream {
        YouTubeStream { loudness_db: None, title: String::new(), url: url.into(), length: None, size: None, headers: HeaderMap::new() }
    }

    #[test]
    fn routes_run_clients_then_yt_dlp() {
        assert_eq!(routes(true, None), ["Vision", "Android VR", "iOS", "yt-dlp"]);
        assert_eq!(routes(false, None), ["Vision", "Android VR", "iOS"]);
        assert_eq!(routes(true, Some("Vision")), ["Android VR", "iOS", "yt-dlp"]);
        assert_eq!(routes(true, Some("iOS")), ["yt-dlp"]);
        assert!(routes(true, Some("yt-dlp")).is_empty());
        assert!(routes(false, Some("yt-dlp")).is_empty(), "turning the backup off must not restart the list");
        assert_eq!(routes(false, Some("iOS")), Vec::<&str>::new());
    }

    #[tokio::test]
    async fn the_first_route_that_works_wins_and_the_rest_are_not_touched() {
        let calls = Mutex::new(Vec::new());
        let (via, value, notes) = first_route(&routes(true, None), |route| {
            calls.lock().unwrap().push(route);
            async move { if route == "Android VR" { Ok(7) } else { Err(format!("{route} is broken")) } }
        })
        .await
        .unwrap();
        assert_eq!((via, value), ("Android VR", 7));
        assert_eq!(*calls.lock().unwrap(), ["Vision", "Android VR"]);
        assert_eq!(notes, ["Vision: Vision is broken"]);
    }

    #[tokio::test]
    async fn yt_dlp_is_only_asked_after_every_client_failed() {
        let calls = Mutex::new(Vec::new());
        let (via, _, notes) = first_route(&routes(true, None), |route| {
            calls.lock().unwrap().push(route);
            async move { if route == "yt-dlp" { Ok(()) } else { Err("403".to_string()) } }
        })
        .await
        .unwrap();
        assert_eq!(via, "yt-dlp");
        assert_eq!(*calls.lock().unwrap(), ["Vision", "Android VR", "iOS", "yt-dlp"]);
        assert_eq!(notes, ["Vision: 403", "Android VR: 403", "iOS: 403"]);
    }

    #[tokio::test]
    async fn with_the_backup_off_yt_dlp_is_never_asked() {
        let calls = Mutex::new(Vec::new());
        let err = first_route(&routes(false, None), |route| {
            calls.lock().unwrap().push(route);
            async { Err::<(), _>("Sign in to confirm your age".to_string()) }
        })
        .await
        .unwrap_err();
        assert_eq!(err, "Sign in to confirm your age");
        assert!(!calls.lock().unwrap().contains(&"yt-dlp"));
    }

    #[tokio::test]
    async fn when_everything_fails_both_reasons_are_reported() {
        let err = first_route(&routes(true, None), |route| async move {
            Err::<(), _>(if route == "yt-dlp" { "Video unavailable".to_string() } else { "no audio".to_string() })
        })
        .await
        .unwrap_err();
        assert_eq!(err, "no audio (yt-dlp: Video unavailable)");
        let only = first_route(&routes(true, Some("iOS")), |_| async { Err::<(), _>("Video unavailable".to_string()) })
            .await
            .unwrap_err();
        assert_eq!(only, "yt-dlp: Video unavailable");
    }

    #[test]
    fn the_log_says_which_route_played() {
        let mut playable = Playable {
            url: String::new(),
            path: None,
            memory: None,
            title: "Song".into(),
            headers: HeaderMap::new(),
            length: None,
            youtube_id: None,
            size: None,
            via: None,
            notes: Vec::new(),
            live_stream: false,
            hls: false,
            private_ok: false,
            adts: false,
            gain: 1.0,
        };
        assert_eq!(playable.via_note(), "");
        playable.via = Some("Android VR");
        assert_eq!(playable.via_note(), " via Android VR");
        playable.via = Some("yt-dlp");
        playable.notes = vec!["Vision: 403".into(), "Android VR: 410".into()];
        assert_eq!(playable.via_note(), " via yt-dlp (Vision: 403; Android VR: 410)");
    }

    /// Answers `/gone` with 410, `/forbidden` with 403, and anything else with two bytes.
    fn link_server() -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { continue };
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                let (status, body) = if path.starts_with("/gone") {
                    ("410 Gone", "")
                } else if path.starts_with("/forbidden") {
                    ("403 Forbidden", "")
                } else {
                    ("200 OK", "ab")
                };
                let _ = write!(sock, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn a_refused_link_fails_the_probe() {
        let base = link_server();
        let client = reqwest::Client::new();
        assert!(probe(&client, &fake_stream(&format!("{base}/ok"))).await.is_ok());
        let forbidden = probe(&client, &fake_stream(&format!("{base}/forbidden?x=1"))).await.unwrap_err();
        assert!(forbidden.contains("403"), "{forbidden}");
        let gone = probe(&client, &fake_stream(&format!("{base}/gone"))).await.unwrap_err();
        assert!(gone.contains("410"), "{gone}");
        // The sized form asks with ?range=, as the player does.
        let mut sized = fake_stream(&format!("{base}/forbidden?x=1"));
        sized.size = Some(1000);
        assert!(probe(&client, &sized).await.is_err());
        assert!(probe(&client, &fake_stream("http://127.0.0.1:1/nothing-listens")).await.is_err());
    }

    #[tokio::test]
    async fn a_403_on_open_moves_on_to_the_next_client_then_yt_dlp() {
        let base = link_server();
        let client = reqwest::Client::new();
        let calls = Mutex::new(Vec::new());
        // Vision's link is forbidden, Android VR's is gone, iOS has no audio, yt-dlp's link works.
        let (via, stream, notes) = first_route(&routes(true, None), |route| {
            calls.lock().unwrap().push(route);
            let client = client.clone();
            let base = base.clone();
            async move {
                let url = match route {
                    "Vision" => format!("{base}/forbidden"),
                    "Android VR" => format!("{base}/gone"),
                    "iOS" => return Err("YouTube did not return an audio stream".to_string()),
                    _ => format!("{base}/ok"),
                };
                let stream = fake_stream(&url);
                probe(&client, &stream).await?;
                Ok(stream)
            }
        })
        .await
        .unwrap();
        assert_eq!(via, "yt-dlp");
        assert!(stream.url.ends_with("/ok"));
        assert_eq!(*calls.lock().unwrap(), ["Vision", "Android VR", "iOS", "yt-dlp"]);
        assert!(notes[0].contains("403") && notes[1].contains("410"), "{notes:?}");
    }

    #[tokio::test]
    async fn a_403_on_the_first_client_with_a_working_second_never_reaches_yt_dlp() {
        let base = link_server();
        let client = reqwest::Client::new();
        let (via, _, _) = first_route(&routes(true, None), |route| {
            let (client, base) = (client.clone(), base.clone());
            async move {
                let stream = fake_stream(&format!("{base}/{}", if route == "Vision" { "forbidden" } else { "ok" }));
                probe(&client, &stream).await?;
                Ok(stream)
            }
        })
        .await
        .unwrap();
        assert_eq!(via, "Android VR");
    }

    #[tokio::test]
    async fn the_backup_answers_when_the_first_source_fails_or_is_empty() {
        let called = std::sync::atomic::AtomicUsize::new(0);
        let backup = || async {
            called.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![2])
        };
        assert_eq!(or_backup(Ok(vec![1]), true, backup).await.unwrap(), [1]);
        assert_eq!(called.load(std::sync::atomic::Ordering::SeqCst), 0, "asked although the first worked");
        assert_eq!(or_backup(Err("search broke".into()), true, backup).await.unwrap(), [2]);
        assert_eq!(or_backup(Ok(Vec::new()), true, backup).await.unwrap(), [2]);
        assert_eq!(called.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(or_backup(Err("search broke".into()), false, backup).await.unwrap_err(), "search broke");
        assert_eq!(called.load(std::sync::atomic::Ordering::SeqCst), 2, "asked although switched off");
        let both = or_backup::<i32, _, _>(Err("search broke".into()), true, || async { Err("no binary".to_string()) }).await;
        assert_eq!(both.unwrap_err(), "search broke (yt-dlp: no binary)");
        let empty = or_backup::<i32, _, _>(Err("search broke".into()), true, || async { Ok(Vec::new()) }).await;
        assert_eq!(empty.unwrap_err(), "search broke");
    }

    #[test]
    fn yt_dlp_rows_become_queue_items() {
        let entry = |id: &str| ytdlp::Entry { id: id.into(), title: "Bustin".into(), length: Some(212), channel: "A".into(), url: None };
        let item = entry_item(entry("0tdyU_gW6WE")).unwrap();
        assert_eq!(item.source, "https://www.youtube.com/watch?v=0tdyU_gW6WE");
        assert_eq!((item.kind, item.length), (Kind::YouTube, Some(212)));
        assert!(entry_item(entry("UCxxxxxxxxxxxxxxxxxxxxxx")).is_none(), "a channel is not a song");
    }

    #[tokio::test]
    async fn a_song_with_no_other_route_reports_that_instead_of_looping() {
        let mut playable = Playable {
            url: String::new(),
            path: None,
            memory: None,
            title: "Song".into(),
            headers: HeaderMap::new(),
            length: None,
            youtube_id: Some("0tdyU_gW6WE".into()),
            size: None,
            via: None,
            notes: Vec::new(),
            live_stream: false,
            hls: false,
            private_ok: false,
            adts: false,
            gain: 1.0,
        };
        assert!(retry_youtube(&playable).await.is_err(), "not a YouTube-by-route song");
        playable.via = Some("yt-dlp");
        // yt-dlp is last, so nothing is left whether or not the setting is on.
        assert!(retry_youtube(&playable).await.err().unwrap().contains("no other way"));
    }

    /// Needs the internet and yt-dlp. The backup route finds a link YouTube serves.
    #[tokio::test]
    #[ignore]
    async fn the_yt_dlp_route_opens_a_real_stream() {
        let stream = try_route(BACKUP, "0tdyU_gW6WE").await.expect("yt-dlp route");
        eprintln!("{:?}s, {:?} bytes", stream.length, stream.size);
    }

    /// Needs the internet. Run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn every_youtube_client_resolves_audio() {
        for client in CLIENTS {
            let stream = youtube_stream_with(client, "0tdyU_gW6WE").await;
            match stream {
                Ok(s) => eprintln!("{}: ok, {:?}s, {}", client.name, s.length, &s.url[..60.min(s.url.len())]),
                Err(e) => eprintln!("{}: FAILED {e}", client.name),
            }
        }
    }

    /// Needs the internet. Run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn youtube_playlist_and_mix_import() {
        let list = youtube_link("https://www.youtube.com/playlist?list=PLMC9KNkIncKtPzgY-5rmhvj7fax8fdxoj", 5).await.expect("playlist");
        eprintln!("playlist: {:?}", list.iter().map(|i| (&i.title, i.length)).collect::<Vec<_>>());
        assert_eq!(list.len(), 5);
        // Past the watch panel's ~200 cap, so this only passes if the playlist page is followed.
        let long = youtube_list("PLw-VjHDlEOgvIaXEz6xErW8uo2gqg5xmx", None, 210).await.expect("long playlist");
        assert!(long.len() > 200, "playlist page did not continue: {}", long.len());
        let late = youtube_id(&long[205].source).expect("late video");
        let from = youtube_list("PLw-VjHDlEOgvIaXEz6xErW8uo2gqg5xmx", Some(&late), 5).await.expect("from late");
        assert_eq!(youtube_id(&from[0].source).as_deref(), Some(late.as_str()));
        assert_eq!(from.len(), 5);
        let mix = youtube_list("RD0tdyU_gW6WE", Some("0tdyU_gW6WE"), 10).await.expect("mix");
        eprintln!("mix: {:?}", mix.iter().map(|i| &i.title).collect::<Vec<_>>());
        assert!(mix.len() >= 5);
        // A song shared from a mix plays just that song.
        let one = youtube_link("https://www.youtube.com/watch?v=0tdyU_gW6WE&list=RD0tdyU_gW6WE", 5).await.expect("one");
        assert_eq!(one.len(), 1);
    }

    /// Needs the internet. Run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn spotify_and_soundcloud_resolve_and_play() {
        let spotify = resolve("https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT", 5, false).await.expect("spotify");
        eprintln!("spotify: {:?}", spotify[0]);
        assert_eq!(spotify[0].kind, Kind::Search);
        let playable = materialize(&spotify[0]).await.expect("spotify plays");
        eprintln!("spotify -> {} ({:?}s)", playable.title, playable.length);
        match resolve("https://soundcloud.com/forss/flickermood", 5, false).await {
            Ok(sc) => {
                eprintln!("soundcloud: {:?}", sc[0]);
                assert_eq!(sc[0].kind, Kind::SoundCloud);
                match materialize(&sc[0]).await {
                    Ok(p) => eprintln!("soundcloud plays: {}", &p.url[..50.min(p.url.len())]),
                    Err(e) => eprintln!("soundcloud refused (expected for HLS-only): {e}"),
                }
            }
            Err(e) => eprintln!("soundcloud lookup failed: {e}"),
        }
    }

    /// Needs the internet. Run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn resolves_bustin_audio_url() {
        let stream = youtube_stream("0tdyU_gW6WE", None).await.expect("resolve").stream;
        assert!(stream.title.to_lowercase().contains("bustin"), "{}", stream.title);
        assert!(stream.url.starts_with("https://"), "{}", stream.url);
    }

    #[test]
    fn private_ip_addresses_are_blocked() {
        assert!(blocked_address("127.0.0.1".parse().unwrap()));
        assert!(blocked_address("10.0.0.1".parse().unwrap()));
        assert!(blocked_address("172.16.0.1".parse().unwrap()));
        assert!(blocked_address("192.168.0.1".parse().unwrap()));
        assert!(blocked_address("100.64.0.1".parse().unwrap()));
        assert!(blocked_address("::1".parse().unwrap()));
        assert!(!blocked_address("8.8.8.8".parse().unwrap()));
    }

    #[tokio::test]
    async fn literal_private_http_urls_are_refused() {
        assert!(ensure_fetch_allowed("http://192.168.0.5/song.mp3", false).await.is_err());
        assert!(ensure_fetch_allowed("http://192.168.0.5/song.mp3", true).await.is_ok());
    }

    #[test]
    fn apple_itunes_links_match_the_same_regex_as_music_apple_com() {
        let re = Regex::new(r"(?:music|itunes)\.apple\.com/(?:[a-z]{2}/)?(song|album|playlist)/[^/]+/([^/?#]+)").unwrap();
        let url = "https://itunes.apple.com/us/album/example/id1234567890";
        let caps = re.captures(url).expect("itunes album");
        assert_eq!(&caps[1], "album");
        assert_eq!(&caps[2], "id1234567890");
        let song = "https://music.apple.com/gb/song/example/1234567890";
        assert!(re.captures(song).is_some());
    }

    #[test]
    fn redirect_urls_to_private_hosts_are_rejected() {
        let next = resolve_redirect_url("http://example.com/a", "http://127.0.0.1/x").unwrap();
        let host = host_from_http_url(&next).unwrap();
        assert!(host.parse::<IpAddr>().is_ok());
        assert!(blocked_address(host.parse().unwrap()));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lantern-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"x").unwrap();
    }

    #[test]
    fn local_tracks_use_the_file_name_and_hide_the_folder() {
        let root = scratch("names");
        let song = root.join("Secret Folder").join("Song Title.mp3");
        touch(&song);
        touch(&root.join("Secret Folder").join("notes.txt"));
        touch(&root.join("Secret Folder").join("Album").join("02 Deep Cut.flac"));
        touch(&root.join("Secret Folder").join("Album").join("10 Later.FLAC"));
        touch(&root.join("Secret Folder").join("Album").join("2 Early.wav"));
        let (items, truncated) = collect_local_audio(&[root.join("Secret Folder")]);
        assert!(!truncated);
        let titles: Vec<_> = items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, ["02 Deep Cut", "2 Early", "10 Later", "Song Title"]);
        assert!(items.iter().all(|i| i.kind == Kind::File && i.link().is_none()));
        assert!(items.iter().all(|i| !i.title.contains("Secret") && !i.title.contains('\\') && !i.title.contains('/')));
        assert!(items.iter().all(|i| i.source.contains("Secret Folder")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_picked_file_is_one_track_and_hidden_names_inside_folders_are_skipped() {
        let root = scratch("one");
        let song = root.join("Just One.ogg");
        touch(&song);
        touch(&root.join(".hidden").join("Nope.mp3"));
        let (items, _) = collect_local_audio(std::slice::from_ref(&song));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Just One");
        let (from_dir, _) = collect_local_audio(std::slice::from_ref(&root));
        assert_eq!(from_dir.len(), 1);
        assert_eq!(from_dir[0].title, "Just One");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn natural_order_puts_2_before_10() {
        assert_eq!(natural_cmp("b/2 Song.mp3", "b/10 Song.mp3"), Ordering::Less);
        assert_eq!(natural_cmp("B/10.mp3", "b/10.mp3"), Ordering::Equal);
    }

    #[tokio::test]
    async fn a_missing_local_file_does_not_name_the_path() {
        let item = Item {
            title: "Song Title".into(),
            source: scratch("gone").join("Secret").join("Song Title.mp3").to_string_lossy().into_owned(),
            query: None,
            kind: Kind::File,
            length: None,
            allow_private_host: false,
            requested_by: None,
        };
        let err = match materialize(&item).await {
            Err(err) => err,
            Ok(_) => panic!("a missing file should not open"),
        };
        assert_eq!(err, "that file isn't there anymore");
        assert!(!err.contains("Secret"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_local_wav_opens_under_its_file_name() {
        let root = scratch("wav");
        let path = root.join("Private").join("Song Title.wav");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, tiny_wav()).unwrap();
        let (items, _) = collect_local_audio(std::slice::from_ref(&root));
        assert_eq!(items[0].title, "Song Title");
        let playable = materialize(&items[0]).await.expect("file");
        assert_eq!(playable.title, "Song Title");
        assert!(playable.path.as_deref().is_some_and(|p| p.contains("Private")));
        let live = playable.input().make_playable_async(songbird::input::codecs::get_codec_registry(), songbird::input::codecs::get_probe()).await;
        if let Err(err) = live {
            panic!("open failed: {err}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    fn tiny_wav() -> Vec<u8> {
        let rate = 8000u32;
        let samples = 800u32;
        let data_len = samples * 2;
        let mut out = Vec::new();
        out.extend(b"RIFF");
        out.extend((36 + data_len).to_le_bytes());
        out.extend(b"WAVEfmt ");
        out.extend(16u32.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(rate.to_le_bytes());
        out.extend((rate * 2).to_le_bytes());
        out.extend(2u16.to_le_bytes());
        out.extend(16u16.to_le_bytes());
        out.extend(b"data");
        out.extend(data_len.to_le_bytes());
        out.extend(std::iter::repeat_n(0u8, data_len as usize));
        out
    }

    #[test]
    fn soundcloud_search_prefixes() {
        assert_eq!(soundcloud_search_words("sc: daft punk"), Some("daft punk"));
        assert_eq!(soundcloud_search_words("SC:daft punk"), Some("daft punk"));
        assert_eq!(soundcloud_search_words("soundcloud: forss"), Some("forss"));
        assert_eq!(soundcloud_search_words("scsearch:forss"), Some("forss"));
        assert_eq!(soundcloud_search_words("sc:"), None);
        assert_eq!(soundcloud_search_words("scary monsters"), None);
        assert_eq!(soundcloud_search_words("https://soundcloud.com/a/b"), None);
    }

    #[test]
    fn soundcloud_skips_go_plus_previews() {
        assert!(soundcloud_full(&serde_json::json!({ "policy": "ALLOW" })));
        assert!(soundcloud_full(&serde_json::json!({ "policy": "MONETIZE" })));
        assert!(soundcloud_full(&serde_json::json!({})));
        assert!(!soundcloud_full(&serde_json::json!({ "policy": "SNIP" })));
        assert!(!soundcloud_full(&serde_json::json!({ "policy": "BLOCK" })));
        let page = serde_json::json!({ "collection": [
            { "title": "Preview", "permalink_url": "https://soundcloud.com/a/p", "policy": "SNIP", "duration": 30000 },
            { "title": "Full", "permalink_url": "https://soundcloud.com/a/f", "policy": "ALLOW", "duration": 200000 },
            { "title": "No link", "policy": "ALLOW" }
        ]});
        let items = soundcloud_collection(&page);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Full");
        assert_eq!(items[0].length, Some(200));
        assert_eq!(items[0].kind, Kind::SoundCloud);
    }

    #[test]
    fn soundcloud_prefers_mp3_then_aac_hls_and_never_opus() {
        let t = |protocol: &str, mime: &str, url: &str| serde_json::json!({ "url": url, "format": { "protocol": protocol, "mime_type": mime } });
        let all = [
            t("hls", "audio/ogg; codecs=\"opus\"", "o"),
            t("hls", "audio/mpeg", "m"),
            t("hls", "audio/mp4; codecs=\"mp4a.40.2\"", "a"),
            t("progressive", "audio/mpeg", "p"),
        ];
        assert_eq!(pick_transcoding(&all), Some(("p", false, "mp3")));
        assert_eq!(pick_transcoding(&all[..3]), Some(("a", true, "mp4")));
        assert_eq!(pick_transcoding(&all[..2]), Some(("m", true, "mp3")));
        assert_eq!(pick_transcoding(&all[..1]), None);
        let ctr = [t("ctr-encrypted-hls", "audio/mp4", "x"), t("hls", "audio/mp4; codecs=\"mp4a.40.2\"", "a")];
        assert_eq!(pick_transcoding(&ctr), Some(("a", true, "mp4")));
    }

    #[test]
    fn hls_pieces_reads_map_and_segments() {
        let list = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-MAP:URI=\"https://cdn.example/t/init.mp4?sig=1\"\n#EXTINF:10.0,\nhttps://cdn.example/t/data000.m4s?sig=1\n#EXTINF:10.0,\ndata001.m4s?sig=1\n#EXT-X-ENDLIST\n";
        let pieces = hls_pieces("https://cdn.example/t/playlist.m3u8?sig=1", list).unwrap();
        assert_eq!(
            pieces,
            vec![
                "https://cdn.example/t/init.mp4?sig=1".to_string(),
                "https://cdn.example/t/data000.m4s?sig=1".to_string(),
                "https://cdn.example/t/data001.m4s?sig=1".to_string(),
            ]
        );
        assert!(hls_pieces("https://x/p.m3u8", "<html>").is_err());
        assert!(hls_pieces("https://x/p.m3u8", "#EXTM3U\n#EXT-X-ENDLIST\n").is_err());
        assert!(hls_pieces("https://x/p.m3u8", "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\na.ts\n").is_err());
    }

    #[test]
    fn soundcloud_rows_from_ytdlp_keep_the_page() {
        let row = |url: Option<&str>| ytdlp::Entry { id: "1".into(), title: "NEVA".into(), length: Some(90), channel: String::new(), url: url.map(str::to_string) };
        let item = soundcloud_entry(row(Some("https://soundcloud.com/amon-tobin/neva"))).unwrap();
        assert_eq!(item.source, "https://soundcloud.com/amon-tobin/neva");
        assert_eq!(item.kind, Kind::SoundCloud);
        assert_eq!(item.length, Some(90));
        assert!(soundcloud_entry(row(Some("https://api.soundcloud.com/tracks/soundcloud%3Atracks%3A1"))).is_none());
        assert!(soundcloud_entry(row(None)).is_none());
    }

    #[tokio::test]
    #[ignore = "network"]
    async fn soundcloud_hls_track_downloads_whole() {
        let items = resolve("https://soundcloud.com/forss/flickermood", 5, false).await.unwrap();
        let playable = materialize(&items[0]).await.unwrap();
        let (bytes, ext) = playable.memory.clone().expect("HLS in memory, or a plain link");
        eprintln!("{} bytes ({ext}) via {:?}", bytes.len(), playable.via);
        assert!(bytes.len() > 1_000_000);
        let related = soundcloud_related("https://soundcloud.com/forss/flickermood", 10).await.unwrap();
        assert!(!related.is_empty());
        assert!(related.iter().all(|i| i.kind == Kind::SoundCloud));
        assert!(!soundcloud_hits("forss flickermood", 5).await.unwrap().is_empty());
    }
}
