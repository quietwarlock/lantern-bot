//! yt-dlp, the backup for YouTube.
//!
//! When the Innertube clients in [`crate::media`] stop working, yt-dlp can
//! still find a direct audio link. Lantern looks for `yt-dlp.exe` beside its
//! own exe, then on PATH, and downloads the official release beside the exe
//! the first time it is needed. The copy beside the exe is kept fresh with
//! `yt-dlp -U` at most once a day. Every run is a hidden, time-limited
//! `tokio::process` child, so nothing here blocks the bot.
//!
//! YouTube now hides some formats behind JavaScript that yt-dlp can only
//! solve with a JS runtime. Lantern downloads the official `deno.exe` beside
//! itself in the background after yt-dlp is first used, and passes it with
//! `--js-runtimes`. Without deno, yt-dlp still runs as before.

use serde_json::Value;
use std::ffi::OsStr;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

#[cfg(windows)]
pub const EXE_NAME: &str = "yt-dlp.exe";
#[cfg(not(windows))]
pub const EXE_NAME: &str = "yt-dlp";

const DOWNLOAD_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
#[cfg(windows)]
pub const DENO_NAME: &str = "deno.exe";
#[cfg(not(windows))]
pub const DENO_NAME: &str = "deno";
const DENO_URL: &str = "https://github.com/denoland/deno/releases/latest/download/deno-x86_64-pc-windows-msvc.zip";
/// m4a plays everywhere Lantern does. WebM/Opus (the only other YouTube
/// audio) also plays, through Symphonia's Matroska reader and Songbird's Opus decoder.
const AUDIO_FORMAT: &str = "bestaudio[ext=m4a]/bestaudio";
/// SoundCloud: a plain MP3, else AAC (HLS), else MP3 (HLS). Never Opus-in-HLS.
const SOUNDCLOUD_FORMAT: &str = "http_mp3_128/hls_aac_160k/hls_aac_96k/bestaudio[acodec^=mp4a]/hls_mp3_128/bestaudio[ext=mp3]";
/// The real thing is about 18 MB. Anything far below that is an error page.
const MIN_EXE_BYTES: usize = 1_000_000;

const AUDIO_TIMEOUT: Duration = Duration::from_secs(40);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
const LIST_TIMEOUT: Duration = Duration::from_secs(60);
const UPDATE_TIMEOUT: Duration = Duration::from_secs(120);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(180);
/// `yt-dlp -U` runs at most this often.
const UPDATE_EVERY: Duration = Duration::from_secs(24 * 3600);
/// If `-U` fails and the copy beside the exe is older than this, it is downloaded again.
const REPLACE_AFTER: Duration = Duration::from_secs(7 * 24 * 3600);
/// After a failed download, wait this long before trying again, so every song
/// doesn't sit through the same failure.
const DOWNLOAD_RETRY: Duration = Duration::from_secs(600);
const UPDATE_MARKER: &str = "lantern-ytdlp-check.txt";

static ENABLED: AtomicBool = AtomicBool::new(true);

/// The window setting "YouTube backup (yt-dlp)".
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

type Logger = Arc<dyn Fn(&str) + Send + Sync>;

fn logger() -> &'static Mutex<Option<Logger>> {
    static LOGGER: OnceLock<Mutex<Option<Logger>>> = OnceLock::new();
    LOGGER.get_or_init(|| Mutex::new(None))
}

fn log(line: &str) {
    let sink = logger().lock().unwrap_or_else(|p| p.into_inner()).clone();
    if let Some(sink) = sink {
        sink(&format!("yt-dlp: {line}"));
    }
}

/// Sends yt-dlp's own messages (download, update) to the activity log, and
/// refreshes the copy beside the exe if it is due. Call once from inside the runtime.
pub fn start(sink: impl Fn(&str) + Send + Sync + 'static) {
    *logger().lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(sink));
    if enabled() {
        let dir = crate::settings::app_dir();
        if dir.join(EXE_NAME).is_file() {
            refresh_in_background(dir.join(EXE_NAME));
        }
    }
}

// ---------------------------------------------------------------- finding it

/// `yt-dlp.exe` beside Lantern first, then each folder on PATH.
pub fn find(dir: &Path, path_var: Option<&OsStr>) -> Option<PathBuf> {
    let beside = dir.join(EXE_NAME);
    if beside.is_file() {
        return Some(beside);
    }
    std::env::split_paths(path_var?).map(|folder| folder.join(EXE_NAME)).find(|candidate| candidate.is_file())
}

/// The path to yt-dlp, downloading it first if there isn't one.
pub async fn tool() -> Result<PathBuf, String> {
    // Held for the whole lookup, so two songs don't download it twice.
    static GATE: tokio::sync::Mutex<Option<(Instant, String)>> = tokio::sync::Mutex::const_new(None);
    let mut failed = GATE.lock().await;
    let dir = crate::settings::app_dir();
    let path_var = std::env::var_os("PATH");
    if let Some(path) = find(&dir, path_var.as_deref()) {
        if path.parent() == Some(dir.as_path()) {
            refresh_in_background(path.clone());
        }
        fetch_deno_in_background(&dir, path_var.as_deref());
        return Ok(path);
    }
    if let Some((at, why)) = failed.as_ref() {
        if at.elapsed() < DOWNLOAD_RETRY {
            return Err(why.clone());
        }
    }
    match locate_or_fetch(&dir, path_var.as_deref(), |dest| async move { fetch_release(&dest).await }).await {
        Ok(path) => {
            *failed = None;
            Ok(path)
        }
        Err(err) => {
            *failed = Some((Instant::now(), err.clone()));
            Err(err)
        }
    }
}

/// Finds yt-dlp, or asks `fetch` to put it beside the exe.
pub async fn locate_or_fetch<F, Fut>(dir: &Path, path_var: Option<&OsStr>, fetch: F) -> Result<PathBuf, String>
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    if let Some(path) = find(dir, path_var) {
        return Ok(path);
    }
    log(&format!("not found beside Lantern or on PATH, downloading {EXE_NAME} (first use)"));
    let dest = dir.join(EXE_NAME);
    fetch(dest.clone()).await.map_err(|why| format!("yt-dlp isn't installed and couldn't be downloaded: {why}"))?;
    log(&format!("downloaded to {}", dest.display()));
    Ok(dest)
}

async fn fetch_release(dest: &Path) -> Result<(), String> {
    if !cfg!(windows) {
        return Err("Lantern only downloads yt-dlp.exe on Windows. Put yt-dlp on PATH.".into());
    }
    download_from(DOWNLOAD_URL, dest).await
}

/// Downloads to a temporary file beside `dest`, then renames it into place.
pub async fn download_from(url: &str, dest: &Path) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .user_agent("Lantern")
        .connect_timeout(Duration::from_secs(15))
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let response = client.get(url).send().await.map_err(|e| e.to_string())?;
    let response = response.error_for_status().map_err(|e| e.to_string())?;
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() < MIN_EXE_BYTES || !bytes.starts_with(b"MZ") {
        return Err(format!("the download wasn't a program ({} bytes)", bytes.len()));
    }
    let mut part = dest.as_os_str().to_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    if let Err(err) = tokio::fs::write(&part, &bytes).await {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(err.to_string());
    }
    if let Err(err) = tokio::fs::rename(&part, dest).await {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(err.to_string());
    }
    Ok(())
}

// --------------------------------------------------------------- staying fresh

/// `-U` is due when it has never run or last ran a day ago.
pub fn update_due(since_last_check: Option<Duration>) -> bool {
    since_last_check.is_none_or(|age| age >= UPDATE_EVERY)
}

/// A copy this old is replaced by a fresh download when `-U` fails.
pub fn replace_due(age: Option<Duration>) -> bool {
    age.is_some_and(|age| age >= REPLACE_AFTER)
}

fn age_of(path: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    SystemTime::now().duration_since(modified).ok()
}

/// Updates the copy beside Lantern.exe in the background. A yt-dlp somewhere
/// else on PATH belongs to whoever installed it, so it is left alone.
fn refresh_in_background(exe: PathBuf) {
    static BUSY: AtomicBool = AtomicBool::new(false);
    let marker = crate::settings::app_dir().join(UPDATE_MARKER);
    if !update_due(age_of(&marker)) || BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    tokio::spawn(async move {
        // Marked first, so a failure isn't retried on every song.
        let _ = std::fs::write(&marker, "");
        log("checking for an update");
        match run(&exe, &["-U"], UPDATE_TIMEOUT).await {
            Ok(out) => log(out.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("update check finished").trim()),
            Err(err) => {
                log(&format!("update check failed: {err}"));
                if replace_due(age_of(&exe)) {
                    match fetch_release(&exe).await {
                        Ok(()) => log("downloaded a fresh copy"),
                        Err(err) => log(&format!("couldn't download a fresh copy: {err}")),
                    }
                }
            }
        }
        BUSY.store(false, Ordering::Release);
    });
}

// ------------------------------------------------------------------------ deno

/// deno beside Lantern, then on PATH.
pub fn find_deno(dir: &Path, path_var: Option<&OsStr>) -> Option<PathBuf> {
    let beside = dir.join(DENO_NAME);
    if beside.is_file() {
        return Some(beside);
    }
    std::env::split_paths(path_var?).map(|folder| folder.join(DENO_NAME)).find(|candidate| candidate.is_file())
}

/// Tells yt-dlp exactly which deno to use.
pub fn js_runtime_args(deno: &Path) -> [String; 2] {
    ["--js-runtimes".into(), format!("deno:{}", deno.display())]
}

/// Downloads deno beside Lantern once, in the background. Windows only.
fn fetch_deno_in_background(dir: &Path, path_var: Option<&OsStr>) {
    static TRIED: AtomicBool = AtomicBool::new(false);
    if !cfg!(windows) || find_deno(dir, path_var).is_some() || TRIED.swap(true, Ordering::AcqRel) {
        return;
    }
    let dest = dir.join(DENO_NAME);
    tokio::spawn(async move {
        log("downloading deno (JavaScript runtime that helps yt-dlp with YouTube)");
        match fetch_deno(&dest).await {
            Ok(()) => log(&format!("deno saved to {}", dest.display())),
            Err(err) => log(&format!("couldn't download deno, yt-dlp carries on without it: {err}")),
        }
    });
}

async fn fetch_deno(dest: &Path) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .user_agent("Lantern")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client.get(DENO_URL).send().await.map_err(|e| e.to_string())?;
    let bytes = response.error_for_status().map_err(|e| e.to_string())?.bytes().await.map_err(|e| e.to_string())?;
    let exe = tokio::task::spawn_blocking(move || exe_from_zip(&bytes, "deno.exe")).await.map_err(|e| e.to_string())??;
    let mut part = dest.as_os_str().to_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    if let Err(err) = tokio::fs::write(&part, &exe).await {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(err.to_string());
    }
    tokio::fs::rename(&part, dest).await.map_err(|e| {
        let _ = std::fs::remove_file(&part);
        e.to_string()
    })
}

/// Pulls one program out of a zip archive.
pub fn exe_from_zip(zip: &[u8], name: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).map_err(|e| format!("not a zip: {e}"))?;
    let mut file = archive.by_name(name).map_err(|_| format!("{name} isn't in the zip"))?;
    let mut out = Vec::new();
    file.read_to_end(&mut out).map_err(|e| e.to_string())?;
    if out.len() < MIN_EXE_BYTES || !out.starts_with(b"MZ") {
        return Err(format!("{name} in the zip wasn't a program"));
    }
    Ok(out)
}

// --------------------------------------------------------------------- running

/// Runs yt-dlp with no console window and a time limit. A child that outlives
/// the limit is killed when the wait is dropped.
pub async fn run(exe: &Path, args: &[&str], limit: Duration) -> Result<String, String> {
    let mut command = tokio::process::Command::new(exe);
    command.args(args);
    if let Some(deno) = find_deno(&crate::settings::app_dir(), std::env::var_os("PATH").as_deref()) {
        command.args(js_runtime_args(&deno));
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PYTHONIOENCODING", "utf-8")
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let child = command.spawn().map_err(|e| format!("couldn't start yt-dlp ({}): {e}", exe.display()))?;
    let output = match tokio::time::timeout(limit, child.wait_with_output()).await {
        Ok(result) => result.map_err(|e| format!("yt-dlp failed: {e}"))?,
        Err(_) => return Err(format!("yt-dlp took longer than {} seconds", limit.as_secs())),
    };
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(failure_reason(&String::from_utf8_lossy(&output.stderr), output.status.code()))
    }
}

/// The last `ERROR:` line, without yt-dlp's `[youtube] id:` prefix.
fn failure_reason(stderr: &str, code: Option<i32>) -> String {
    let line = stderr.lines().rev().find_map(|l| l.split_once("ERROR:").map(|(_, rest)| rest.trim()));
    let Some(line) = line else {
        return format!("yt-dlp stopped with exit code {}", code.unwrap_or(-1));
    };
    let line = match (line.starts_with('['), line.find("] ")) {
        (true, Some(end)) => line[end + 2..].trim(),
        _ => line,
    };
    let line = line.split_once(": ").filter(|(id, _)| id.len() == 11).map_or(line, |(_, rest)| rest);
    line.chars().take(200).collect()
}

// --------------------------------------------------------------------- results

#[derive(Debug, PartialEq)]
pub struct Audio {
    pub url: String,
    /// Seconds, from the link's `dur=`.
    pub length: Option<u64>,
    /// Bytes, from the link's `clen=`.
    pub size: Option<u64>,
}

#[derive(Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub length: Option<u64>,
    pub channel: String,
    /// The page link, when yt-dlp gives one (SoundCloud lists do).
    pub url: Option<String>,
}

/// A direct audio link for one video.
pub async fn audio(id: &str) -> Result<Audio, String> {
    audio_with(&tool().await?, id).await
}

pub async fn audio_with(exe: &Path, id: &str) -> Result<Audio, String> {
    let page = format!("https://www.youtube.com/watch?v={id}");
    // --ignore-config keeps a user's own yt-dlp.conf from changing the output Lantern parses.
    let args = ["-f", AUDIO_FORMAT, "-g", "--no-playlist", "--no-warnings", "--ignore-config", &page];
    let out = run(exe, &args, AUDIO_TIMEOUT).await?;
    parse_audio(&out)
}

/// Output of `-g`: one link per line. YouTube's links say how big and how
/// long the audio is, so no second call is needed for that.
pub fn parse_audio(output: &str) -> Result<Audio, String> {
    let url = output
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("https://") || l.starts_with("http://"))
        .ok_or("yt-dlp didn't return an audio link")?;
    if url.contains(".m3u8") || url.contains("/manifest/") {
        return Err("yt-dlp only found a live or HLS stream, which Lantern can't play".into());
    }
    let number = |key: &str| query_value(url, key).and_then(|v| v.parse::<f64>().ok()).filter(|v| *v > 0.0);
    Ok(Audio {
        url: url.to_string(),
        length: number("dur").map(|v| v.round() as u64),
        size: number("clen").map(|v| v as u64),
    })
}

fn query_value<'a>(url: &'a str, key: &str) -> Option<&'a str> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|pair| pair.strip_prefix(key)?.strip_prefix('='))
}

/// The audio link for a SoundCloud track page. SoundCloud mostly sends HLS
/// now, so an `.m3u8` link is returned as is for Lantern to download.
pub async fn soundcloud_audio(page: &str) -> Result<String, String> {
    let exe = tool().await?;
    let args = ["-f", SOUNDCLOUD_FORMAT, "-g", "--no-playlist", "--no-warnings", "--ignore-config", page];
    let out = run(&exe, &args, AUDIO_TIMEOUT).await?;
    out.lines()
        .map(str::trim)
        .find(|l| l.starts_with("https://") || l.starts_with("http://"))
        .map(str::to_string)
        .ok_or_else(|| "yt-dlp didn't return an audio link".into())
}

/// The first `count` videos of a YouTube search.
pub async fn search(query: &str, count: usize) -> Result<Vec<Entry>, String> {
    let exe = tool().await?;
    let term = format!("ytsearch{count}:{query}");
    let args = ["--flat-playlist", "-J", "--no-warnings", "--ignore-config", &term];
    parse_entries(&run(&exe, &args, SEARCH_TIMEOUT).await?)
}

/// The videos of a playlist or mix, without opening each one.
pub async fn playlist(url: &str, limit: usize) -> Result<Vec<Entry>, String> {
    let exe = tool().await?;
    let end = limit.max(1).to_string();
    let args = ["--flat-playlist", "-J", "--playlist-end", &end, "--no-warnings", "--ignore-config", url];
    parse_entries(&run(&exe, &args, LIST_TIMEOUT).await?)
}

/// Output of `--flat-playlist -J`. Deleted and private videos and null rows are dropped.
pub fn parse_entries(output: &str) -> Result<Vec<Entry>, String> {
    let value: Value = serde_json::from_str(output).map_err(|e| format!("yt-dlp's list wasn't readable: {e}"))?;
    let rows = value["entries"].as_array().ok_or("yt-dlp returned no list")?;
    let mut entries = Vec::new();
    for row in rows {
        let (Some(id), Some(title)) = (row["id"].as_str(), row["title"].as_str()) else { continue };
        if title == "[Private video]" || title == "[Deleted video]" {
            continue;
        }
        let channel = row["channel"].as_str().or_else(|| row["uploader"].as_str()).unwrap_or("");
        entries.push(Entry {
            id: id.to_string(),
            title: title.to_string(),
            length: row["duration"].as_f64().filter(|d| *d > 0.0).map(|d| d.round() as u64),
            channel: channel.to_string(),
            url: row["url"].as_str().or_else(|| row["webpage_url"].as_str()).map(str::to_string),
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lantern-ytdlp-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const LINK: &str = "https://rr3---sn-x.googlevideo.com/videoplayback?expire=1&mime=audio%2Fmp4&clen=3376004&dur=212.061&itag=140&sig=abc";

    #[test]
    fn dash_g_output_gives_the_link_size_and_length() {
        let audio = parse_audio(&format!("{LINK}\n")).unwrap();
        assert_eq!(audio.url, LINK);
        assert_eq!(audio.size, Some(3_376_004));
        assert_eq!(audio.length, Some(212));
    }

    #[test]
    fn dash_g_output_tolerates_noise_and_plain_links() {
        let audio = parse_audio(&format!("\r\nWARNING: something\r\n  {LINK}  \r\n")).unwrap();
        assert_eq!(audio.url, LINK);
        let plain = parse_audio("https://example.com/audio.m4a\n").unwrap();
        assert_eq!(plain, Audio { url: "https://example.com/audio.m4a".into(), length: None, size: None });
        assert!(parse_audio("").is_err());
        assert!(parse_audio("ERROR: nope\n").is_err());
    }

    #[test]
    fn hls_links_are_refused() {
        let err = parse_audio("https://manifest.googlevideo.com/api/manifest/hls_playlist/id/x/index.m3u8\n").unwrap_err();
        assert!(err.contains("HLS"), "{err}");
    }

    #[test]
    fn dash_j_search_and_playlist_output_parses() {
        let json = r#"{
            "_type": "playlist", "id": "bustin", "title": "bustin",
            "entries": [
                {"id": "0tdyU_gW6WE", "title": "Bustin", "duration": 212.0, "channel": "Artist", "uploader": "Other"},
                null,
                {"id": "AAAAAAAAAAA", "title": "[Private video]", "duration": null},
                {"id": "BBBBBBBBBBB", "title": "[Deleted video]"},
                {"id": "CCCCCCCCCCC", "title": "No length", "duration": null, "uploader": "Up"},
                {"title": "No id"}
            ]
        }"#;
        let entries = parse_entries(json).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            Entry { id: "0tdyU_gW6WE".into(), title: "Bustin".into(), length: Some(212), channel: "Artist".into(), url: None }
        );
        assert_eq!(entries[1].length, None);
        assert_eq!(entries[1].channel, "Up");
    }

    #[test]
    fn dash_j_garbage_is_an_error_not_a_panic() {
        assert!(parse_entries("not json").is_err());
        assert!(parse_entries("{\"id\": \"x\"}").is_err());
        assert_eq!(parse_entries("{\"entries\": []}").unwrap(), vec![]);
    }

    #[test]
    fn errors_read_like_a_sentence() {
        let stderr = "WARNING: x\nERROR: [youtube] 0tdyU_gW6WE: Sign in to confirm you're not a bot\n";
        assert_eq!(failure_reason(stderr, Some(1)), "Sign in to confirm you're not a bot");
        assert_eq!(failure_reason("ERROR: Requested format is not available", Some(1)), "Requested format is not available");
        assert_eq!(failure_reason("", Some(3)), "yt-dlp stopped with exit code 3");
    }

    #[test]
    fn the_copy_beside_lantern_wins_over_path() {
        let beside = temp("beside");
        let on_path = temp("onpath");
        std::fs::write(on_path.join(EXE_NAME), "x").unwrap();
        let path_var = std::env::join_paths([&on_path]).unwrap();
        assert_eq!(find(&beside, Some(&path_var)), Some(on_path.join(EXE_NAME)));
        std::fs::write(beside.join(EXE_NAME), "x").unwrap();
        assert_eq!(find(&beside, Some(&path_var)), Some(beside.join(EXE_NAME)));
    }

    #[test]
    fn nothing_found_is_none() {
        let empty = temp("empty");
        let path_var = std::env::join_paths([temp("empty2")]).unwrap();
        assert_eq!(find(&empty, Some(&path_var)), None);
        assert_eq!(find(&empty, None), None);
    }

    #[tokio::test]
    async fn a_missing_binary_is_downloaded_once_and_a_failed_download_is_a_clear_error() {
        let dir = temp("fetch");
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let err = locate_or_fetch(&dir, None, |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err("no internet".to_string())
        })
        .await
        .unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(err.contains("isn't installed") && err.contains("no internet"), "{err}");

        let ok = locate_or_fetch(&dir, None, |dest| async move { std::fs::write(dest, "x").map_err(|e| e.to_string()) }).await;
        assert_eq!(ok.unwrap(), dir.join(EXE_NAME));

        // Now it exists, so nothing is fetched.
        let again = locate_or_fetch(&dir, None, |_| async { panic!("fetched although it was there") }).await;
        assert_eq!(again.unwrap(), dir.join(EXE_NAME));
    }

    #[tokio::test]
    async fn running_a_binary_that_isnt_there_fails_cleanly() {
        let err = run(Path::new("/definitely/not/here/yt-dlp"), &["-g"], Duration::from_secs(5)).await.unwrap_err();
        assert!(err.contains("couldn't start yt-dlp"), "{err}");
        assert!(audio_with(Path::new("/definitely/not/here/yt-dlp"), "0tdyU_gW6WE").await.is_err());
    }

    #[test]
    fn update_and_replace_schedules() {
        assert!(update_due(None));
        assert!(!update_due(Some(Duration::from_secs(3600))));
        assert!(update_due(Some(Duration::from_secs(25 * 3600))));
        assert!(!replace_due(None));
        assert!(!replace_due(Some(Duration::from_secs(6 * 24 * 3600))));
        assert!(replace_due(Some(Duration::from_secs(8 * 24 * 3600))));
    }

    /// Answers every request with this status and body, on localhost.
    fn serve_once_per_request(status: &'static str, body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { continue };
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf);
                let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = sock.write_all(head.as_bytes());
                let _ = sock.write_all(&body);
            }
        });
        format!("http://127.0.0.1:{port}/yt-dlp.exe")
    }

    #[tokio::test]
    async fn download_keeps_a_real_program_and_rejects_error_pages() {
        let dir = temp("download");
        let dest = dir.join(EXE_NAME);

        let mut program = vec![0u8; MIN_EXE_BYTES + 10];
        program[..2].copy_from_slice(b"MZ");
        let url = serve_once_per_request("200 OK", program.clone());
        download_from(&url, &dest).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), program);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "a .part file was left behind");

        let page = serve_once_per_request("200 OK", b"<html>rate limited</html>".to_vec());
        let other = dir.join("other.exe");
        assert!(download_from(&page, &other).await.unwrap_err().contains("wasn't a program"));
        assert!(!other.exists());

        let missing = serve_once_per_request("404 Not Found", Vec::new());
        assert!(download_from(&missing, &other).await.is_err());
        assert!(!other.exists());
    }

    #[cfg(unix)]
    fn script(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-yt-dlp");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_stand_in_yt_dlp_is_called_with_the_documented_arguments() {
        let dir = temp("script");
        let exe = script(&dir, &format!("echo \"$@\" > \"{}/args\"\necho '{LINK}'", dir.display()));
        let audio = audio_with(&exe, "0tdyU_gW6WE").await.unwrap();
        assert_eq!(audio.length, Some(212));
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        assert!(
            args.starts_with("-f bestaudio[ext=m4a]/bestaudio -g --no-playlist"),
            "{args}"
        );
        assert!(args.contains("https://www.youtube.com/watch?v=0tdyU_gW6WE"), "{args}");
    }

    #[test]
    fn deno_is_found_beside_lantern_and_passed_by_path() {
        let dir = temp("deno");
        assert!(find_deno(&dir, None).is_none());
        std::fs::write(dir.join(DENO_NAME), b"x").unwrap();
        let deno = find_deno(&dir, None).unwrap();
        let args = js_runtime_args(&deno);
        assert_eq!(args[0], "--js-runtimes");
        assert!(args[1].starts_with("deno:") && args[1].ends_with(DENO_NAME), "{}", args[1]);
    }

    #[test]
    fn deno_comes_out_of_its_zip() {
        use std::io::Write;
        let mut body = b"MZ".to_vec();
        body.resize(MIN_EXE_BYTES + 10, 0);
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            zip.start_file("deno.exe", zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(&body).unwrap();
            zip.finish().unwrap();
        }
        let bytes = buf.into_inner();
        assert_eq!(exe_from_zip(&bytes, "deno.exe").unwrap().len(), body.len());
        assert!(exe_from_zip(&bytes, "other.exe").is_err());
        assert!(exe_from_zip(b"<html>", "deno.exe").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_or_hanging_yt_dlp_is_reported_and_killed() {
        let dir = temp("script-fail");
        let failing = script(&dir, "echo 'ERROR: [youtube] 0tdyU_gW6WE: Video unavailable' >&2\nexit 1");
        assert_eq!(audio_with(&failing, "0tdyU_gW6WE").await.unwrap_err(), "Video unavailable");

        let hanging = script(&dir, "sleep 30");
        let started = Instant::now();
        let err = run(&hanging, &["x"], Duration::from_millis(300)).await.unwrap_err();
        assert!(err.contains("took longer"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// Needs the internet and a yt-dlp on PATH or beside the test binary. `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn real_yt_dlp_searches_and_lists() {
        let found = search("bustin", 4).await.expect("search");
        eprintln!("{found:?}");
        assert!(!found.is_empty() && found.len() <= 4);
        let list = playlist("https://www.youtube.com/playlist?list=PLMC9KNkIncKtPzgY-5rmhvj7fax8fdxoj", 3).await.expect("playlist");
        assert_eq!(list.len(), 3);
    }

    /// Needs the internet and a yt-dlp that YouTube lets through (not a data center IP).
    #[tokio::test]
    #[ignore]
    async fn real_yt_dlp_finds_audio() {
        let audio = audio("0tdyU_gW6WE").await.expect("audio");
        eprintln!("{:?} bytes, {:?}s, {}", audio.size, audio.length, &audio.url[..60.min(audio.url.len())]);
        assert!(audio.url.starts_with("https://"));
    }
}
