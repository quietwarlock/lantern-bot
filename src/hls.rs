//! Live HLS radio (1.10.1).
//!
//! Many stations in the Radio Browser directory (iHeart, BBC, RTVE, ...) only
//! offer HLS: a `.m3u8` playlist of short audio pieces that is reloaded every
//! few seconds. Symphonia can't read a playlist, so this follows it: it picks
//! a variant from a master playlist, downloads each new piece in order, turns
//! it into one continuous audio stream (packed AAC/MP3 pieces lose their ID3
//! tags; MPEG-TS pieces are unpacked to their AAC or MP3 audio), and hands that
//! to the player as a live, non-seekable source.

use reqwest::Url;
use serenity::async_trait;
use songbird::input::{AudioStream, AudioStreamError, Compose, Input};
use std::collections::{HashSet, VecDeque};
use std::io::{Error as IoError, ErrorKind, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use symphonia::core::io::MediaSource;

/// Pieces taken from the live edge when a stream starts.
const START_PIECES: usize = 3;
/// The player gives up on a stream that sends nothing for this long.
const STALL: Duration = Duration::from_secs(40);
/// Playlist reloads that may fail in a row before the stream counts as dropped.
const RELOAD_FAILURES: u32 = 5;
/// Audio buffered ahead of the player at most.
const MAX_BUFFER: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub url: String,
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Playlist {
    /// Variants: (bandwidth, url, codecs).
    Master(Vec<(u64, String, String)>),
    Media {
        target: u64,
        segments: Vec<Segment>,
        map: Option<String>,
        encrypted: bool,
        ended: bool,
    },
}

fn attr<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let at = line.find(&format!("{name}="))? + name.len() + 1;
    let rest = &line[at..];
    if let Some(quoted) = rest.strip_prefix('"') {
        quoted.split('"').next()
    } else {
        rest.split(',').next()
    }
}

/// Reads a master or media playlist. Relative links are taken from `base`.
pub fn parse_playlist(base: &str, text: &str) -> Result<Playlist, String> {
    let base = Url::parse(base).map_err(|e| e.to_string())?;
    let join = |link: &str| base.join(link.trim()).map(|u| u.to_string()).map_err(|e| e.to_string());
    if !text.trim_start().starts_with("#EXTM3U") {
        return Err("that isn't an HLS playlist".into());
    }
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.iter().any(|l| l.starts_with("#EXT-X-STREAM-INF")) {
        let mut variants = Vec::new();
        let mut pending: Option<(u64, String)> = None;
        for line in &lines {
            if line.starts_with("#EXT-X-STREAM-INF") {
                let bandwidth = attr(line, "BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0);
                let codecs = attr(line, "CODECS").unwrap_or("").to_string();
                pending = Some((bandwidth, codecs));
            } else if !line.starts_with('#') {
                if let Some((bandwidth, codecs)) = pending.take() {
                    variants.push((bandwidth, join(line)?, codecs));
                }
            }
        }
        if variants.is_empty() {
            return Err("the HLS playlist lists no streams".into());
        }
        return Ok(Playlist::Master(variants));
    }
    let mut target = 6;
    let mut seq = 0u64;
    let mut segments = Vec::new();
    let mut map = None;
    let mut encrypted = false;
    let mut ended = false;
    for line in &lines {
        if let Some(v) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            target = v.trim().parse::<f64>().map(|t| t.ceil() as u64).unwrap_or(6).clamp(1, 60);
        } else if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = v.trim().parse().unwrap_or(0);
        } else if line.starts_with("#EXT-X-KEY") {
            encrypted = attr(line, "METHOD").is_some_and(|m| !m.eq_ignore_ascii_case("NONE"));
        } else if line.starts_with("#EXT-X-MAP") {
            if let Some(uri) = attr(line, "URI") {
                map = Some(join(uri)?);
            }
        } else if line.starts_with("#EXT-X-ENDLIST") {
            ended = true;
        } else if !line.starts_with('#') {
            segments.push(Segment { url: join(line)?, seq });
            seq += 1;
        }
    }
    if segments.is_empty() {
        return Err("the HLS playlist has no audio pieces".into());
    }
    Ok(Playlist::Media { target, segments, map, encrypted, ended })
}

/// The variant to play: audio-only ones first, then the highest bandwidth up to 320 kbps, else the lowest.
pub fn pick_variant(variants: &[(u64, String, String)]) -> Option<&str> {
    let audio_only = |c: &str| !c.is_empty() && !c.contains("avc") && !c.contains("hvc") && !c.contains("hev");
    let pool: Vec<&(u64, String, String)> = if variants.iter().any(|v| audio_only(&v.2)) {
        variants.iter().filter(|v| audio_only(&v.2)).collect()
    } else {
        variants.iter().collect()
    };
    pool.iter()
        .filter(|v| v.0 <= 320_000)
        .max_by_key(|v| v.0)
        .or_else(|| pool.iter().min_by_key(|v| v.0))
        .map(|v| v.1.as_str())
}

/// Skips ID3v2 tags at the start of a piece (packed audio HLS puts a timestamp tag on each).
pub fn strip_id3(mut bytes: &[u8]) -> &[u8] {
    while bytes.len() >= 10 && &bytes[..3] == b"ID3" {
        let size = bytes[6..10].iter().fold(0usize, |n, b| (n << 7) | usize::from(b & 0x7f));
        let footer = if bytes[5] & 0x10 != 0 { 10 } else { 0 };
        let end = (10 + size + footer).min(bytes.len());
        bytes = &bytes[end..];
    }
    bytes
}

/// What the pieces hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pieces {
    Aac,
    Mp3,
    Ts,
    /// Fragmented MP4 (`EXT-X-MAP`).
    Mp4,
}

pub fn sniff(bytes: &[u8]) -> Option<Pieces> {
    if bytes.len() > 188 && bytes[0] == 0x47 && bytes[188] == 0x47 {
        return Some(Pieces::Ts);
    }
    let audio = strip_id3(bytes);
    match audio {
        [0xff, b, ..] if b & 0xf6 == 0xf0 => Some(Pieces::Aac),
        [0xff, b, ..] if b & 0xe0 == 0xe0 => Some(Pieces::Mp3),
        _ => None,
    }
}

/// Makes AAC ADTS frames readable by the decoder, which only syncs on
/// `FFF1` (MPEG-4, no CRC): MPEG-2 frames get the MPEG-4 id and CRCs are dropped.
#[derive(Default)]
pub struct AdtsFix {
    carry: Vec<u8>,
}

impl AdtsFix {
    pub fn push(&mut self, data: &[u8]) -> Vec<u8> {
        self.carry.extend_from_slice(data);
        let buf = &self.carry;
        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        while buf.len() - i >= 7 {
            if !(buf[i] == 0xff && buf[i + 1] & 0xf6 == 0xf0) {
                i += 1;
                continue;
            }
            let crc = buf[i + 1] & 1 == 0;
            if crc && buf.len() - i < 9 {
                break;
            }
            let len = (usize::from(buf[i + 3] & 3) << 11) | (usize::from(buf[i + 4]) << 3) | usize::from(buf[i + 5] >> 5);
            let head = if crc { 9 } else { 7 };
            if len <= head {
                i += 1;
                continue;
            }
            if buf.len() - i < len {
                break;
            }
            // Joined mid-frame (plain streams): a real frame is followed by another.
            if buf.len() - i > len + 1 && !(buf[i + len] == 0xff && buf[i + len + 1] & 0xf6 == 0xf0) {
                i += 1;
                continue;
            }
            let new_len = len - (head - 7);
            let mut header = [buf[i], 0xf1, buf[i + 2], buf[i + 3], buf[i + 4], buf[i + 5], buf[i + 6]];
            header[3] = (header[3] & 0xfc) | ((new_len >> 11) as u8 & 3);
            header[4] = (new_len >> 3) as u8;
            header[5] = (header[5] & 0x1f) | (((new_len & 7) as u8) << 5);
            out.extend_from_slice(&header);
            out.extend_from_slice(&buf[i + head..i + len]);
            i += len;
        }
        self.carry.drain(..i);
        if self.carry.len() > 1 << 20 {
            self.carry.clear();
        }
        out
    }
}

/// A plain AAC radio stream with its ADTS headers rewritten by [`AdtsFix`]:
/// Symphonia only finds MPEG-4 frames without a checksum, and scans a live
/// stream of any other kind forever (1.10.1).
pub struct AdtsSource {
    inner: crate::stream::RangeSource,
}

impl AdtsSource {
    pub fn new(inner: crate::stream::RangeSource) -> Self {
        AdtsSource { inner }
    }
}

impl From<AdtsSource> for Input {
    fn from(source: AdtsSource) -> Self {
        Input::Lazy(Box::new(source))
    }
}

#[async_trait]
impl Compose for AdtsSource {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Err(AudioStreamError::Unsupported)
    }

    async fn create_async(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        let stream = self.inner.create_async().await?;
        Ok(AudioStream { input: Box::new(AdtsReader { inner: stream.input, fix: AdtsFix::default(), out: VecDeque::new(), pos: 0 }) })
    }

    fn should_create_async(&self) -> bool {
        true
    }
}

struct AdtsReader {
    inner: Box<dyn MediaSource>,
    fix: AdtsFix,
    out: VecDeque<u8>,
    pos: u64,
}

impl Read for AdtsReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut chunk = [0u8; 16 * 1024];
        while self.out.is_empty() {
            let n = self.inner.read(&mut chunk)?;
            if n == 0 {
                return Ok(0);
            }
            self.out.extend(self.fix.push(&chunk[..n]));
        }
        let n = out.len().min(self.out.len());
        for (slot, byte) in out.iter_mut().zip(self.out.drain(..n)) {
            *slot = byte;
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for AdtsReader {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        match to {
            SeekFrom::Current(0) => Ok(self.pos),
            SeekFrom::Start(p) if p == self.pos => Ok(self.pos),
            _ => Err(IoError::new(ErrorKind::Unsupported, "live radio can't seek")),
        }
    }
}

impl MediaSource for AdtsReader {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

/// Pulls the first AAC or MP3 audio track out of MPEG-TS packets.
#[derive(Default)]
pub struct TsDemux {
    carry: Vec<u8>,
    pmt: Option<u16>,
    audio: Option<(u16, Pieces)>,
    started: bool,
}

impl TsDemux {
    pub fn push(&mut self, data: &[u8]) -> Result<Vec<u8>, String> {
        self.carry.extend_from_slice(data);
        let mut out = Vec::new();
        let mut i = 0;
        while self.carry.len() - i >= 188 {
            if self.carry[i] != 0x47 {
                i += 1;
                continue;
            }
            let p = &self.carry[i..i + 188];
            i += 188;
            let pid = (u16::from(p[1] & 0x1f) << 8) | u16::from(p[2]);
            let start = p[1] & 0x40 != 0;
            let afc = (p[3] >> 4) & 3;
            if afc & 1 == 0 {
                continue;
            }
            let mut at = 4;
            if afc & 2 != 0 {
                at += 1 + usize::from(p[4]);
            }
            if at >= 188 {
                continue;
            }
            let payload = &p[at..];
            if pid == 0 && start {
                self.pmt = pat_pmt_pid(payload).or(self.pmt);
            } else if Some(pid) == self.pmt && start && self.audio.is_none() {
                self.audio = Some(pmt_audio(payload)?);
            } else if let Some((audio, _)) = self.audio {
                if pid != audio {
                    continue;
                }
                if start {
                    if payload.len() < 9 || payload[..3] != [0, 0, 1] {
                        continue;
                    }
                    let skip = 9 + usize::from(payload[8]);
                    if skip <= payload.len() {
                        out.extend_from_slice(&payload[skip..]);
                    }
                    self.started = true;
                } else if self.started {
                    out.extend_from_slice(payload);
                }
            }
        }
        self.carry.drain(..i);
        Ok(out)
    }

    pub fn kind(&self) -> Option<Pieces> {
        self.audio.map(|a| a.1)
    }
}

fn section(payload: &[u8]) -> Option<&[u8]> {
    let pointer = usize::from(*payload.first()?);
    payload.get(1 + pointer..)
}

fn pat_pmt_pid(payload: &[u8]) -> Option<u16> {
    let t = section(payload)?;
    if *t.first()? != 0 || t.len() < 8 {
        return None;
    }
    let len = (usize::from(t[1] & 0x0f) << 8) | usize::from(t[2]);
    let end = (3 + len).saturating_sub(4).min(t.len());
    let mut i = 8;
    while i + 4 <= end {
        let program = (u16::from(t[i]) << 8) | u16::from(t[i + 1]);
        let pid = (u16::from(t[i + 2] & 0x1f) << 8) | u16::from(t[i + 3]);
        if program != 0 {
            return Some(pid);
        }
        i += 4;
    }
    None
}

fn pmt_audio(payload: &[u8]) -> Result<(u16, Pieces), String> {
    let t = section(payload).ok_or("bad MPEG-TS program table")?;
    if t.first() != Some(&2) || t.len() < 12 {
        return Err("bad MPEG-TS program table".into());
    }
    let len = (usize::from(t[1] & 0x0f) << 8) | usize::from(t[2]);
    let end = (3 + len).saturating_sub(4).min(t.len());
    let info = (usize::from(t[10] & 0x0f) << 8) | usize::from(t[11]);
    let mut i = 12 + info;
    let mut seen = Vec::new();
    while i + 5 <= end {
        let kind = t[i];
        let pid = (u16::from(t[i + 1] & 0x1f) << 8) | u16::from(t[i + 2]);
        let es = (usize::from(t[i + 3] & 0x0f) << 8) | usize::from(t[i + 4]);
        match kind {
            0x0f => return Ok((pid, Pieces::Aac)),
            0x03 | 0x04 => return Ok((pid, Pieces::Mp3)),
            other => seen.push(other),
        }
        i += 5 + es;
    }
    Err(format!("the stream's audio is in a format Lantern can't play (MPEG-TS types {seen:02x?})"))
}

/// Turns pieces into one audio stream.
pub struct Joiner {
    kind: Option<Pieces>,
    ts: TsDemux,
    adts: AdtsFix,
}

impl Joiner {
    pub fn new() -> Self {
        Self { kind: None, ts: TsDemux::default(), adts: AdtsFix::default() }
    }

    pub fn push(&mut self, piece: &[u8]) -> Result<Vec<u8>, String> {
        if self.kind.is_none() {
            self.kind = Some(sniff(piece).ok_or("the stream's pieces aren't AAC, MP3 or MPEG-TS")?);
        }
        Ok(match self.kind {
            Some(Pieces::Ts) => {
                let audio = self.ts.push(piece)?;
                match self.ts.kind() {
                    Some(Pieces::Aac) => self.adts.push(&audio),
                    _ => audio,
                }
            }
            Some(Pieces::Aac) => self.adts.push(strip_id3(piece)),
            _ => strip_id3(piece).to_vec(),
        })
    }
}

impl Default for Joiner {
    fn default() -> Self {
        Self::new()
    }
}

// ------------------------------------------------------------ the live feed

#[derive(Default)]
struct FeedState {
    buf: VecDeque<u8>,
    eof: bool,
    error: Option<String>,
    closed: bool,
}

#[derive(Default)]
struct Feed {
    state: Mutex<FeedState>,
    data: Condvar,
}

impl Feed {
    fn lock(&self) -> MutexGuard<'_, FeedState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

struct FeedReader {
    feed: Arc<Feed>,
    pos: u64,
}

impl Read for FeedReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let deadline = Instant::now() + STALL;
        let mut state = self.feed.lock();
        loop {
            if !state.buf.is_empty() {
                let n = out.len().min(state.buf.len());
                for (slot, byte) in out.iter_mut().zip(state.buf.drain(..n)) {
                    *slot = byte;
                }
                self.pos += n as u64;
                return Ok(n);
            }
            if let Some(err) = &state.error {
                return Err(IoError::other(err.clone()));
            }
            if state.eof {
                return Ok(0);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(IoError::new(ErrorKind::TimedOut, "the station stopped sending audio"));
            }
            state = self.feed.data.wait_timeout(state, left).unwrap_or_else(|p| p.into_inner()).0;
        }
    }
}

impl Seek for FeedReader {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        match to {
            SeekFrom::Current(0) => Ok(self.pos),
            SeekFrom::Start(p) if p == self.pos => Ok(self.pos),
            _ => Err(IoError::new(ErrorKind::Unsupported, "live radio can't seek")),
        }
    }
}

impl MediaSource for FeedReader {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

impl Drop for FeedReader {
    fn drop(&mut self) {
        self.feed.lock().closed = true;
    }
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(crate::media::WEB_UA)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_default()
    })
}

/// Fetches `url` after checking its host (once per host), following redirects.
/// Returns the final address and the body.
async fn fetch(url: &str, allow_private: bool, checked: &mut HashSet<String>) -> Result<(String, Vec<u8>), String> {
    let host = crate::media::host_from_http_url(url)?;
    if !checked.contains(&host) {
        crate::media::ensure_fetch_allowed(url, allow_private).await?;
        checked.insert(host);
    }
    let response = client().get(url).send().await.map_err(|e| e.to_string())?;
    let response = response.error_for_status().map_err(|e| e.to_string())?;
    let last = response.url().to_string();
    if last != url {
        let host = crate::media::host_from_http_url(&last)?;
        if !checked.contains(&host) {
            crate::media::ensure_fetch_allowed(&last, allow_private).await?;
            checked.insert(host);
        }
    }
    Ok((last, response.bytes().await.map_err(|e| e.to_string())?.to_vec()))
}

struct Media {
    url: String,
    target: u64,
    segments: Vec<Segment>,
    map: Option<String>,
    ended: bool,
}

/// The media playlist behind `url` (a master playlist is followed to one variant).
async fn media_playlist(url: &str, allow_private: bool, checked: &mut HashSet<String>) -> Result<Media, String> {
    let mut url = url.to_string();
    for _ in 0..3 {
        let (last, body) = fetch(&url, allow_private, checked).await?;
        match parse_playlist(&last, &String::from_utf8_lossy(&body))? {
            Playlist::Master(variants) => {
                url = pick_variant(&variants).ok_or("the HLS playlist lists no streams")?.to_string();
            }
            Playlist::Media { encrypted: true, .. } => return Err("the station's stream is encrypted".into()),
            Playlist::Media { target, segments, map, ended, .. } => return Ok(Media { url: last, target, segments, map, ended }),
        }
    }
    Err("the HLS playlist points to itself".into())
}

/// A live HLS station for Songbird.
#[derive(Clone)]
pub struct HlsSource {
    url: String,
    allow_private: bool,
}

impl HlsSource {
    pub fn new(url: String, allow_private: bool) -> Self {
        Self { url, allow_private }
    }
}

impl From<HlsSource> for Input {
    fn from(source: HlsSource) -> Self {
        Input::Lazy(Box::new(source))
    }
}

#[async_trait]
impl Compose for HlsSource {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Err(AudioStreamError::Unsupported)
    }

    async fn create_async(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        let fail = |e: String| AudioStreamError::Fail(Box::new(IoError::other(e)));
        let mut checked = HashSet::new();
        let media = media_playlist(&self.url, self.allow_private, &mut checked).await.map_err(fail)?;
        let feed = Arc::new(Feed::default());
        // The first audio is fetched before the player starts, so a stream that can't
        // be read fails here with a reason instead of playing silence.
        let mut joiner = Joiner::new();
        let mut follow = Follow { media, next: 0, checked, allow_private: self.allow_private };
        follow.next = follow.media.segments.len().saturating_sub(START_PIECES);
        if let Some(map) = follow.media.map.clone() {
            let (_, init) = fetch(&map, self.allow_private, &mut follow.checked).await.map_err(fail)?;
            feed.lock().buf.extend(init);
        }
        let mut first = Vec::new();
        while first.is_empty() && follow.next < follow.media.segments.len() {
            let piece = follow.take_next().await.map_err(fail)?;
            first = if follow.media.map.is_some() { piece } else { joiner.push(&piece).map_err(fail)? };
        }
        if first.is_empty() {
            return Err(fail("the station sent no audio".into()));
        }
        feed.lock().buf.extend(first);
        tokio::spawn(follow.run(feed.clone(), joiner));
        Ok(AudioStream { input: Box::new(FeedReader { feed, pos: 0 }) })
    }

    fn should_create_async(&self) -> bool {
        true
    }
}

struct Follow {
    media: Media,
    /// Index in `media.segments` of the next piece to fetch.
    next: usize,
    checked: HashSet<String>,
    allow_private: bool,
}

impl Follow {
    async fn take_next(&mut self) -> Result<Vec<u8>, String> {
        let url = self.media.segments[self.next].url.clone();
        self.next += 1;
        Ok(fetch(&url, self.allow_private, &mut self.checked).await?.1)
    }

    async fn reload(&mut self) -> Result<(), String> {
        let last_seq = self.next.checked_sub(1).map(|i| self.media.segments[i].seq);
        let (url, body) = fetch(&self.media.url, self.allow_private, &mut self.checked).await?;
        let Playlist::Media { target, segments, ended, .. } = parse_playlist(&url, &String::from_utf8_lossy(&body))? else {
            return Err("the station's playlist changed".into());
        };
        self.next = match last_seq {
            // Pieces after the last one played; if it fell off the list, start at the live edge.
            Some(seq) if segments.first().is_some_and(|s| s.seq <= seq + 1) => segments.iter().position(|s| s.seq > seq).unwrap_or(segments.len()),
            Some(_) => segments.len().saturating_sub(START_PIECES),
            None => 0,
        };
        self.media.target = target;
        self.media.segments = segments;
        self.media.ended = ended;
        Ok(())
    }

    async fn run(mut self, feed: Arc<Feed>, mut joiner: Joiner) {
        let mut failures = 0u32;
        let outcome: Result<(), String> = loop {
            if feed.lock().closed {
                return;
            }
            if feed.lock().buf.len() > MAX_BUFFER {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            if self.next < self.media.segments.len() {
                match self.take_next().await {
                    Ok(piece) => {
                        let audio = if self.media.map.is_some() { Ok(piece) } else { joiner.push(&piece) };
                        match audio {
                            Ok(audio) => {
                                let mut state = feed.lock();
                                state.buf.extend(audio);
                                drop(state);
                                feed.data.notify_all();
                                failures = 0;
                            }
                            Err(err) => break Err(err),
                        }
                    }
                    // A piece that's gone is skipped; the next reload decides.
                    Err(_) => failures += 1,
                }
                if failures >= RELOAD_FAILURES {
                    break Err("the station's audio pieces stopped loading".into());
                }
                continue;
            }
            if self.media.ended {
                break Ok(());
            }
            tokio::time::sleep(Duration::from_secs((self.media.target / 2).max(1))).await;
            match self.reload().await {
                Ok(()) => failures = 0,
                Err(err) => {
                    failures += 1;
                    if failures >= RELOAD_FAILURES {
                        break Err(format!("the station's playlist stopped loading ({err})"));
                    }
                }
            }
        };
        let mut state = feed.lock();
        match outcome {
            Ok(()) => state.eof = true,
            Err(err) => state.error = Some(err),
        }
        drop(state);
        feed.data.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_playlists_pick_an_audio_variant() {
        let text = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=24000,CODECS=\"mp4a.40.2\"\nhttps://cdn.example/zc/2/playlist.m3u8\n";
        let Playlist::Master(v) = parse_playlist("https://n.example/zc/hls.m3u8?tok=1", text).unwrap() else { panic!() };
        assert_eq!(pick_variant(&v), Some("https://cdn.example/zc/2/playlist.m3u8"));
        let text = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,CODECS=\"avc1.4d401f,mp4a.40.2\"\nvideo.m3u8\n\
                    #EXT-X-STREAM-INF:BANDWIDTH=48000,CODECS=\"mp4a.40.5\"\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=320000,CODECS=\"mp4a.40.2\"\nhigh.m3u8\n\
                    #EXT-X-STREAM-INF:BANDWIDTH=640000,CODECS=\"mp4a.40.2\"\ntoo-big.m3u8\n";
        let Playlist::Master(v) = parse_playlist("https://n.example/live/master.m3u8", text).unwrap() else { panic!() };
        assert_eq!(pick_variant(&v), Some("https://n.example/live/high.m3u8"));
    }

    #[test]
    fn media_playlists_list_pieces_with_sequence_numbers() {
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXT-X-MEDIA-SEQUENCE:179727593\n#EXT-X-DISCONTINUITY\n\
                    #EXTINF:10,title=\"Space Lord\",artist=\"MONSTER MAGNET\"\nhttps://cdn.example/main/179727591.aac?x=1\n\
                    #EXTINF:10,\nseg2.aac\n";
        let Playlist::Media { target, segments, map, encrypted, ended } = parse_playlist("https://n.example/a/playlist.m3u8", text).unwrap() else { panic!() };
        assert_eq!(target, 10);
        assert_eq!(segments[0], Segment { url: "https://cdn.example/main/179727591.aac?x=1".into(), seq: 179727593 });
        assert_eq!(segments[1], Segment { url: "https://n.example/a/seg2.aac".into(), seq: 179727594 });
        assert!(map.is_none() && !encrypted && !ended);
        let locked = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:5,\na.ts\n";
        assert!(matches!(parse_playlist("https://x/p.m3u8", locked).unwrap(), Playlist::Media { encrypted: true, .. }));
        assert!(parse_playlist("https://x/p.m3u8", "[playlist]\nFile1=http://x").is_err());
        assert!(parse_playlist("https://x/p.m3u8", "#EXTM3U\n#EXT-X-ENDLIST\n").is_err());
    }

    fn adts_frame(id_mpeg2: bool, crc: bool, payload: &[u8]) -> Vec<u8> {
        let head = if crc { 9 } else { 7 };
        let len = head + payload.len();
        let mut f = vec![0xff, 0xf0 | if id_mpeg2 { 0x08 } else { 0 } | if crc { 0 } else { 1 }, 0x50, 0x80, 0, 0, 0xfc];
        f[3] |= ((len >> 11) & 3) as u8;
        f[4] = (len >> 3) as u8;
        f[5] = (((len & 7) as u8) << 5) | 0x1f;
        if crc {
            f.extend_from_slice(&[0xab, 0xcd]);
        }
        f.extend_from_slice(payload);
        f
    }

    #[test]
    fn adts_frames_are_rewritten_to_what_the_decoder_syncs_on() {
        let mut fix = AdtsFix::default();
        let mut input = adts_frame(true, false, &[1, 2, 3]);
        input.extend(adts_frame(false, true, &[4, 5, 6, 7]));
        input.extend(adts_frame(false, false, &[8]));
        // Split mid-frame, as pieces arrive.
        let mut out = fix.push(&input[..12]);
        out.extend(fix.push(&input[12..]));
        assert_eq!(out, [adts_frame(false, false, &[1, 2, 3]), adts_frame(false, false, &[4, 5, 6, 7]), adts_frame(false, false, &[8])].concat());
    }

    #[test]
    fn a_plain_aac_stream_joined_mid_frame_reads_from_the_next_frame() {
        // Joined part way into a frame whose tail happens to look like a sync word.
        let mut input = vec![0x12, 0xff, 0xf9, 0x50, 0x80, 0x01, 0x1f, 0xfc, 0x33, 0x44];
        input.extend(adts_frame(true, false, &[1, 2, 3]));
        input.extend(adts_frame(true, true, &[4, 5]));
        input.extend(adts_frame(true, false, &[6]));
        let reader = AdtsReader { inner: Box::new(std::io::Cursor::new(input)), fix: AdtsFix::default(), out: VecDeque::new(), pos: 0 };
        let mut out = Vec::new();
        std::io::BufReader::new(reader).read_to_end(&mut out).unwrap();
        // The last frame can't be checked against a next one, but it is complete.
        assert_eq!(out, [adts_frame(false, false, &[1, 2, 3]), adts_frame(false, false, &[4, 5]), adts_frame(false, false, &[6])].concat());
    }

    #[test]
    fn id3_tags_are_skipped_and_pieces_sniffed() {
        let mut piece = b"ID3\x04\x00\x00\x00\x00\x00\x05PRIV!".to_vec();
        piece.extend(adts_frame(false, false, &[9; 20]));
        assert_eq!(strip_id3(&piece)[..2], [0xff, 0xf1]);
        assert_eq!(sniff(&piece), Some(Pieces::Aac));
        assert_eq!(sniff(&[0xff, 0xfb, 0x90, 0x64]), Some(Pieces::Mp3));
        assert_eq!(sniff(b"<html>"), None);
    }

    fn ts_packet(pid: u16, start: bool, payload: &[u8]) -> Vec<u8> {
        // Short payloads are padded with adaptation-field stuffing, as muxers do.
        let mut p = vec![0x47, (if start { 0x40 } else { 0 }) | ((pid >> 8) as u8 & 0x1f), pid as u8, 0x10];
        if payload.len() < 184 {
            p[3] = 0x30;
            let stuffing = 183 - payload.len();
            p.push(stuffing as u8);
            if stuffing > 0 {
                p.push(0);
                p.extend(std::iter::repeat_n(0xff, stuffing - 1));
            }
        }
        p.extend_from_slice(payload);
        assert_eq!(p.len(), 188);
        p
    }

    #[test]
    fn mpeg_ts_pieces_give_their_aac_audio() {
        // PAT: program 1 -> PMT pid 0x100. PMT: AAC (0x0f) on pid 0x101.
        let pat = [0u8, 0x00, 0xb0, 0x0d, 0, 1, 0xc1, 0, 0, 0, 1, 0xe1, 0x00, 0, 0, 0, 0];
        let pmt = [0u8, 0x02, 0xb0, 0x12, 0, 1, 0xc1, 0, 0, 0xe1, 0x01, 0xf0, 0x00, 0x0f, 0xe1, 0x01, 0xf0, 0x00, 0, 0, 0, 0];
        let frame = adts_frame(true, false, &[7; 30]);
        let mut pes = vec![0, 0, 1, 0xc0, 0, 0, 0x80, 0x80, 5, 0, 0, 0, 0, 0];
        pes.extend_from_slice(&frame[..20]);
        let mut piece = ts_packet(0, true, &pat);
        piece.extend(ts_packet(0x100, true, &pmt));
        piece.extend(ts_packet(0x101, true, &pes));
        // A video packet in between is ignored.
        piece.extend(ts_packet(0x102, true, &[0, 0, 1, 0xe0]));
        let mut joiner = Joiner::new();
        let first = joiner.push(&piece).unwrap();
        // Rest of the frame arrives in the next piece.
        let rest = joiner.push(&ts_packet(0x101, false, &frame[20..])).unwrap();
        assert_eq!([first, rest].concat(), adts_frame(false, false, &[7; 30]));
    }

    #[test]
    fn mpeg_ts_without_aac_or_mp3_is_refused_with_a_reason() {
        let pat = [0u8, 0x00, 0xb0, 0x0d, 0, 1, 0xc1, 0, 0, 0, 1, 0xe1, 0x00, 0, 0, 0, 0];
        let pmt = [0u8, 0x02, 0xb0, 0x12, 0, 1, 0xc1, 0, 0, 0xe1, 0x01, 0xf0, 0x00, 0x11, 0xe1, 0x01, 0xf0, 0x00, 0, 0, 0, 0];
        let mut piece = ts_packet(0, true, &pat);
        piece.extend(ts_packet(0x100, true, &pmt));
        let err = Joiner::new().push(&piece).unwrap_err();
        assert!(err.contains("can't play"), "{err}");
    }

    #[test]
    fn the_feed_hands_out_bytes_then_the_error() {
        let feed = Arc::new(Feed::default());
        feed.lock().buf.extend([1u8, 2, 3]);
        let mut reader = FeedReader { feed: feed.clone(), pos: 0 };
        let mut buf = [0u8; 8];
        assert_eq!(reader.read(&mut buf).unwrap(), 3);
        assert_eq!(reader.stream_position().unwrap(), 3);
        assert!(reader.seek(SeekFrom::Start(0)).is_err());
        feed.lock().error = Some("gone".into());
        assert_eq!(reader.read(&mut buf).unwrap_err().to_string(), "gone");
        drop(reader);
        assert!(feed.lock().closed, "the follower stops when the player drops the stream");
    }
}
