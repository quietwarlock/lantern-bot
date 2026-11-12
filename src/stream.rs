//! An HTTP audio source that can seek.
//!
//! Songbird's built-in HTTP source reads straight through and can't jump.
//! This one keeps a small read-ahead buffer filled by a background task.
//! When the player seeks outside that buffer, the task reconnects with
//! `Range: bytes=N-`, so `seek`, `forward`, and `rewind` work on YouTube and
//! on direct files. If the connection drops mid-song it reconnects at the same
//! byte. Live radio (no length) is read straight through and reconnects from "now".

use futures::StreamExt;
use reqwest::header::{HeaderMap, ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use reqwest::StatusCode;
use serenity::async_trait;
use songbird::input::{AudioStream, AudioStreamError, Compose, Input};
use std::collections::VecDeque;
use std::io::{Error as IoError, ErrorKind, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;
use symphonia::core::io::MediaSource;
use tokio::sync::Notify;

/// How far the background task reads ahead of the player.
const READ_AHEAD: usize = 2 * 1024 * 1024;
/// A forward seek this close to the buffered data waits instead of reconnecting.
const NEAR: u64 = 512 * 1024;
/// Reconnect attempts in a row before the track is given up.
const RECONNECTS: u32 = 3;
/// The player gives up on a stream that sends nothing for this long.
const STALL: Duration = Duration::from_secs(40);
/// YouTube serves `&range=` requests at full speed but throttles one long
/// download to about real time, so it is fetched in pieces of this size.
const QUERY_CHUNK: u64 = 8 * 1024 * 1024;

type Bytes = futures::stream::BoxStream<'static, std::io::Result<bytes::Bytes>>;

#[derive(Clone)]
pub struct RangeSource {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    /// Total size when known up front. With it, pieces are asked for with a
    /// `&range=start-end` query instead of a `Range` header (YouTube).
    query_len: Option<u64>,
    /// Live radio may reconnect after a drop without a byte offset.
    live_stream: bool,
}

impl RangeSource {
    pub fn new(client: reqwest::Client, url: String, headers: HeaderMap) -> Self {
        Self { client, url, headers, query_len: None, live_stream: false }
    }

    /// For googlevideo links: fetch by `&range=` pieces of a file of this size.
    pub fn with_query_range(mut self, len: Option<u64>) -> Self {
        self.query_len = len.filter(|l| *l > 0);
        self
    }

    pub fn with_live_stream(mut self, live: bool) -> Self {
        self.live_stream = live;
        self
    }

    async fn open(&self, pos: u64, ranged: bool) -> Result<Opened, IoError> {
        if let Some(len) = self.query_len {
            let end = (pos + QUERY_CHUNK).min(len) - 1;
            let sep = if self.url.contains('?') { '&' } else { '?' };
            let url = format!("{}{sep}range={pos}-{end}", self.url);
            let response = self.client.get(url).headers(self.headers.clone()).send().await.map_err(IoError::other)?;
            let status = response.status();
            if !status.is_success() {
                return Err(IoError::other(format!("the server answered {status}")));
            }
            return Ok(Opened { body: body_of(response), len: Some(len), ranges: true });
        }
        let mut request = self.client.get(&self.url).headers(self.headers.clone());
        if ranged {
            request = request.header(RANGE, format!("bytes={pos}-"));
        }
        let response = match request.send().await {
            Ok(response) => response,
            // Old Shoutcast servers answer `ICY 200 OK`, which isn't HTTP.
            Err(err) if !ranged && may_be_icy(&err) => {
                let (_, body) = icy_open(&self.url, &self.headers).await.map_err(|_| IoError::other(err))?;
                return Ok(Opened { body, len: None, ranges: false });
            }
            Err(err) => return Err(IoError::other(err)),
        };
        let status = response.status();
        if !status.is_success() {
            return Err(IoError::other(format!("the server answered {status}")));
        }
        if ranged && pos > 0 && status != StatusCode::PARTIAL_CONTENT {
            return Err(IoError::new(ErrorKind::Unsupported, "the server can't jump to a position"));
        }
        let headers = response.headers();
        let total = headers
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.parse::<u64>().ok());
        let len = total.or_else(|| {
            headers
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|n| n + if status == StatusCode::PARTIAL_CONTENT { pos } else { 0 })
        });
        let ranges = status == StatusCode::PARTIAL_CONTENT
            || headers
                .get(ACCEPT_RANGES)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.eq_ignore_ascii_case("bytes"));
        Ok(Opened { body: body_of(response), len, ranges })
    }
}

/// Lowercase header names and their values.
pub(crate) type IcyHeaders = Vec<(String, String)>;

fn body_of(response: reqwest::Response) -> Bytes {
    response.bytes_stream().map(|r| r.map_err(IoError::other)).boxed()
}

/// A failure that may just be an `ICY 200 OK` answer: the request was sent
/// to a plain `http://` server and the reply couldn't be read as HTTP.
pub(crate) fn may_be_icy(err: &reqwest::Error) -> bool {
    !err.is_timeout() && !err.is_connect() && err.url().is_some_and(|u| u.scheme() == "http")
}

/// The status line and headers of an `ICY 200 OK` (or HTTP/1.x) answer.
/// Returns the status code, lowercase headers, and where the audio starts.
pub(crate) fn parse_icy_head(head: &[u8]) -> Option<(u16, IcyHeaders, usize)> {
    let end = head.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
        .or_else(|| head.windows(2).position(|w| w == b"\n\n").map(|i| i + 2))?;
    let text = String::from_utf8_lossy(&head[..end]);
    let mut lines = text.lines();
    let status = lines.next()?.trim();
    let (proto, rest) = status.split_once(' ')?;
    if !(proto.eq_ignore_ascii_case("ICY") || proto.to_ascii_uppercase().starts_with("HTTP/1.")) {
        return None;
    }
    let code = rest.trim().split(' ').next()?.parse::<u16>().ok()?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Some((code, headers, end))
}

/// Opens an old Shoutcast stream by hand, since the HTTP client rejects
/// `ICY 200 OK`. Plain `http://` only. Returns the headers and the audio.
pub(crate) async fn icy_open(url: &str, extra: &HeaderMap) -> Result<(IcyHeaders, Bytes), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let parsed = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
    if parsed.scheme() != "http" {
        return Err("not an http link".into());
    }
    let host = parsed.host_str().ok_or("no host")?.to_string();
    let port = parsed.port_or_known_default().unwrap_or(80);
    let mut path = parsed.path().to_string();
    if let Some(q) = parsed.query() {
        path.push('?');
        path.push_str(q);
    }
    let wait = Duration::from_secs(15);
    let mut sock = tokio::time::timeout(wait, tokio::net::TcpStream::connect((host.as_str(), port)))
        .await
        .map_err(|_| "timed out connecting".to_string())?
        .map_err(|e| e.to_string())?;
    let mut request = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nIcy-MetaData: 0\r\nAccept: */*\r\n");
    let has_agent = extra.contains_key(reqwest::header::USER_AGENT);
    for (name, value) in extra {
        if name == reqwest::header::RANGE || name.as_str().eq_ignore_ascii_case("icy-metadata") {
            continue;
        }
        if let Ok(value) = value.to_str() {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    if !has_agent {
        request.push_str("User-Agent: Lantern\r\n");
    }
    request.push_str("\r\n");
    sock.write_all(request.as_bytes()).await.map_err(|e| e.to_string())?;
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    let (code, headers, start) = loop {
        let n = tokio::time::timeout(wait, sock.read(&mut buf)).await.map_err(|_| "timed out".to_string())?.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("the server hung up".into());
        }
        head.extend_from_slice(&buf[..n]);
        if let Some(found) = parse_icy_head(&head) {
            break found;
        }
        if head.len() > 16 * 1024 {
            return Err("the server's answer wasn't HTTP or ICY".into());
        }
    };
    if code != 200 {
        return Err(format!("the server answered {code}"));
    }
    let first = bytes::Bytes::copy_from_slice(&head[start..]);
    let rest = futures::stream::unfold(sock, move |mut sock| async move {
        let mut buf = vec![0u8; 16 * 1024];
        match tokio::time::timeout(Duration::from_secs(30), sock.read(&mut buf)).await {
            Ok(Ok(0)) => None,
            Ok(Ok(n)) => {
                buf.truncate(n);
                Some((Ok(bytes::Bytes::from(buf)), sock))
            }
            Ok(Err(e)) => Some((Err(e), sock)),
            Err(_) => Some((Err(IoError::new(ErrorKind::TimedOut, "the stream stopped sending")), sock)),
        }
    });
    let body = futures::stream::iter((!first.is_empty()).then_some(Ok(first))).chain(rest).boxed();
    Ok((headers, body))
}

struct Opened {
    body: Bytes,
    len: Option<u64>,
    ranges: bool,
}

impl From<RangeSource> for Input {
    fn from(source: RangeSource) -> Self {
        Input::Lazy(Box::new(source))
    }
}

#[async_trait]
impl Compose for RangeSource {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Err(AudioStreamError::Unsupported)
    }

    async fn create_async(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        let opened = self.open(0, false).await.map_err(|e| AudioStreamError::Fail(Box::new(e)))?;
        let seekable = opened.ranges && opened.len.is_some();
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            data: Condvar::new(),
            wake: Notify::new(),
        });
        let fetch = Fetcher {
            source: self.clone(),
            shared: shared.clone(),
            len: opened.len,
            seekable,
            resumable: opened.ranges && !self.live_stream,
            live_stream: self.live_stream,
        };
        tokio::spawn(fetch.run(opened.body));
        let reader = SeekableHttp { shared, len: opened.len, seekable, pos: 0 };
        Ok(AudioStream { input: Box::new(reader) })
    }

    fn should_create_async(&self) -> bool {
        true
    }
}

#[derive(Default)]
struct State {
    /// Bytes from `start` onward.
    buf: VecDeque<u8>,
    start: u64,
    /// Where the player is reading.
    want: u64,
    /// Bumped on every reconnecting seek; the fetcher drops its old connection.
    generation: u64,
    eof: bool,
    error: Option<String>,
    /// The player dropped the track.
    closed: bool,
}

struct Shared {
    state: Mutex<State>,
    /// Signals the player that bytes, an error, or the end arrived.
    data: Condvar,
    /// Signals the fetcher that the player moved, freed space, or closed.
    wake: Notify,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Background task that keeps the buffer full.
struct Fetcher {
    source: RangeSource,
    shared: Arc<Shared>,
    len: Option<u64>,
    seekable: bool,
    /// The server takes `Range` even though it sent no length (a chunked
    /// file), so a dropped connection resumes at the same byte, not byte 0.
    resumable: bool,
    live_stream: bool,
}

impl Fetcher {
    async fn run(self, first: Bytes) {
        let mut body = Some(first);
        let mut failures = 0u32;
        loop {
            let (generation, from) = {
                let state = self.shared.lock();
                if state.closed {
                    return;
                }
                (state.generation, state.start + state.buf.len() as u64)
            };
            // The first connection starts at byte 0. If the player jumped elsewhere
            // before this task got going, that body would be filed under the wrong offset.
            let reusable = body.take().filter(|_| from == 0);
            // Nothing lies past the end. Asking for it only earns a 416 or a bad request.
            if reusable.is_none() && self.len.is_some_and(|len| from >= len) {
                {
                    let mut state = self.shared.lock();
                    if state.generation == generation {
                        state.eof = true;
                    }
                }
                self.shared.data.notify_all();
                if !self.wait_for_seek(generation).await {
                    return;
                }
                continue;
            }
            let mut stream = match reusable {
                Some(stream) => stream,
                None => {
                    let ranged = (self.seekable || self.resumable) && from > 0;
                    match self.source.open(from, ranged).await {
                        Ok(opened) => opened.body,
                        Err(err) => {
                            failures += 1;
                            if failures > RECONNECTS || !self.can_reconnect() {
                                self.fail(generation, self.reconnect_error(err.to_string()));
                                if !self.wait_for_seek(generation).await {
                                    return;
                                }
                                failures = 0;
                            } else {
                                tokio::time::sleep(Duration::from_millis(500 * u64::from(failures))).await;
                            }
                            continue;
                        }
                    }
                }
            };
            // Read until the end, an error, or a seek elsewhere.
            loop {
                if !self.wait_for_space(generation).await {
                    break;
                }
                let next = tokio::select! {
                    chunk = stream.next() => chunk,
                    _ = self.shared.wake.notified() => {
                        if self.moved(generation) { break; }
                        continue;
                    }
                };
                match next {
                    Some(Ok(chunk)) => {
                        failures = 0;
                        let mut state = self.shared.lock();
                        if state.generation != generation || state.closed {
                            break;
                        }
                        state.buf.extend(chunk.iter());
                        drop(state);
                        self.shared.data.notify_all();
                    }
                    Some(Err(err)) => {
                        failures += 1;
                        if failures > RECONNECTS || !self.can_reconnect() {
                            self.fail(generation, self.reconnect_error(err.to_string()));
                            if !self.wait_for_seek(generation).await {
                                return;
                            }
                            failures = 0;
                        }
                        break;
                    }
                    None => {
                        let end = { let s = self.shared.lock(); s.start + s.buf.len() as u64 };
                        let short = self.len.is_some_and(|len| end < len);
                        if short && self.seekable && failures < RECONNECTS {
                            // The server hung up early. Reconnect where it stopped.
                            failures += 1;
                            break;
                        }
                        {
                            let mut state = self.shared.lock();
                            if state.generation == generation {
                                state.eof = true;
                            }
                        }
                        self.shared.data.notify_all();
                        if !self.wait_for_seek(generation).await {
                            return;
                        }
                        break;
                    }
                }
            }
        }
    }

    fn can_reconnect(&self) -> bool {
        if self.seekable || self.resumable {
            return true;
        }
        if self.len.is_none() {
            return self.live_stream;
        }
        false
    }

    fn reconnect_error(&self, detail: String) -> String {
        if self.len.is_none() && !self.live_stream {
            "The connection dropped and this stream can't be resumed (no length and not live radio).".to_string()
        } else {
            detail
        }
    }

    fn moved(&self, generation: u64) -> bool {
        let state = self.shared.lock();
        state.generation != generation || state.closed
    }

    fn fail(&self, generation: u64, error: String) {
        let mut state = self.shared.lock();
        if state.generation == generation {
            state.error = Some(error);
        }
        drop(state);
        self.shared.data.notify_all();
    }

    /// Waits while the buffer is full. False if the player moved or closed.
    async fn wait_for_space(&self, generation: u64) -> bool {
        loop {
            {
                let state = self.shared.lock();
                if state.closed || state.generation != generation {
                    return false;
                }
                let ahead = (state.start + state.buf.len() as u64).saturating_sub(state.want);
                if ahead < READ_AHEAD as u64 {
                    return true;
                }
            }
            self.shared.wake.notified().await;
        }
    }

    /// After the end or an error, wait for a seek. False if the player closed.
    async fn wait_for_seek(&self, generation: u64) -> bool {
        loop {
            {
                let state = self.shared.lock();
                if state.closed {
                    return false;
                }
                if state.generation != generation {
                    return true;
                }
            }
            self.shared.wake.notified().await;
        }
    }
}

/// The player's side: a normal blocking reader for symphonia.
struct SeekableHttp {
    shared: Arc<Shared>,
    len: Option<u64>,
    seekable: bool,
    pos: u64,
}

impl Read for SeekableHttp {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut state = self.shared.lock();
        loop {
            // Drop what's behind the player.
            let behind = self.pos.saturating_sub(state.start).min(state.buf.len() as u64) as usize;
            if behind > 0 {
                state.buf.drain(..behind);
                state.start += behind as u64;
                self.shared.wake.notify_one();
            }
            if state.start == self.pos && !state.buf.is_empty() {
                let n = out.len().min(state.buf.len());
                for (slot, byte) in out.iter_mut().zip(state.buf.drain(..n)) {
                    *slot = byte;
                }
                state.start += n as u64;
                self.pos += n as u64;
                state.want = self.pos;
                drop(state);
                self.shared.wake.notify_one();
                return Ok(n);
            }
            if state.eof {
                return Ok(0);
            }
            if let Some(err) = &state.error {
                return Err(IoError::other(err.clone()));
            }
            let (next, timeout) = self.shared.data.wait_timeout(state, STALL).unwrap_or_else(|p| p.into_inner());
            state = next;
            if timeout.timed_out() && state.buf.is_empty() && !state.eof {
                return Err(IoError::new(ErrorKind::TimedOut, "the stream stopped sending audio"));
            }
        }
    }
}

impl Seek for SeekableHttp {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        let target = match position {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => self.len.and_then(|len| len.checked_add_signed(d)),
        }
        .ok_or_else(|| IoError::from(ErrorKind::InvalidInput))?;
        // A jump past the end lands on the end, so it reads as the end of the file.
        let target = self.len.map_or(target, |len| target.min(len));
        if target == self.pos {
            return Ok(target);
        }
        if !self.seekable {
            return Err(ErrorKind::Unsupported.into());
        }
        let mut state = self.shared.lock();
        let end = state.start + state.buf.len() as u64;
        let near = target >= state.start && target <= end + NEAR && !state.eof && state.error.is_none();
        let buffered = target >= state.start && target <= end;
        if !(near || buffered) {
            state.generation += 1;
            state.buf.clear();
            state.start = target;
            state.eof = false;
            state.error = None;
        }
        state.want = target;
        drop(state);
        self.pos = target;
        self.shared.wake.notify_one();
        Ok(target)
    }
}

impl MediaSource for SeekableHttp {
    fn is_seekable(&self) -> bool {
        self.seekable
    }

    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}

impl Drop for SeekableHttp {
    fn drop(&mut self) {
        self.shared.lock().closed = true;
        self.shared.wake.notify_one();
    }
}

/// A whole song already in memory (SoundCloud's HLS pieces joined together).
/// It can seek anywhere.
#[derive(Clone)]
pub struct MemorySource {
    bytes: Arc<Vec<u8>>,
}

impl MemorySource {
    pub fn new(bytes: Arc<Vec<u8>>) -> Self {
        Self { bytes }
    }
}

struct MemoryReader {
    bytes: Arc<Vec<u8>>,
    pos: u64,
}

impl Read for MemoryReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let start = (self.pos as usize).min(self.bytes.len());
        let n = buf.len().min(self.bytes.len() - start);
        buf[..n].copy_from_slice(&self.bytes[start..start + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for MemoryReader {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let len = self.bytes.len() as i128;
        let next = match to {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::End(n) => len + n as i128,
            SeekFrom::Current(n) => self.pos as i128 + n as i128,
        };
        if next < 0 {
            return Err(IoError::new(ErrorKind::InvalidInput, "seek before the start"));
        }
        self.pos = next as u64;
        Ok(self.pos)
    }
}

impl MediaSource for MemoryReader {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.bytes.len() as u64)
    }
}

#[async_trait]
impl Compose for MemorySource {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Ok(AudioStream { input: Box::new(MemoryReader { bytes: self.bytes.clone(), pos: 0 }) })
    }

    async fn create_async(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        self.create()
    }

    fn should_create_async(&self) -> bool {
        false
    }
}

impl From<MemorySource> for Input {
    fn from(source: MemorySource) -> Self {
        Input::Lazy(Box::new(source))
    }
}


#[cfg(test)]
mod tests {
    #[test]
    fn icy_answers_are_read_like_http() {
        use super::parse_icy_head;
        let head = b"ICY 200 OK\r\nicy-name: Test FM\r\ncontent-type: audio/mpeg\r\n\r\nAUDIO";
        let (code, headers, start) = parse_icy_head(head).unwrap();
        assert_eq!(code, 200);
        assert!(headers.contains(&("icy-name".to_string(), "Test FM".to_string())));
        assert_eq!(&head[start..], b"AUDIO");
        assert!(parse_icy_head(b"ICY 200 OK\r\nicy-name: x\r\n").is_none(), "headers not finished");
        assert!(parse_icy_head(b"SSH-2.0-x\r\n\r\n").is_none());
    }

    #[tokio::test]
    async fn an_icy_200_shoutcast_server_plays() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = pattern(50_000);
        let sent = body.clone();
        thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { continue };
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(b"ICY 200 OK\r\nicy-name: Test FM\r\ncontent-type: audio/mpeg\r\n\r\n");
                let _ = sock.write_all(&sent);
            }
        });
        let mut source = RangeSource::new(reqwest::Client::new(), format!("http://127.0.0.1:{port}/"), HeaderMap::new()).with_live_stream(true);
        let mut reader = source.create_async().await.expect("ICY stream opens").input;
        let got = blocking(move || {
            let mut out = vec![0u8; 50_000];
            reader.read_exact(&mut out).map(|_| out)
        })
        .await
        .unwrap();
        assert_eq!(got, body);
    }

    use super::RangeSource;
    use reqwest::header::HeaderMap;
    use songbird::input::codecs::{get_codec_registry, get_probe};
    use songbird::input::{Compose, Input};
    use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;
    use symphonia::core::formats::{SeekMode, SeekTo};
    use symphonia::core::io::MediaSource;
    use symphonia::core::units::Time;

    /// How the local test server treats requests.
    #[derive(Clone, Default)]
    struct Behavior {
        /// Honors `Range: bytes=N-` and says so.
        ranges: bool,
        /// Says it has ranges but answers every request with the whole file.
        ignore_ranges: bool,
        /// Hangs up on the first connection after this many bytes of a longer Content-Length.
        drop_first_after: Option<usize>,
        /// Answers every request with this status and no body.
        fail_with: Option<u16>,
        /// Pause between 8 KB pieces.
        pace: Duration,
        /// Sends the body chunked, with no length anywhere.
        chunked: bool,
    }

    struct Server {
        port: u16,
        connections: Arc<AtomicUsize>,
        sent: Arc<AtomicU64>,
        /// Requests for bytes past the end of the file.
        refused: Arc<AtomicUsize>,
    }

    /// A tiny HTTP server on localhost. It also understands YouTube's `?range=a-b`.
    fn serve(body: Vec<u8>, how: Behavior) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server = Server {
            port: listener.local_addr().unwrap().port(),
            connections: Arc::default(),
            sent: Arc::default(),
            refused: Arc::default(),
        };
        let (connections, sent, refused) = (server.connections.clone(), server.sent.clone(), server.refused.clone());
        let body = Arc::new(body);
        thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(sock) = sock else { continue };
                let index = connections.fetch_add(1, Ordering::SeqCst);
                let (body, how, sent, refused) = (body.clone(), how.clone(), sent.clone(), refused.clone());
                thread::spawn(move || answer(sock, &body, &how, index, &sent, &refused));
            }
        });
        server
    }

    fn answer(mut sock: TcpStream, body: &[u8], how: &Behavior, index: usize, sent: &AtomicU64, refused: &AtomicUsize) {
        sock.set_write_timeout(Some(Duration::from_secs(10))).ok();
        let mut buf = [0u8; 4096];
        let n = sock.read(&mut buf).unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
        if let Some(code) = how.fail_with {
            let _ = write!(sock, "HTTP/1.1 {code} Nope\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return;
        }
        let total = body.len();
        let path = request.lines().next().unwrap_or("").split_whitespace().nth(1).unwrap_or("");
        let query_range = path.split("range=").nth(1).and_then(|r| {
            let (from, to) = r.split_once('-')?;
            Some((from.parse::<usize>().ok()?, to.parse::<usize>().ok()?))
        });
        let header_start = request
            .lines()
            .find_map(|l| l.strip_prefix("range: bytes="))
            .and_then(|r| r.split('-').next())
            .and_then(|s| s.trim().parse::<usize>().ok());
        let out_of_range = |sock: &mut TcpStream| {
            refused.fetch_add(1, Ordering::SeqCst);
            let _ = write!(
                sock,
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        };
        let (status, start, end) = match (query_range, header_start) {
            (Some((from, _)), _) if from >= total => return out_of_range(&mut sock),
            (Some((from, to)), _) => ("HTTP/1.1 200 OK\r\n".to_string(), from, (to + 1).min(total)),
            (None, Some(from)) if how.ranges && !how.ignore_ranges => {
                if from >= total {
                    return out_of_range(&mut sock);
                }
                let size = if how.chunked { "*".to_string() } else { total.to_string() };
                (format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {from}-{}/{size}\r\n", total - 1), from, total)
            }
            _ => ("HTTP/1.1 200 OK\r\n".to_string(), 0, total),
        };
        let accept = if how.ranges || how.ignore_ranges { "Accept-Ranges: bytes\r\n" } else { "" };
        let length = if how.chunked { "Transfer-Encoding: chunked\r\n".to_string() } else { format!("Content-Length: {}\r\n", end - start) };
        let head = format!("{status}Content-Type: audio/mpeg\r\n{accept}{length}Connection: close\r\n\r\n");
        if sock.write_all(head.as_bytes()).is_err() {
            return;
        }
        let stop = match how.drop_first_after {
            Some(n) if index == 0 => (start + n).min(end),
            _ => end,
        };
        for chunk in body[start..stop].chunks(8192) {
            let framed = if how.chunked {
                let mut out = format!("{:x}\r\n", chunk.len()).into_bytes();
                out.extend_from_slice(chunk);
                out.extend_from_slice(b"\r\n");
                out
            } else {
                chunk.to_vec()
            };
            if sock.write_all(&framed).is_err() {
                return;
            }
            sent.fetch_add(chunk.len() as u64, Ordering::SeqCst);
            thread::sleep(how.pace);
        }
        if how.chunked && stop == end {
            let _ = sock.write_all(b"0\r\n\r\n");
        }
    }

    /// Bytes where every position is different enough to notice a wrong offset.
    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i.wrapping_mul(31) ^ (i >> 8)) as u8).collect()
    }

    fn url(port: u16) -> String {
        format!("http://127.0.0.1:{port}/song.mp3")
    }

    async fn open(port: u16, query_len: Option<u64>) -> Box<dyn MediaSource> {
        let mut source = RangeSource::new(reqwest::Client::new(), url(port), HeaderMap::new()).with_query_range(query_len);
        source.create_async().await.expect("open").input
    }

    async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
        tokio::task::spawn_blocking(work).await.expect("reader thread")
    }

    fn read_at(reader: &mut dyn MediaSource, pos: u64, count: usize) -> std::io::Result<Vec<u8>> {
        reader.seek(SeekFrom::Start(pos))?;
        let mut got = vec![0u8; count];
        reader.read_exact(&mut got)?;
        Ok(got)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_server_without_ranges_streams_but_cannot_seek() {
        let body = pattern(600_000);
        let server = serve(body.clone(), Behavior::default());
        let mut reader = open(server.port, None).await;
        assert!(!reader.is_seekable());
        assert_eq!(reader.byte_len(), Some(600_000));
        blocking(move || {
            let mut got = Vec::new();
            reader.read_to_end(&mut got).unwrap();
            assert!(got == body, "bytes differ");
            assert_eq!(reader.seek(SeekFrom::Start(10)).unwrap_err().kind(), ErrorKind::Unsupported);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ranged_seeks_return_the_right_bytes() {
        // Far enough from the start that no read-ahead buffer could reach it.
        let body = pattern(8_000_000);
        let server = serve(body.clone(), Behavior { ranges: true, ..Default::default() });
        let mut reader = open(server.port, None).await;
        assert!(reader.is_seekable());
        let connections = server.connections.clone();
        blocking(move || {
            assert!(read_at(&mut *reader, 0, 1000).unwrap() == body[..1000]);
            assert!(read_at(&mut *reader, 7_900_000, 1000).unwrap() == body[7_900_000..7_901_000]);
            assert!(read_at(&mut *reader, 100, 1000).unwrap() == body[100..1100]);
            // Inside what is already buffered: no new connection.
            let before = connections.load(Ordering::SeqCst);
            assert!(read_at(&mut *reader, 400_000, 1000).unwrap() == body[400_000..401_000]);
            assert_eq!(connections.load(Ordering::SeqCst), before, "a nearby seek reconnected");
            reader.seek(SeekFrom::End(-500)).unwrap();
            let mut tail = vec![0u8; 500];
            reader.read_exact(&mut tail).unwrap();
            assert!(tail == body[body.len() - 500..]);
            assert_eq!(reader.seek(SeekFrom::Current(-500)).unwrap(), 7_999_500);
        })
        .await;
        assert!(server.connections.load(Ordering::SeqCst) >= 3, "far seeks should open ranged connections");
        assert_eq!(server.refused.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn seeking_past_the_end_lands_on_the_end() {
        let body = pattern(3_000_000);
        let server = serve(body.clone(), Behavior { ranges: true, ..Default::default() });
        let mut reader = open(server.port, None).await;
        blocking(move || {
            assert!(read_at(&mut *reader, 0, 10).unwrap() == body[..10]);
            assert_eq!(reader.seek(SeekFrom::Start(3_005_000)).unwrap(), 3_000_000);
            assert_eq!(reader.read(&mut [0u8; 16]).unwrap(), 0, "past the end should read as the end");
            assert_eq!(reader.seek(SeekFrom::End(100)).unwrap(), 3_000_000);
            assert_eq!(reader.read(&mut [0u8; 16]).unwrap(), 0);
            // And the stream still works afterwards.
            assert!(read_at(&mut *reader, 2_999_990, 10).unwrap() == body[2_999_990..]);
            assert!(read_at(&mut *reader, 5, 10).unwrap() == body[5..15]);
        })
        .await;
        thread::sleep(Duration::from_millis(300));
        assert_eq!(server.refused.load(Ordering::SeqCst), 0, "asked the server for bytes past the end");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn a_seek_before_the_download_task_starts_reads_from_the_new_place() {
        let body = pattern(3_000_000);
        let server = serve(body.clone(), Behavior { ranges: true, ..Default::default() });
        // One worker thread: the download task cannot run until this test awaits.
        let mut reader = open(server.port, None).await;
        reader.seek(SeekFrom::Start(2_500_000)).unwrap();
        blocking(move || {
            let mut got = [0u8; 1000];
            reader.read_exact(&mut got).unwrap();
            assert!(got == body[2_500_000..2_501_000], "bytes from the start of the file were served as the middle");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn seeking_past_the_end_of_a_fully_buffered_file() {
        let body = pattern(50_000);
        let server = serve(body.clone(), Behavior { ranges: true, ..Default::default() });
        let mut reader = open(server.port, None).await;
        blocking(move || {
            let mut got = Vec::new();
            reader.read_to_end(&mut got).unwrap();
            assert!(got == body);
            assert_eq!(reader.seek(SeekFrom::Start(80_000)).unwrap(), 50_000);
            assert_eq!(reader.read(&mut [0u8; 16]).unwrap(), 0);
            assert!(read_at(&mut *reader, 49_000, 1000).unwrap() == body[49_000..]);
        })
        .await;
        assert_eq!(server.refused.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_connection_resumes_at_the_same_byte() {
        let body = pattern(3_000_000);
        let server = serve(body.clone(), Behavior { ranges: true, drop_first_after: Some(700_000), ..Default::default() });
        let mut reader = open(server.port, None).await;
        blocking(move || {
            let mut got = Vec::new();
            reader.read_to_end(&mut got).unwrap();
            assert_eq!(got.len(), body.len());
            assert!(got == body, "audio after the reconnect is not the same bytes");
        })
        .await;
        assert!(server.connections.load(Ordering::SeqCst) >= 2, "it never reconnected");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_chunked_file_resumes_at_the_same_byte_not_byte_zero() {
        let body = pattern(3_000_000);
        let how = Behavior { ranges: true, chunked: true, drop_first_after: Some(700_000), ..Default::default() };
        let server = serve(body.clone(), how);
        let mut reader = open(server.port, None).await;
        assert_eq!(reader.byte_len(), None);
        blocking(move || {
            let mut got = Vec::new();
            reader.read_to_end(&mut got).expect("the drop was not resumed");
            assert_eq!(got.len(), body.len());
            assert!(got == body, "bytes after the reconnect came from the wrong place");
        })
        .await;
        assert!(server.connections.load(Ordering::SeqCst) >= 2, "it never reconnected");
    }

    fn may_reconnect(seekable: bool, len: Option<u64>, live_stream: bool) -> bool {
        if seekable {
            return true;
        }
        if len.is_none() {
            return live_stream;
        }
        false
    }

    #[test]
    fn unknown_length_only_reconnects_for_live_radio() {
        assert!(!may_reconnect(false, None, false));
        assert!(may_reconnect(false, None, true));
        assert!(may_reconnect(true, Some(1_000), false));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_connection_without_range_support_is_an_error_not_a_short_song() {
        let body = pattern(3_000_000);
        let server = serve(body, Behavior { drop_first_after: Some(700_000), ..Default::default() });
        let mut reader = open(server.port, None).await;
        let result = blocking(move || {
            let mut got = Vec::new();
            reader.read_to_end(&mut got)
        })
        .await;
        assert!(result.is_err(), "a cut-off song looked like a complete one");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_server_that_ignores_range_is_reported_not_played_from_the_wrong_place() {
        let body = pattern(8_000_000);
        let server = serve(body, Behavior { ignore_ranges: true, ..Default::default() });
        let mut reader = open(server.port, None).await;
        assert!(reader.is_seekable());
        let result = blocking(move || {
            let mut first = [0u8; 10];
            reader.read_exact(&mut first).unwrap();
            read_at(&mut *reader, 7_900_000, 10)
        })
        .await;
        let err = result.expect_err("the whole file was served as if it were the middle");
        assert!(err.to_string().contains("jump"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_server_error_is_reported_when_opening() {
        let server = serve(Vec::new(), Behavior { fail_with: Some(503), ..Default::default() });
        let mut source = RangeSource::new(reqwest::Client::new(), url(server.port), HeaderMap::new());
        let Err(err) = source.create_async().await else { panic!("a 503 should not open") };
        assert!(format!("{err:?}").contains("503"), "{err:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn youtube_style_range_queries_cover_more_than_one_piece() {
        // Bigger than one 8 MB piece, so a reconnect must happen at the boundary.
        let body = pattern(9_000_000);
        let server = serve(body.clone(), Behavior::default());
        let mut reader = open(server.port, Some(body.len() as u64)).await;
        assert!(reader.is_seekable());
        blocking(move || {
            let mut got = Vec::new();
            reader.read_to_end(&mut got).unwrap();
            assert!(got == body, "pieces were stitched together wrongly");
            assert!(read_at(&mut *reader, 8_500_000, 1000).unwrap() == body[8_500_000..8_501_000]);
            assert!(read_at(&mut *reader, 10, 1000).unwrap() == body[10..1010]);
            assert_eq!(reader.seek(SeekFrom::Start(20_000_000)).unwrap(), 9_000_000);
            assert_eq!(reader.read(&mut [0u8; 16]).unwrap(), 0);
        })
        .await;
        assert!(server.connections.load(Ordering::SeqCst) >= 3);
        assert_eq!(server.refused.load(Ordering::SeqCst), 0);
    }

    /// Three minutes of a 440 Hz tone as 8 kHz mono WAV, so no encoder is needed.
    fn wav_fixture() -> Vec<u8> {
        let (rate, seconds) = (8000u32, 180u32);
        let data_len = rate * 2 * seconds;
        let mut out = Vec::with_capacity(44 + data_len as usize);
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
        for i in 0..rate * seconds {
            let wave = (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin();
            out.extend(((wave * 8000.0) as i16).to_le_bytes());
        }
        out
    }

    fn input(port: u16) -> Input {
        RangeSource::new(reqwest::Client::new(), url(port), HeaderMap::new()).into()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn audio_plays_before_the_file_finishes() {
        let audio = wav_fixture();
        let total = audio.len() as u64;
        let server = serve(audio, Behavior { pace: Duration::from_millis(5), ..Default::default() });
        let mut live = input(server.port).make_playable_async(get_codec_registry(), get_probe()).await.expect("open");
        let parsed = live.parsed_mut().expect("parsed");
        let mut frames = 0usize;
        while frames <= 4000 {
            let packet = parsed.format.next_packet().expect("packet");
            if packet.track_id() == parsed.track_id {
                frames += parsed.decoder.decode(&packet).expect("decode").frames();
            }
        }
        let got = server.sent.load(Ordering::SeqCst);
        assert!(got + 64 * 1024 < total, "playback waited for the whole file: sent {got} of {total}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn audio_seeks_forward_and_back_with_range_requests() {
        let server = serve(wav_fixture(), Behavior { ranges: true, ..Default::default() });
        let mut live = input(server.port).make_playable_async(get_codec_registry(), get_probe()).await.expect("open");
        let parsed = live.parsed_mut().expect("parsed");
        let track = parsed.track_id;
        for target in [120u64, 30, 150] {
            let landed = tokio::task::block_in_place(|| {
                parsed
                    .format
                    .seek(SeekMode::Coarse, SeekTo::Time { time: Time::from(target), track_id: Some(track) })
                    .expect("seek")
            });
            let rate = parsed.format.tracks().iter().find(|t| t.id == track).unwrap().codec_params.sample_rate.unwrap() as u64;
            let seconds = landed.actual_ts / rate;
            assert!(seconds.abs_diff(target) <= 2, "asked for {target}s, landed at {seconds}s");
            let packet = tokio::task::block_in_place(|| parsed.format.next_packet()).expect("packet after seek");
            assert!(parsed.decoder.decode(&packet).is_ok());
        }
        assert!(server.connections.load(Ordering::SeqCst) >= 2, "seeking should open new ranged connections");
        assert_eq!(server.refused.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn audio_from_a_server_without_ranges_never_lands_in_the_wrong_place() {
        let server = serve(wav_fixture(), Behavior::default());
        let mut live = input(server.port).make_playable_async(get_codec_registry(), get_probe()).await.expect("open");
        let parsed = live.parsed_mut().expect("parsed");
        let track = parsed.track_id;
        let result = tokio::task::block_in_place(|| {
            parsed.format.seek(SeekMode::Coarse, SeekTo::Time { time: Time::from(120u64), track_id: Some(track) })
        });
        // Symphonia may refuse, or read forward and discard. Either is fine; landing somewhere else is not.
        if let Ok(landed) = result {
            let rate = parsed.format.tracks().iter().find(|t| t.id == track).unwrap().codec_params.sample_rate.unwrap() as u64;
            let seconds = landed.actual_ts / rate;
            assert!(seconds.abs_diff(120) <= 2, "asked for 120s, landed at {seconds}s");
        }
    }

    /// Needs the internet: seek inside a real YouTube stream. `cargo test -- --ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn seeks_inside_a_real_youtube_stream() {
        let item = crate::media::Item {
            title: "Bustin".into(),
            source: "https://www.youtube.com/watch?v=0tdyU_gW6WE".into(),
            query: None,
            kind: crate::media::Kind::YouTube,
            length: None,
            allow_private_host: false,
            requested_by: None,
        };
        let t = std::time::Instant::now();
        let playable = crate::media::materialize(&item).await.expect("resolve");
        eprintln!("resolved {:?}", t.elapsed());
        let mut live = playable.input().make_playable_async(get_codec_registry(), get_probe()).await.expect("open");
        eprintln!("opened {:?}", t.elapsed());
        let parsed = live.parsed_mut().expect("parsed");
        let track = parsed.track_id;
        let landed = tokio::task::block_in_place(|| {
            parsed.format.seek(SeekMode::Coarse, SeekTo::Time { time: Time::from(120u64), track_id: Some(track) })
        })
        .expect("seek");
        let rate = parsed.format.tracks().iter().find(|t| t.id == track).unwrap().codec_params.sample_rate.unwrap() as u64;
        eprintln!("landed at {}s", landed.actual_ts / rate);
        assert!((landed.actual_ts / rate).abs_diff(120) <= 3);
        eprintln!("seeked {:?}", t.elapsed());
        let packet = tokio::task::block_in_place(|| parsed.format.next_packet()).expect("packet");
        assert!(parsed.decoder.decode(&packet).is_ok());
        eprintln!("decoded {:?}", t.elapsed());
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;

    #[test]
    fn memory_reader_reads_and_seeks() {
        let mut r = MemoryReader { bytes: Arc::new((0u8..10).collect()), pos: 0 };
        let mut buf = [0u8; 4];
        assert_eq!(r.read(&mut buf).unwrap(), 4);
        assert_eq!(buf, [0, 1, 2, 3]);
        assert_eq!(r.seek(SeekFrom::End(-2)).unwrap(), 8);
        assert_eq!(r.read(&mut buf).unwrap(), 2);
        assert_eq!(r.read(&mut buf).unwrap(), 0);
        assert!(r.seek(SeekFrom::Current(-20)).is_err());
        assert_eq!(r.seek(SeekFrom::Start(100)).unwrap(), 100);
        assert_eq!(r.read(&mut buf).unwrap(), 0);
        assert_eq!(r.byte_len(), Some(10));
    }
}
