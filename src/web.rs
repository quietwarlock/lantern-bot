//! The web page for the server build (1.9): what the Windows window does, in a
//! browser. Unraid's WebUI button opens it.
//!
//! No sign-in. It listens on `LANTERN_WEB_PORT` (default 8080) inside the
//! container; keep that port on the home network. The bot token is never sent
//! to the browser, only whether one is set; a new one can be pasted in.
//! Changes go through the same paths as the window: settings are checked with
//! [`Settings::clamp`] and saved to `lantern-settings.json`, and player
//! buttons become the same [`Action`]s.
//!
//! Every change is a JSON POST. Browsers won't send JSON to another site's
//! address without asking it first, so a web page elsewhere can't press the
//! buttons for you.

use crate::bot::{self, Action};
use crate::queue::LoopMode;
use crate::settings::Settings;
use crate::ui_state::{GuildView, UiState};
use crate::webplayer::{Placement, Repeat, WebPlayer, WebQueue};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const PORT_ENV: &str = "LANTERN_WEB_PORT";
pub const DEFAULT_PORT: u16 = 8080;

const PAGE: &str = include_str!("../assets/web/index.html");
const ICON: &[u8] = include_bytes!("../assets/logo.png");

/// `LANTERN_WEB_PORT`: unset means 8080, `off` or `0` turns the page off.
pub fn port_from_env(value: Option<&str>) -> Result<Option<u16>, String> {
    let Some(text) = value.map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok(Some(DEFAULT_PORT));
    };
    if text.eq_ignore_ascii_case("off") || text == "0" {
        return Ok(None);
    }
    text.parse::<u16>().map(Some).map_err(|_| format!("{PORT_ENV}={text} isn't a port number (1-65535, or off)."))
}

/// Start and Stop from the web page, and whether the bot should be signed in.
/// The server only signs in again after a drop while this says so.
pub struct BotControl {
    handle: bot::Handle,
    want: AtomicBool,
    starts: AtomicU64,
}

impl BotControl {
    pub fn new(handle: bot::Handle) -> Self {
        Self { handle, want: AtomicBool::new(false), starts: AtomicU64::new(0) }
    }

    pub fn start(&self) {
        self.want.store(true, Ordering::Release);
        self.starts.fetch_add(1, Ordering::AcqRel);
        self.handle.start();
    }

    pub fn stop(&self) {
        self.want.store(false, Ordering::Release);
        self.handle.stop();
    }

    /// Stop trying (the token or intents are wrong) until Start is pressed.
    pub fn give_up(&self) {
        self.want.store(false, Ordering::Release);
    }

    pub fn wanted(&self) -> bool {
        self.want.load(Ordering::Acquire)
    }

    /// Bumped by every Start, so the supervisor can tell a fresh start.
    pub fn starts(&self) -> u64 {
        self.starts.load(Ordering::Acquire)
    }

    pub fn handle(&self) -> &bot::Handle {
        &self.handle
    }
}

/// Scans the music library folders on a background thread, one scan at a time.
/// A rescan asked for during a scan runs right after it.
pub struct Library {
    ui: UiState,
    index_path: PathBuf,
    scanning: AtomicBool,
    again: AtomicBool,
}

impl Library {
    pub fn new(ui: UiState, index_path: PathBuf) -> Arc<Self> {
        Arc::new(Self { ui, index_path, scanning: AtomicBool::new(false), again: AtomicBool::new(false) })
    }

    pub fn scanning(&self) -> bool {
        self.scanning.load(Ordering::Acquire)
    }

    pub fn rescan(self: &Arc<Self>) {
        if self.scanning.swap(true, Ordering::AcqRel) {
            self.again.store(true, Ordering::Release);
            return;
        }
        let me = self.clone();
        std::thread::spawn(move || loop {
            me.scan_once();
            if me.again.swap(false, Ordering::AcqRel) {
                continue;
            }
            me.scanning.store(false, Ordering::Release);
            // A request that came in between the check and the store.
            if me.again.swap(false, Ordering::AcqRel) && !me.scanning.swap(true, Ordering::AcqRel) {
                continue;
            }
            break;
        });
    }

    fn scan_once(&self) {
        use crate::library;
        let folders = self.ui.settings().library_folders;
        let mut index = if folders.is_empty() {
            library::Index::default()
        } else {
            let previous = library::with(|i| i.clone());
            library::scan(&folders, &previous)
        };
        // Folders removed while the scan ran drop out.
        let keep = self.ui.settings().library_folders;
        index.tracks.retain(|t| keep.contains(&t.folder));
        index.problems.retain(|folder, _| keep.contains(folder));
        for (folder, problem) in &index.problems {
            self.ui.log(format!("Music library: {folder}: {problem}"));
        }
        self.ui.log(format!("Music library: {} song(s)", index.tracks.len()));
        if let Err(err) = library::save(&index, &self.index_path) {
            self.ui.log(format!("Couldn't save the library index: {err}"));
        }
        library::replace(index);
    }
}

/// Everything the page's requests need.
pub struct Web {
    pub ui: UiState,
    pub control: Arc<BotControl>,
    pub library: Arc<Library>,
    pub settings_path: PathBuf,
    /// The token in memory came from `DISCORD_TOKEN`; it is never written to the file.
    pub token_from_env: bool,
    /// `LANTERN_LIBRARY`, which replaces the library folders at every container start.
    pub library_from_env: Option<String>,
    /// The browser's own player and queue (1.11).
    pub player: Arc<WebPlayer>,
    save_warning: Mutex<Option<String>>,
    temp_counter: AtomicU64,
}

impl Web {
    pub fn new(ui: UiState, control: Arc<BotControl>, library: Arc<Library>, settings_path: PathBuf, token_from_env: bool, library_from_env: Option<String>) -> Arc<Self> {
        let (player, problem) = WebPlayer::load(settings_path.with_file_name(crate::webplayer::FILE));
        if let Some(problem) = problem {
            ui.log(problem);
        }
        Arc::new(Self { ui, control, library, settings_path, token_from_env, library_from_env, player, save_warning: Mutex::new(None), temp_counter: AtomicU64::new(0) })
    }

    fn temp_dir(&self) -> PathBuf {
        let n = self.temp_counter.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("lantern-web-{}-{n}", std::process::id()))
    }
}

pub fn router(web: Arc<Web>) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/icon.png", get(icon))
        .route("/favicon.ico", get(icon))
        .route("/api/status", get(status))
        .route("/api/bot/start", post(bot_start))
        .route("/api/bot/stop", post(bot_stop))
        .route("/api/guild/{id}", post(guild_action))
        .route("/api/guild/{id}/export", get(export))
        .route("/api/settings", get(settings_get).post(settings_post))
        .route("/api/library", get(library_get))
        .route("/api/library/rescan", post(library_rescan))
        .route("/api/browse", get(browse_get))
        .route("/api/radio/search", get(radio_search))
        .route("/api/log", get(log))
        .route("/api/diagnostics", get(diagnostics))
        .route("/api/setup", get(setup))
        .route("/api/dismiss", post(dismiss))
        .route("/api/web", get(web_queue))
        .route("/api/web/add", post(web_add))
        .route("/api/web/action", post(web_action))
        .route("/api/web/search", get(web_search))
        .route("/api/web/library", get(web_library))
        .route("/api/web/stream/{id}", get(web_stream))
        .route("/api/web/art/{id}", get(web_art))
        .with_state(web)
}

/// Serves the page until the process ends.
pub async fn serve(web: Arc<Web>, port: u16) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await.map_err(|e| format!("Web page: can't use port {port}: {e}"))?;
    axum::serve(listener, router(web)).await.map_err(|e| format!("Web page stopped: {e}"))
}

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn bad(message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message.into() })))
}

fn said(message: impl Into<String>) -> Reply {
    Ok(Json(json!({ "message": message.into() })))
}

async fn page() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-store")], Html(PAGE))
}

async fn icon() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "max-age=86400")], ICON)
}

fn running(connection: &str) -> bool {
    connection != "Offline"
}

fn guild_json(g: &GuildView) -> Value {
    json!({
        // As text: Discord IDs are too big for JavaScript numbers.
        "id": g.id.to_string(),
        "name": g.name,
        "current": g.current,
        "paused": g.paused,
        "position": g.position,
        "length": g.length,
        "live": g.live,
        "upcoming": g.upcoming,
        "loop": g.loop_mode.label(),
        "autoplay": g.autoplay,
        "soundcloud_autoplay": g.soundcloud_autoplay,
        "volume": g.volume,
        "recent": g.recent.iter().map(|(at, title)| json!({ "at": at, "title": title })).collect::<Vec<_>>(),
    })
}

async fn status(State(web): State<Arc<Web>>) -> Json<Value> {
    let live = web.ui.live();
    let settings = web.ui.settings();
    let warning = {
        let mut kept = web.save_warning.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(new) = web.ui.take_save_warning() {
            *kept = Some(new);
        }
        kept.clone()
    };
    let (tracks, problems) = crate::library::with(|i| (i.tracks.len(), i.problems.len()));
    Json(json!({
        "version": crate::VERSION,
        "connection": live.connection,
        "title": live.title,
        "detail": live.detail,
        "running": running(&live.connection),
        "wanted": web.control.wanted(),
        "token_set": !settings.token.trim().is_empty(),
        "update": web.ui.update().map(|(version, link)| json!({ "version": version, "link": link })),
        "save_warning": warning,
        "library": { "tracks": tracks, "problems": problems, "scanning": web.library.scanning() },
        "guilds": web.ui.guilds().iter().map(guild_json).collect::<Vec<_>>(),
    }))
}

#[derive(Deserialize, Default)]
struct Empty {}

async fn bot_start(State(web): State<Arc<Web>>, Json(_): Json<Empty>) -> Reply {
    if web.ui.settings().token.trim().is_empty() {
        return Err(bad("Paste the bot token in Settings first."));
    }
    if running(&web.ui.live().connection) {
        return said("Already running.");
    }
    web.control.start();
    said("Starting…")
}

async fn bot_stop(State(web): State<Arc<Web>>, Json(_): Json<Empty>) -> Reply {
    web.control.stop();
    said("Stopping…")
}

#[derive(Deserialize, Debug)]
#[serde(tag = "action", rename_all = "snake_case")]
enum GuildRequest {
    Pause,
    Resume,
    Skip,
    Leave,
    Clear,
    Shuffle,
    Loop { mode: String },
    Autoplay { on: bool },
    AutoplaySoundcloud { on: bool },
    Volume { level: i32 },
    Remove { position: usize },
    Move { from: usize, to: usize },
    Play { query: String },
    AddPath { path: String },
    Replay { at: i64, title: String },
    Import { name: String, text: String },
    /// A station from the Radio tab, by its Radio Browser ID (1.10).
    Radio { uuid: String },
}

fn guild_id(text: &str) -> Result<u64, (StatusCode, Json<Value>)> {
    text.parse::<u64>().ok().filter(|id| *id > 0).ok_or_else(|| bad("That isn't a server ID."))
}

/// The library folders as real paths (symlinks resolved). Folders that are missing are left out.
fn library_roots(folders: &[String]) -> Vec<PathBuf> {
    folders.iter().filter_map(|f| Path::new(f).canonicalize().ok()).collect()
}

const OUTSIDE: &str = "Only files and folders inside a music library folder can be used.";

/// A file or folder inside one of the music library folders. Anything else is refused,
/// so the page can't list or queue files from elsewhere in the container (1.10:
/// `..` is refused outright, and a symlink that leads outside is refused too).
pub fn inside_library(path: &str, folders: &[String]) -> Result<PathBuf, String> {
    let wanted = Path::new(path.trim());
    if !wanted.is_absolute() {
        return Err("Type the full path, like /music/Artist/Album.".into());
    }
    if wanted.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(OUTSIDE.into());
    }
    // Checked before looking at the disk, so nothing is said about paths elsewhere.
    let lexically_inside = folders.iter().any(|f| wanted.starts_with(Path::new(f)));
    let roots = library_roots(folders);
    if !lexically_inside && !roots.iter().any(|root| wanted.starts_with(root)) {
        return Err(OUTSIDE.into());
    }
    let real = wanted.canonicalize().map_err(|_| format!("{} isn't there.", wanted.display()))?;
    if roots.iter().any(|root| real.starts_with(root)) {
        Ok(real)
    } else {
        Err(OUTSIDE.into())
    }
}

/// Keeps only songs whose real path is inside a library folder (a symlink in a
/// folder must not pull in a file from elsewhere).
pub fn keep_inside(items: Vec<crate::media::Item>, folders: &[String]) -> Vec<crate::media::Item> {
    let roots = library_roots(folders);
    items
        .into_iter()
        .filter(|item| Path::new(&item.source).canonicalize().is_ok_and(|real| roots.iter().any(|root| real.starts_with(root))))
        .collect()
}

/// One folder for the Browse view (1.10): folders first, then audio files, each in
/// natural order, with title/artist/length from the library index when it has
/// them. An empty `path` lists the library folders themselves.
pub fn browse(path: &str, folders: &[String]) -> Result<Value, String> {
    use crate::media::{is_audio_file, natural_cmp};
    if path.trim().is_empty() {
        let list: Vec<Value> = folders
            .iter()
            .filter(|f| Path::new(f.as_str()).canonicalize().is_ok_and(|p| p.is_dir()))
            .map(|f| json!({ "name": f, "path": f }))
            .collect();
        return Ok(json!({ "path": "", "crumbs": [], "folders": list, "files": [] }));
    }
    let real = inside_library(path, folders)?;
    if !real.is_dir() {
        return Err("That isn't a folder.".into());
    }
    let roots = library_roots(folders);
    // Breadcrumbs start at the library folder as it is configured (like /music).
    let (root_name, root_real) = folders
        .iter()
        .filter_map(|f| Path::new(f).canonicalize().ok().map(|r| (f.clone(), r)))
        .filter(|(_, r)| real.starts_with(r))
        .max_by_key(|(_, r)| r.components().count())
        .ok_or_else(|| OUTSIDE.to_string())?;
    let mut crumbs = vec![json!({ "name": root_name, "path": root_name })];
    let mut shown = PathBuf::from(&root_name);
    if let Ok(rest) = real.strip_prefix(&root_real) {
        for part in rest.components() {
            shown.push(part);
            crumbs.push(json!({ "name": part.as_os_str().to_string_lossy(), "path": shown.to_string_lossy() }));
        }
    }
    let read = std::fs::read_dir(&real).map_err(|e| format!("Couldn't open that folder: {e}"))?;
    let mut dirs: Vec<(String, PathBuf)> = Vec::new();
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let child = shown.join(&name);
        let Ok(target) = entry.path().canonicalize() else { continue };
        if !roots.iter().any(|root| target.starts_with(root)) {
            continue; // a symlink that leads outside the library
        }
        if target.is_dir() {
            dirs.push((name, child));
        } else if is_audio_file(&target) {
            files.push((name, child));
        }
    }
    dirs.sort_by(|a, b| natural_cmp(&a.0, &b.0));
    files.sort_by(|a, b| natural_cmp(&a.0, &b.0));
    let folder_text = real.to_string_lossy().into_owned();
    let shown_text = shown.to_string_lossy().into_owned();
    let tags: std::collections::HashMap<String, crate::library::Track> = crate::library::with(|index| {
        index
            .tracks
            .iter()
            .filter(|t| {
                let parent = Path::new(&t.path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                parent == folder_text || parent == shown_text
            })
            .map(|t| (Path::new(&t.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), t.clone()))
            .collect()
    });
    let files: Vec<Value> = files
        .into_iter()
        .map(|(name, child)| {
            let tag = tags.get(&name);
            json!({
                "name": name,
                "path": child.to_string_lossy(),
                "title": tag.map(|t| t.title.clone()).filter(|t| !t.is_empty()),
                "artist": tag.map(|t| t.artist.clone()).filter(|a| !a.is_empty()),
                "album": tag.map(|t| t.album.clone()).filter(|a| !a.is_empty()),
                "length": tag.and_then(|t| t.length),
            })
        })
        .collect();
    let dirs: Vec<Value> = dirs.into_iter().map(|(name, child)| json!({ "name": name, "path": child.to_string_lossy() })).collect();
    Ok(json!({ "path": shown_text, "crumbs": crumbs, "folders": dirs, "files": files }))
}

#[derive(Deserialize)]
struct BrowseQuery {
    path: Option<String>,
}

async fn browse_get(State(web): State<Arc<Web>>, Query(query): Query<BrowseQuery>) -> Reply {
    let folders = web.ui.settings().library_folders;
    let path = query.path.unwrap_or_default();
    // Reading a big folder on a network share can take a moment; keep it off the async threads.
    tokio::task::spawn_blocking(move || browse(&path, &folders))
        .await
        .map_err(|_| bad("Couldn't read that folder."))?
        .map(Json)
        .map_err(bad)
}

#[derive(Deserialize)]
struct RadioQuery {
    name: Option<String>,
    tag: Option<String>,
    country: Option<String>,
}

/// What the Radio tab shows for one station.
fn station_json(station: &crate::radiobrowser::Station) -> Value {
    json!({
        "uuid": station.stationuuid,
        "name": station.title(),
        "country": station.country.trim(),
        "countrycode": station.countrycode.trim(),
        "tags": station.tag_list(),
        "codec": station.codec.trim(),
        "bitrate": station.bitrate,
        "favicon": Some(station.favicon.trim()).filter(|f| f.starts_with("https://") || f.starts_with("http://")),
        "homepage": Some(station.homepage.trim()).filter(|h| h.starts_with("https://") || h.starts_with("http://")),
    })
}

/// Internet radio search (1.10): name, tag (genre) and country, any mix.
async fn radio_search(Query(query): Query<RadioQuery>) -> Reply {
    let field = |v: Option<String>| v.unwrap_or_default().trim().chars().take(100).collect::<String>();
    let query = crate::radiobrowser::Query { name: field(query.name), tag: field(query.tag), country: field(query.country) };
    if query.is_empty() {
        return Err(bad("Type a station name, a tag like jazz, or a country."));
    }
    let stations = crate::radiobrowser::search(&query, 30).await.map_err(|e| (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))))?;
    Ok(Json(json!({ "stations": stations.iter().map(station_json).collect::<Vec<_>>() })))
}

async fn guild_action(State(web): State<Arc<Web>>, UrlPath(id): UrlPath<String>, Json(request): Json<GuildRequest>) -> Reply {
    let id = guild_id(&id)?;
    let action = match request {
        GuildRequest::Pause => Action::Pause,
        GuildRequest::Resume => Action::Resume,
        GuildRequest::Skip => Action::Skip,
        GuildRequest::Leave => Action::Leave,
        GuildRequest::Clear => Action::Clear,
        GuildRequest::Shuffle => Action::Shuffle,
        GuildRequest::Loop { mode } => Action::Loop(LoopMode::parse(&mode).ok_or_else(|| bad("Loop is off, track, or queue."))?),
        GuildRequest::Autoplay { on } => Action::Autoplay(on),
        GuildRequest::AutoplaySoundcloud { on } => Action::AutoplaySoundCloud(on),
        GuildRequest::Volume { level } => Action::Volume(level.clamp(0, 200)),
        GuildRequest::Remove { position } => Action::Remove(position),
        GuildRequest::Move { from, to } => Action::Move(from, to),
        GuildRequest::Play { query } => {
            if query.trim().is_empty() {
                return Err(bad("Type a link or something to search for."));
            }
            Action::Play(query)
        }
        GuildRequest::Replay { at, title } => Action::Replay(at, title),
        GuildRequest::AddPath { path } => {
            let folders = web.ui.settings().library_folders;
            let real = inside_library(&path, &folders).map_err(bad)?;
            let (items, truncated) = tokio::task::spawn_blocking(move || {
                let (items, truncated) = crate::media::collect_local_audio(&[real]);
                (keep_inside(items, &folders), truncated)
            })
            .await
            .map_err(|_| bad("Couldn't read that folder."))?;
            if items.is_empty() {
                return Err(bad("No audio files there. Lantern plays mp3, m4a, ogg, opus, flac, wav, aac, and webm."));
            }
            Action::Enqueue(items, truncated)
        }
        GuildRequest::Import { name, text } => return import(&web, id, &name, &text).await,
        GuildRequest::Radio { uuid } => {
            if !crate::radiobrowser::valid_uuid(uuid.trim()) {
                return Err(bad("That isn't a station ID."));
            }
            // Looked up again by ID, so the page can't hand over some other address.
            let station = crate::radiobrowser::by_uuid(uuid.trim()).await.map_err(bad)?;
            let item = station.item().ok_or_else(|| bad("That station has no address to play."))?;
            let answer = web.control.handle().act_wait(id, Action::Enqueue(vec![item], false)).await;
            if answer.starts_with("Queued") {
                crate::radiobrowser::click(&station.stationuuid);
            }
            return said(answer);
        }
    };
    said(web.control.handle().act_wait(id, action).await)
}

/// Keeps the file name (it shows in the log and the Discord note) but nothing that could leave the folder.
fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    if cleaned.is_empty() { "playlist".into() } else { cleaned }
}

async fn import(web: &Web, id: u64, name: &str, text: &str) -> Reply {
    let name = safe_file_name(name);
    let ext = Path::new(&name).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if !crate::playlist_file::EXTENSIONS.contains(&ext.as_str()) {
        return Err(bad("Pick a .m3u, .m3u8, .pls or .xspf playlist."));
    }
    if text.len() as u64 > crate::playlist_file::MAX_FILE_BYTES {
        return Err(bad("That playlist is too big (4 MB at most)."));
    }
    let dir = web.temp_dir();
    std::fs::create_dir_all(&dir).map_err(|e| bad(format!("Couldn't read the upload: {e}")))?;
    let path = dir.join(&name);
    let written = std::fs::write(&path, text);
    let answer = match written {
        Ok(()) => web.control.handle().act_wait(id, Action::Import(path)).await,
        Err(err) => format!("Couldn't read the upload: {err}"),
    };
    let _ = std::fs::remove_dir_all(&dir);
    said(answer)
}

#[derive(Deserialize)]
struct ExportQuery {
    format: Option<String>,
}

async fn export(State(web): State<Arc<Web>>, UrlPath(id): UrlPath<String>, Query(query): Query<ExportQuery>) -> Response {
    let id = match guild_id(&id) {
        Ok(id) => id,
        Err(err) => return err.into_response(),
    };
    let ext = query.format.unwrap_or_else(|| "m3u8".into()).to_ascii_lowercase();
    if !crate::playlist_file::EXTENSIONS.contains(&ext.as_str()) {
        return bad("Export as m3u, m3u8, pls or xspf.").into_response();
    }
    let dir = web.temp_dir();
    if let Err(err) = std::fs::create_dir_all(&dir) {
        return bad(format!("Couldn't export: {err}")).into_response();
    }
    let path = dir.join(format!("Lantern queue.{ext}"));
    let answer = web.control.handle().act_wait(id, Action::Export(path.clone())).await;
    let text = std::fs::read(&path);
    let _ = std::fs::remove_dir_all(&dir);
    match text {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                (header::CONTENT_DISPOSITION, format!("attachment; filename=\"Lantern queue.{ext}\"")),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => bad(answer).into_response(),
    }
}

/// The settings for the page: every field except the token, which is only "set" or not.
pub fn settings_json(web: &Web, settings: &Settings) -> Value {
    let mut value = serde_json::to_value(settings).unwrap_or_else(|_| json!({}));
    if let Some(fields) = value.as_object_mut() {
        fields.remove("token");
        fields.insert("token_set".into(), json!(!settings.token.trim().is_empty()));
        fields.insert("token_from_env".into(), json!(web.token_from_env));
        fields.insert("library_from_env".into(), json!(web.library_from_env));
    }
    value
}

async fn settings_get(State(web): State<Arc<Web>>) -> Json<Value> {
    Json(settings_json(&web, &web.ui.settings()))
}

/// Applies the fields in `changes` to `current`, checking each one. The token
/// is only replaced by a non-empty new one. Returns the new settings and
/// whether the token was replaced.
pub fn apply_changes(current: &Settings, changes: &Value) -> Result<(Settings, bool), String> {
    let Value::Object(fields) = changes else { return Err("Send the settings as a JSON object.".into()) };
    let mut base = serde_json::to_value(current).map_err(|e| e.to_string())?;
    let mut new_token = None;
    for (key, value) in fields {
        match key.as_str() {
            "token" => match value {
                Value::String(text) if !text.trim().is_empty() => new_token = Some(text.clone()),
                Value::String(_) | Value::Null => {}
                _ => return Err("token: paste the token as text.".into()),
            },
            "token_set" | "token_from_env" | "library_from_env" => {}
            _ => {
                let Some(slot) = base.get_mut(key) else { return Err(format!("{key} isn't a Lantern setting.")) };
                *slot = value.clone();
                if let Err(err) = serde_json::from_value::<Settings>(base.clone()) {
                    return Err(format!("{key}: {err}"));
                }
            }
        }
    }
    let mut settings: Settings = serde_json::from_value(base).map_err(|e| e.to_string())?;
    let replaced = new_token.is_some();
    if let Some(token) = new_token {
        crate::setup::check_token(&token)?;
        settings.token = token;
    }
    crate::setup::check_owner(&settings.owner)?;
    settings.clamp();
    Ok((settings, replaced))
}

async fn settings_post(State(web): State<Arc<Web>>, Json(changes): Json<Value>) -> Reply {
    let current = web.ui.settings();
    let (settings, replaced) = apply_changes(&current, &changes).map_err(bad)?;
    let mut on_disk = settings.clone();
    if web.token_from_env && !replaced {
        // DISCORD_TOKEN stays in memory; the file keeps whatever it had.
        on_disk.token = Settings::load(&web.settings_path).0.token;
    }
    if let Err(err) = on_disk.save(&web.settings_path) {
        let message = format!("Couldn't save lantern-settings.json: {err}");
        web.ui.note_save_failure(message.clone());
        return Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": message }))));
    }
    let folders_changed = settings.library_folders != current.library_folders;
    web.ui.set_settings(settings.clone());
    web.ui.log(if replaced { "Settings saved (web page). New bot token: press Stop, then Start, to use it." } else { "Settings saved (web page)" });
    if folders_changed {
        web.library.rescan();
    }
    Ok(Json(json!({ "message": "Settings saved", "settings": settings_json(&web, &settings) })))
}

async fn library_get(State(web): State<Arc<Web>>) -> Json<Value> {
    let folders = web.ui.settings().library_folders;
    let (total, list) = crate::library::with(|i| {
        let list: Vec<Value> = folders.iter().map(|f| json!({ "folder": f, "tracks": i.count_in(f), "problem": i.problems.get(f) })).collect();
        (i.tracks.len(), list)
    });
    Json(json!({ "folders": list, "total": total, "scanning": web.library.scanning(), "from_env": web.library_from_env }))
}

async fn library_rescan(State(web): State<Arc<Web>>, Json(_): Json<Empty>) -> Reply {
    web.library.rescan();
    said("Scanning the music library…")
}

#[derive(Deserialize)]
struct LogQuery {
    since: Option<u64>,
}

async fn log(State(web): State<Arc<Web>>, Query(query): Query<LogQuery>) -> Json<Value> {
    let (lines, next) = web.ui.logs_since(query.since.unwrap_or(0));
    Json(json!({ "lines": lines, "next": next }))
}

async fn diagnostics(State(web): State<Arc<Web>>) -> impl IntoResponse {
    let report = crate::diagnostics::report(&web.ui.settings(), &web.ui.logs(), &web.ui.live().connection);
    web.ui.log("Diagnostics shown on the web page (token, IDs, paths and addresses removed).");
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], report)
}

async fn setup(State(web): State<Arc<Web>>) -> Json<Value> {
    let invite = crate::setup::app_id_from_token(&web.ui.settings().token).map(crate::setup::invite_url);
    Json(json!({ "portal": crate::setup::PORTAL_URL, "invite": invite }))
}

async fn dismiss(State(web): State<Arc<Web>>, Json(_): Json<Empty>) -> Reply {
    *web.save_warning.lock().unwrap_or_else(|p| p.into_inner()) = None;
    said("")
}

// ------------------------------------------------------------- web player (1.11)

fn youtube_art(item: &crate::media::Item) -> Option<String> {
    matches!(item.kind, crate::media::Kind::YouTube).then(|| crate::media::youtube_id(&item.source)).flatten().map(|id| format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg"))
}

/// What the page shows for the web queue. Library songs never show their path.
pub fn web_queue_json(queue: &WebQueue) -> Value {
    use crate::media::Kind;
    let entries: Vec<Value> = queue
        .entries
        .iter()
        .map(|e| {
            let art = match e.item.kind {
                Kind::File => Some(format!("/api/web/art/{}", e.id)),
                _ => youtube_art(&e.item),
            };
            json!({
                "id": e.id,
                "title": e.item.title,
                "source": crate::webplayer::source_label(&e.item),
                "length": e.item.length,
                "live": crate::webplayer::is_live(&e.item),
                "link": if e.item.kind == Kind::File { None } else { e.item.link() },
                "art": art,
                "error": e.error,
            })
        })
        .collect();
    json!({ "entries": entries, "current": queue.current, "repeat": queue.repeat })
}

async fn web_queue(State(web): State<Arc<Web>>) -> Json<Value> {
    Json(web_queue_json(&web.player.snapshot()))
}

fn web_reply(web: &Web, message: impl Into<String>) -> Reply {
    Ok(Json(json!({ "message": message.into(), "queue": web_queue_json(&web.player.snapshot()) })))
}

#[derive(Deserialize)]
struct WebAdd {
    query: Option<String>,
    path: Option<String>,
    uuid: Option<String>,
    mode: Option<String>,
}

/// Adds to the web queue: a link or search (the same lookup as Discord, with the
/// yt-dlp backup), a library file or folder, or a radio station by its ID.
async fn web_add(State(web): State<Arc<Web>>, Json(request): Json<WebAdd>) -> Reply {
    let placement = Placement::parse(request.mode.as_deref());
    let mut station = None;
    let items = if let Some(path) = request.path.as_deref().filter(|p| !p.trim().is_empty()) {
        let folders = web.ui.settings().library_folders;
        let real = inside_library(path, &folders).map_err(bad)?;
        let (items, _) = tokio::task::spawn_blocking(move || {
            let (items, truncated) = crate::media::collect_local_audio(&[real]);
            (keep_inside(items, &folders), truncated)
        })
        .await
        .map_err(|_| bad("Couldn't read that folder."))?;
        if items.is_empty() {
            return Err(bad("No audio files there. Lantern plays mp3, m4a, ogg, opus, flac, wav, aac, and webm."));
        }
        // Titles, artists and lengths from the library index where it has the song.
        crate::library::with(|index| {
            let known: std::collections::HashMap<&str, &crate::library::Track> = index.tracks.iter().map(|t| (t.path.as_str(), t)).collect();
            items.into_iter().map(|item| known.get(item.source.as_str()).map(|t| t.item()).unwrap_or(item)).collect()
        })
    } else if let Some(uuid) = request.uuid.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        if !crate::radiobrowser::valid_uuid(uuid) {
            return Err(bad("That isn't a station ID."));
        }
        let found = crate::radiobrowser::by_uuid(uuid).await.map_err(bad)?;
        let item = found.item().ok_or_else(|| bad("That station has no address to play."))?;
        station = Some(found.stationuuid.clone());
        vec![item]
    } else {
        let query = request.query.unwrap_or_default();
        let query = query.trim();
        if query.is_empty() {
            return Err(bad("Type a link or something to search for."));
        }
        if let Some(words) = crate::commands::local_query(query) {
            let found = crate::library::with(|index| index.find(words).map(|t| t.item()));
            vec![found.ok_or_else(|| bad(format!("Nothing in the music library matches {words}.")))?]
        } else {
            let limit = web.ui.settings().playlist_limit;
            match crate::cache::resolve(query, limit, false).await {
                Ok(items) if !items.is_empty() => items,
                Ok(_) => return Err(bad("Nothing found.")),
                Err(err) => return Err(bad(format!("Couldn't find that: {err}"))),
            }
        }
    };
    let first = items[0].title.clone();
    let added = web.player.change(|q| q.add(items, placement)).map_err(bad)?;
    if added == 0 {
        return Err(bad(format!("The web queue is full ({} songs).", crate::webplayer::MAX_ENTRIES)));
    }
    if let Some(uuid) = station {
        crate::radiobrowser::click(&uuid);
    }
    let message = match (added, placement) {
        (1, Placement::Now) => format!("Playing {first}"),
        (1, _) => format!("Added {first}"),
        (n, Placement::Now) => format!("Playing {first} and {} more", n - 1),
        (n, _) => format!("Added {n} songs"),
    };
    web_reply(&web, message)
}

#[derive(Deserialize, Debug)]
#[serde(tag = "action", rename_all = "snake_case")]
enum WebAction {
    Play { id: u64 },
    /// `ended`: the song played out (Repeat one replays it); otherwise the Next button.
    Next { #[serde(default)] ended: bool },
    Previous,
    Remove { id: u64 },
    Move { id: u64, to: usize },
    Clear,
    ClearPlayed,
    Shuffle,
    Repeat { mode: String },
}

async fn web_action(State(web): State<Arc<Web>>, Json(request): Json<WebAction>) -> Reply {
    let player = &web.player;
    let ok = match request {
        WebAction::Play { id } => player.change(|q| q.select(id)),
        WebAction::Next { ended } => player.change(|q| {
            q.next(ended);
            true
        }),
        WebAction::Previous => player.change(|q| {
            q.previous();
            true
        }),
        WebAction::Remove { id } => player.change(|q| q.remove(id)),
        WebAction::Move { id, to } => player.change(|q| q.move_to(id, to)),
        WebAction::Clear => player.change(|q| {
            q.clear();
            true
        }),
        WebAction::ClearPlayed => player.change(|q| {
            q.clear_played();
            true
        }),
        WebAction::Shuffle => player.change(|q| {
            q.shuffle();
            true
        }),
        WebAction::Repeat { mode } => {
            let mode = Repeat::parse(&mode).ok_or_else(|| bad("Repeat is off, all, or one."))?;
            player.change(|q| {
                q.repeat = mode;
                true
            })
        }
    }
    .map_err(bad)?;
    if !ok {
        return Err(bad("That song isn't in the web queue anymore."));
    }
    web_reply(&web, "")
}

#[derive(Deserialize)]
struct WordsQuery {
    q: Option<String>,
}

/// YouTube (or `sc:` SoundCloud) results to pick from.
async fn web_search(Query(query): Query<WordsQuery>) -> Reply {
    let words: String = query.q.unwrap_or_default().trim().chars().take(200).collect();
    if words.is_empty() {
        return Err(bad("Type something to search for."));
    }
    let results: Vec<Value> = if let Some(sc) = crate::media::soundcloud_search_words(&words) {
        let items = crate::media::soundcloud_hits(sc, 8).await.map_err(|e| (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))))?;
        items.iter().map(|i| json!({ "title": i.title, "link": i.source, "length": i.length, "by": "", "source": "SoundCloud", "art": null })).collect()
    } else {
        let hits = crate::cache::youtube_hits(&words, 8).await.map_err(|e| (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))))?;
        hits.iter()
            .map(|h| json!({ "title": h.item.title, "link": h.item.source, "length": h.item.length, "by": h.channel, "source": "YouTube", "art": youtube_art(&h.item) }))
            .collect()
    };
    Ok(Json(json!({ "results": results })))
}

/// Songs in the music library index matching the words.
async fn web_library(Query(query): Query<WordsQuery>) -> Json<Value> {
    let words = query.q.unwrap_or_default();
    let tracks: Vec<Value> = crate::library::with(|index| {
        index
            .search(&words, 100)
            .into_iter()
            .map(|t| json!({ "title": t.title, "artist": t.artist, "album": t.album, "length": t.length, "path": t.path }))
            .collect()
    });
    Json(json!({ "tracks": tracks }))
}

/// Opens a stream the way the Discord player does, parsed and ready to decode.
async fn open_parsed(found: &crate::media::Playable) -> Result<songbird::input::Parsed, String> {
    use songbird::input::codecs::{get_codec_registry, get_probe};
    use songbird::input::{Input, LiveInput};
    let opened = tokio::time::timeout(std::time::Duration::from_secs(45), found.input().make_playable_async(get_codec_registry(), get_probe()))
        .await
        .map_err(|_| "the stream took too long to start".to_string())?
        .map_err(|e| format!("couldn't open the stream: {e}"))?;
    match opened {
        Input::Live(LiveInput::Parsed(parsed), _) => Ok(parsed),
        _ => Err("couldn't open the stream".into()),
    }
}

#[derive(Deserialize)]
struct StreamQuery {
    /// Start this many seconds in (a seek).
    t: Option<f64>,
}

fn stream_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// The audio for one web queue entry, as Ogg Opus, from `t` seconds.
async fn web_stream(State(web): State<Arc<Web>>, UrlPath(id): UrlPath<u64>, Query(query): Query<StreamQuery>) -> Response {
    let Some(slot) = web.player.open_slot() else {
        return stream_error(StatusCode::SERVICE_UNAVAILABLE, "Too many web player streams are open.");
    };
    let Some(entry) = web.player.snapshot().entry(id).cloned() else {
        return stream_error(StatusCode::NOT_FOUND, "That song isn't in the web queue anymore.");
    };
    let live = crate::webplayer::is_live(&entry.item);
    let start = if live { 0.0 } else { query.t.unwrap_or(0.0).clamp(0.0, 24.0 * 3600.0) };
    let title = entry.item.title.clone();
    let failed = |web: &Web, why: String| {
        web.ui.log(format!("Web player: {title} didn't play: {why}"));
        web.player.note(id, Some(why.clone()), None);
        stream_error(StatusCode::BAD_GATEWAY, why)
    };
    let mut found = match web.player.playable(id, &entry.item).await {
        Ok(found) => found,
        Err(err) => return failed(&web, err),
    };
    let parsed = match open_parsed(&found).await {
        Ok(parsed) => parsed,
        Err(first) if found.youtube_id.is_some() => {
            web.player.forget(id);
            match crate::media::retry_youtube(&found).await {
                Ok(next) => {
                    found = next;
                    match open_parsed(&found).await {
                        Ok(parsed) => {
                            web.player.remember(id, &found);
                            parsed
                        }
                        Err(err) => return failed(&web, err),
                    }
                }
                Err(_) => return failed(&web, first),
            }
        }
        Err(err) => {
            web.player.forget(id);
            return failed(&web, err);
        }
    };
    web.player.note(id, None, found.length);
    if start < 0.5 {
        web.ui.log(format!("Web player: {title}{}", found.via_note()));
    }
    let gain = if web.ui.settings().level_volume { found.gain } else { 1.0 };
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let ui = web.ui.clone();
    std::thread::spawn(move || {
        let _slot = slot;
        let result = crate::transcode::run(parsed, start, gain, |bytes| tx.blocking_send(bytes).is_ok());
        if let Err(err) = result {
            ui.log(format!("Web player: {title} stopped: {err}"));
        }
    });
    let body = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|bytes| (Ok::<_, std::io::Error>(bytes::Bytes::from(bytes)), rx)) });
    (
        [(header::CONTENT_TYPE, crate::transcode::CONTENT_TYPE), (header::CACHE_CONTROL, "no-store")],
        axum::body::Body::from_stream(body),
    )
        .into_response()
}

/// Cover art for a library song: the picture in its tags, or a cover/folder image beside it.
pub fn cover_art(path: &Path) -> Option<(Vec<u8>, String)> {
    use lofty::file::TaggedFileExt;
    if let Ok(tagged) = lofty::read_from_path(path) {
        for tag in tagged.tags() {
            if let Some(picture) = tag.pictures().first() {
                let mime = picture.mime_type().map(|m| m.as_str().to_string()).unwrap_or_else(|| "image/jpeg".into());
                return Some((picture.data().to_vec(), mime));
            }
        }
    }
    let dir = path.parent()?;
    for name in ["cover.jpg", "folder.jpg", "Cover.jpg", "Folder.jpg", "cover.png", "folder.png", "front.jpg", "AlbumArt.jpg"] {
        let candidate = dir.join(name);
        if let Ok(bytes) = std::fs::read(&candidate) {
            let mime = if name.ends_with(".png") { "image/png" } else { "image/jpeg" };
            return Some((bytes, mime.into()));
        }
    }
    None
}

async fn web_art(State(web): State<Arc<Web>>, UrlPath(id): UrlPath<u64>) -> Response {
    let Some(entry) = web.player.snapshot().entry(id).cloned() else { return StatusCode::NOT_FOUND.into_response() };
    if entry.item.kind != crate::media::Kind::File {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Only songs still inside the library folders.
    let folders = web.ui.settings().library_folders;
    let Ok(real) = inside_library(&entry.item.source, &folders) else { return StatusCode::NOT_FOUND.into_response() };
    match tokio::task::spawn_blocking(move || cover_art(&real)).await.ok().flatten() {
        Some((bytes, mime)) => ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "max-age=3600".to_string())], bytes).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "MTIzNDU2Nzg5MDEyMzQ1Njc4.GabcDE.abcdefghijklmnopqrstuvwxyz0123456789ABCD";

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lantern-web-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A page on a free port with the bot not started (nothing reaches Discord).
    async fn page_for(dir: &Path, settings: Settings, token_from_env: bool) -> (String, Arc<Web>) {
        let ui = UiState::new(settings);
        let control = Arc::new(BotControl::new(bot::spawn(ui.clone())));
        let library = Library::new(ui.clone(), dir.join("lantern-library.json"));
        let web = Web::new(ui, control, library, dir.join("lantern-settings.json"), token_from_env, None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let app = router(web.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (address, web)
    }

    fn quiet_settings() -> Settings {
        Settings { token: TOKEN.into(), update_check: false, youtube_backup: false, ..Default::default() }
    }

    #[test]
    fn web_port_from_the_environment() {
        assert_eq!(port_from_env(None), Ok(Some(8080)));
        assert_eq!(port_from_env(Some(" ")), Ok(Some(8080)));
        assert_eq!(port_from_env(Some("8094")), Ok(Some(8094)));
        assert_eq!(port_from_env(Some("off")), Ok(None));
        assert_eq!(port_from_env(Some("0")), Ok(None));
        assert!(port_from_env(Some("99999")).is_err());
        assert!(port_from_env(Some("web")).is_err());
    }

    #[test]
    fn changes_are_checked_like_the_window() {
        let current = quiet_settings();
        let (s, replaced) = apply_changes(&current, &json!({ "prefix": "  ?  ", "volume": 900, "idle_minutes": 1000, "quiet": true })).unwrap();
        assert!(!replaced);
        assert_eq!(s.prefix, "?");
        assert_eq!(s.volume, 200);
        assert_eq!(s.idle_minutes, 240);
        assert!(s.quiet);
        assert_eq!(s.token, TOKEN, "the token stays when none is sent");
        let (s, _) = apply_changes(&current, &json!({ "prefix": "", "token": "" })).unwrap();
        assert_eq!(s.prefix, "!");
        assert_eq!(s.token, TOKEN, "an empty token box keeps the token");
        assert!(apply_changes(&current, &json!({ "volume": "loud" })).unwrap_err().starts_with("volume:"));
        assert!(apply_changes(&current, &json!({ "colour": "red" })).is_err());
        assert!(apply_changes(&current, &json!({ "owner": "me" })).is_err());
        assert!(apply_changes(&current, &json!({ "token": "nope" })).is_err());
        let other = TOKEN.replace("abc", "xyz");
        let (s, replaced) = apply_changes(&current, &json!({ "token": other })).unwrap();
        assert!(replaced);
        assert_eq!(s.token, other);
    }

    #[test]
    fn library_paths_stay_inside_the_library() {
        let dir = temp("inside");
        let music = dir.join("music");
        std::fs::create_dir_all(music.join("album")).unwrap();
        std::fs::write(music.join("album/a.mp3"), b"x").unwrap();
        std::fs::write(dir.join("secret.mp3"), b"x").unwrap();
        let folders = vec![music.to_string_lossy().into_owned()];
        assert!(inside_library(&music.join("album").to_string_lossy(), &folders).is_ok());
        assert!(inside_library(&music.join("album/a.mp3").to_string_lossy(), &folders).is_ok());
        assert!(inside_library(&dir.join("secret.mp3").to_string_lossy(), &folders).is_err());
        assert!(inside_library(&music.join("../secret.mp3").to_string_lossy(), &folders).is_err());
        assert!(inside_library("album/a.mp3", &folders).is_err());
        assert!(inside_library(&music.join("album").to_string_lossy(), &[]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn browse_lists_one_folder_and_never_leaves_the_library() {
        use std::os::unix::fs::symlink;
        let dir = temp("browse");
        let music = dir.join("music");
        let config = dir.join("config");
        std::fs::create_dir_all(music.join("Album 10")).unwrap();
        std::fs::create_dir_all(music.join("Album 2")).unwrap();
        std::fs::create_dir_all(music.join(".hidden")).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(music.join("b.mp3"), b"x").unwrap();
        std::fs::write(music.join("A.flac"), b"x").unwrap();
        std::fs::write(music.join("notes.txt"), b"x").unwrap();
        std::fs::write(config.join("lantern-settings.json"), b"{}").unwrap();
        std::fs::write(config.join("outside.mp3"), b"x").unwrap();
        symlink(&config, music.join("escape")).unwrap();
        symlink(config.join("outside.mp3"), music.join("escape.mp3")).unwrap();
        symlink(music.join("Album 2"), music.join("inside-link")).unwrap();
        let root = music.to_string_lossy().into_owned();
        let folders = vec![root.clone()];

        let top = browse("", &folders).unwrap();
        assert_eq!(top["folders"][0]["path"], root.as_str());
        let listing = browse(&root, &folders).unwrap();
        let names = |key: &str| listing[key].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(names("folders"), ["Album 2", "Album 10", "inside-link"], "natural order, hidden and escaping links left out");
        assert_eq!(names("files"), ["A.flac", "b.mp3"], "audio only, and the link to a file outside is left out");
        assert_eq!(listing["crumbs"][0]["name"], root.as_str());
        let deeper = browse(&music.join("Album 2").to_string_lossy(), &folders).unwrap();
        assert_eq!(deeper["crumbs"].as_array().unwrap().len(), 2);

        for bad_path in [
            config.to_string_lossy().into_owned(),
            music.join("..").join("config").to_string_lossy().into_owned(),
            music.join("Album 2/../../config").to_string_lossy().into_owned(),
            music.join("escape").to_string_lossy().into_owned(),
            "/".to_string(),
            "/etc".to_string(),
            "music".to_string(),
        ] {
            assert!(browse(&bad_path, &folders).is_err(), "{bad_path} must be refused");
            assert!(inside_library(&bad_path, &folders).is_err(), "{bad_path} must be refused");
        }
        assert!(inside_library(&music.join("escape.mp3").to_string_lossy(), &folders).is_err());
        assert!(browse(&music.join("b.mp3").to_string_lossy(), &folders).is_err(), "a file isn't a folder");
        // Nothing at all outside the library: the same answer whether it exists or not.
        assert_eq!(inside_library("/no/such/place", &folders), inside_library(&config.to_string_lossy(), &folders));

        // Queuing the whole library folder drops the file reached through the escaping link.
        let (items, _) = crate::media::collect_local_audio(std::slice::from_ref(&music));
        let kept = keep_inside(items, &folders);
        assert!(!kept.iter().any(|i| Path::new(&i.source).canonicalize().unwrap().starts_with(config.canonicalize().unwrap())));
        assert!(kept.iter().any(|i| i.source.ends_with("b.mp3")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uploaded_names_cannot_leave_the_folder() {
        assert_eq!(safe_file_name("../../etc/x.m3u"), "x.m3u");
        assert_eq!(safe_file_name(r"C:\Music\mix.pls"), "mix.pls");
        assert_eq!(safe_file_name(".hidden.m3u"), "hidden.m3u");
        assert_eq!(safe_file_name(""), "playlist");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_page_and_its_api_answer_without_signing_in() {
        let dir = temp("api");
        let (base, _web) = page_for(&dir, quiet_settings(), false).await;
        let http = reqwest::Client::new();
        let page = http.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(page.status(), 200);
        assert!(page.text().await.unwrap().contains("Lantern"));
        let icon = http.get(format!("{base}/icon.png")).send().await.unwrap();
        assert_eq!(icon.headers()[header::CONTENT_TYPE], "image/png");
        let status: Value = http.get(format!("{base}/api/status")).send().await.unwrap().json().await.unwrap();
        assert_eq!(status["connection"], "Offline");
        assert_eq!(status["running"], false);
        assert_eq!(status["token_set"], true);
        assert_eq!(status["version"], crate::VERSION);
        assert!(status["guilds"].as_array().unwrap().is_empty());
        let log: Value = http.get(format!("{base}/api/log?since=0")).send().await.unwrap().json().await.unwrap();
        assert!(!log["lines"].as_array().unwrap().is_empty());
        assert!(log["next"].as_u64().unwrap() >= 1);
        let setup: Value = http.get(format!("{base}/api/setup")).send().await.unwrap().json().await.unwrap();
        assert!(setup["invite"].as_str().unwrap().contains("client_id=123456789012345678"));
        let library: Value = http.get(format!("{base}/api/library")).send().await.unwrap().json().await.unwrap();
        assert!(library["folders"].as_array().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_token_never_reaches_the_browser() {
        let dir = temp("token");
        let (base, _web) = page_for(&dir, quiet_settings(), false).await;
        let http = reqwest::Client::new();
        for path in ["/api/status", "/api/settings", "/api/diagnostics", "/api/log", "/api/setup", "/api/library"] {
            let text = http.get(format!("{base}{path}")).send().await.unwrap().text().await.unwrap();
            assert!(!text.contains(TOKEN), "{path} sent the token");
            assert!(!text.contains("GabcDE"), "{path} sent part of the token");
        }
        let settings: Value = http.get(format!("{base}/api/settings")).send().await.unwrap().json().await.unwrap();
        assert!(settings.get("token").is_none());
        assert_eq!(settings["token_set"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_settings_save_round_trips_and_keeps_the_token() {
        let dir = temp("save");
        let (base, web) = page_for(&dir, quiet_settings(), false).await;
        let http = reqwest::Client::new();
        let answer = http.post(format!("{base}/api/settings")).json(&json!({ "quiet": true, "idle_minutes": 9, "max_per_user": 3 })).send().await.unwrap();
        assert_eq!(answer.status(), 200);
        let text = answer.text().await.unwrap();
        assert!(!text.contains(TOKEN));
        let saved = Settings::load(&dir.join("lantern-settings.json")).0;
        assert!(saved.quiet);
        assert_eq!(saved.idle_minutes, 9);
        assert_eq!(saved.max_per_user, 3);
        assert_eq!(saved.token, TOKEN, "the file keeps the token");
        assert!(web.ui.settings().quiet, "applied to the running bot");
        let back: Value = http.get(format!("{base}/api/settings")).send().await.unwrap().json().await.unwrap();
        assert_eq!(back["idle_minutes"], 9);
        let refused = http.post(format!("{base}/api/settings")).json(&json!({ "volume": "loud" })).send().await.unwrap();
        assert_eq!(refused.status(), 400);
        assert_eq!(Settings::load(&dir.join("lantern-settings.json")).0.volume, Settings::default().volume);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_token_from_the_environment_is_never_written() {
        let dir = temp("envtoken");
        let file_token = TOKEN.replace("abc", "fil");
        Settings { token: file_token.clone(), ..Default::default() }.save(&dir.join("lantern-settings.json")).unwrap();
        let (base, web) = page_for(&dir, quiet_settings(), true).await;
        let http = reqwest::Client::new();
        let answer = http.post(format!("{base}/api/settings")).json(&json!({ "prefix": "?" })).send().await.unwrap();
        assert_eq!(answer.status(), 200);
        let saved = Settings::load(&dir.join("lantern-settings.json")).0;
        assert_eq!(saved.prefix, "?");
        assert_eq!(saved.token, file_token, "DISCORD_TOKEN stays out of the file");
        assert_eq!(web.ui.settings().token, TOKEN, "and stays in use");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn changes_must_be_json_and_bad_requests_are_refused() {
        let dir = temp("json");
        let (base, _web) = page_for(&dir, quiet_settings(), false).await;
        let http = reqwest::Client::new();
        // A form post (what another site could send without asking) is refused.
        let form = http.post(format!("{base}/api/bot/stop")).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded").body("x=1").send().await.unwrap();
        assert_eq!(form.status(), 415);
        let form = http.post(format!("{base}/api/settings")).header(header::CONTENT_TYPE, "text/plain").body("{\"quiet\":true}").send().await.unwrap();
        assert_eq!(form.status(), 415);
        let bad_id = http.post(format!("{base}/api/guild/abc")).json(&json!({ "action": "skip" })).send().await.unwrap();
        assert_eq!(bad_id.status(), 400);
        let bad_loop = http.post(format!("{base}/api/guild/1")).json(&json!({ "action": "loop", "mode": "sideways" })).send().await.unwrap();
        assert_eq!(bad_loop.status(), 400);
        let unknown = http.post(format!("{base}/api/guild/1")).json(&json!({ "action": "explode" })).send().await.unwrap();
        assert!(unknown.status().is_client_error());
        let outside = http.post(format!("{base}/api/guild/1")).json(&json!({ "action": "add_path", "path": "/etc" })).send().await.unwrap();
        assert_eq!(outside.status(), 400);
        // Browse: no library folders here, so everything is outside.
        for path in ["/", "/etc", "/config", "../", "%2Fconfig"] {
            let answer = http.get(format!("{base}/api/browse")).query(&[("path", path)]).send().await.unwrap();
            assert_eq!(answer.status(), 400, "{path}");
        }
        let top: Value = http.get(format!("{base}/api/browse")).send().await.unwrap().json().await.unwrap();
        assert!(top["folders"].as_array().unwrap().is_empty());
        // Radio: an empty search and a made-up station ID are refused before anything goes online.
        let empty = http.get(format!("{base}/api/radio/search")).query(&[("name", "  ")]).send().await.unwrap();
        assert_eq!(empty.status(), 400);
        let fake = http.post(format!("{base}/api/guild/1")).json(&json!({ "action": "radio", "uuid": "../../etc/passwd" })).send().await.unwrap();
        assert_eq!(fake.status(), 400);
        let import = http.post(format!("{base}/api/guild/1")).json(&json!({ "action": "import", "name": "x.txt", "text": "" })).send().await.unwrap();
        assert_eq!(import.status(), 400);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_radio_tab_gets_only_web_links() {
        let station = crate::radiobrowser::Station {
            stationuuid: "96062a7b-0601-11e8-ae97-52543be04c81".into(),
            name: "Jazz".into(),
            favicon: "javascript:alert(1)".into(),
            homepage: "https://jazz.example".into(),
            tags: "jazz,swing".into(),
            codec: "MP3".into(),
            bitrate: 128,
            ..Default::default()
        };
        let shown = station_json(&station);
        assert_eq!(shown["favicon"], Value::Null, "only http(s) pictures");
        assert_eq!(shown["homepage"], "https://jazz.example");
        assert_eq!(shown["tags"], json!(["jazz", "swing"]));
        assert!(shown.get("url").is_none(), "the page never gets the stream address to send back");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn player_buttons_answer_when_the_bot_is_stopped() {
        let dir = temp("stopped");
        let (base, _web) = page_for(&dir, quiet_settings(), false).await;
        let http = reqwest::Client::new();
        let answer: Value = http.post(format!("{base}/api/guild/1")).json(&json!({ "action": "skip" })).send().await.unwrap().json().await.unwrap();
        assert_eq!(answer["message"], "Start the bot first.");
        let stop: Value = http.post(format!("{base}/api/bot/stop")).json(&json!({})).send().await.unwrap().json().await.unwrap();
        assert!(stop["message"].as_str().unwrap().starts_with("Stopping"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_needs_a_token() {
        let dir = temp("notoken");
        let (base, web) = page_for(&dir, Settings { update_check: false, ..Default::default() }, false).await;
        let http = reqwest::Client::new();
        let answer = http.post(format!("{base}/api/bot/start")).json(&json!({})).send().await.unwrap();
        assert_eq!(answer.status(), 400);
        assert!(!web.control.wanted());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rescan_during_a_scan_runs_after_it() {
        let dir = temp("rescan");
        let ui = UiState::new(Settings { update_check: false, ..Default::default() });
        let library = Library::new(ui.clone(), dir.join("lantern-library.json"));
        library.rescan();
        library.rescan();
        for _ in 0..200 {
            if !library.scanning() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!library.scanning());
        assert!(dir.join("lantern-library.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_web_player_queues_library_songs_and_streams_them_as_ogg_opus() {
        let dir = temp("player");
        let music = dir.join("music");
        std::fs::create_dir_all(music.join("Album")).unwrap();
        std::fs::write(music.join("Album/one.wav"), crate::transcode::test_wav(44_100, 2, 2.0)).unwrap();
        std::fs::write(music.join("Album/two.wav"), crate::transcode::test_wav(48_000, 1, 1.0)).unwrap();
        let folders = vec![music.to_string_lossy().into_owned()];
        let settings = Settings { library_folders: folders, ..quiet_settings() };
        let (base, web) = page_for(&dir, settings, false).await;
        let http = reqwest::Client::new();

        let added = http.post(format!("{base}/api/web/add")).json(&json!({ "path": music.join("Album").to_string_lossy() })).send().await.unwrap();
        assert_eq!(added.status(), 200);
        let added: Value = added.json().await.unwrap();
        assert_eq!(added["message"], "Added 2 songs");
        let queue: Value = http.get(format!("{base}/api/web")).send().await.unwrap().json().await.unwrap();
        let text = queue.to_string();
        assert!(!text.contains(&*music.to_string_lossy()), "the page never sees library paths: {text}");
        let entries = queue["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["source"], "Library");
        assert_eq!(entries[0]["live"], false);
        assert!(entries[0]["link"].is_null());
        let first = entries[0]["id"].as_u64().unwrap();
        let second = entries[1]["id"].as_u64().unwrap();
        assert_eq!(queue["current"], first);

        // The stream is Ogg Opus, from the start or from a seek.
        let stream = http.get(format!("{base}/api/web/stream/{first}")).send().await.unwrap();
        assert_eq!(stream.status(), 200);
        assert_eq!(stream.headers()[header::CONTENT_TYPE], crate::transcode::CONTENT_TYPE);
        let bytes = stream.bytes().await.unwrap();
        assert_eq!(&bytes[..4], b"OggS");
        assert!(bytes.windows(8).any(|w| w == b"OpusHead"));
        let seeked = http.get(format!("{base}/api/web/stream/{first}?t=1.5")).send().await.unwrap().bytes().await.unwrap();
        assert!(seeked.len() < bytes.len(), "a seek sends less audio");
        // Playing it didn't touch the Discord side.
        assert!(web.ui.guilds().is_empty());

        // Next / previous / pick / remove.
        let next: Value = http.post(format!("{base}/api/web/action")).json(&json!({ "action": "next", "ended": true })).send().await.unwrap().json().await.unwrap();
        assert_eq!(next["queue"]["current"], second);
        let prev: Value = http.post(format!("{base}/api/web/action")).json(&json!({ "action": "previous" })).send().await.unwrap().json().await.unwrap();
        assert_eq!(prev["queue"]["current"], first);
        let moved: Value = http.post(format!("{base}/api/web/action")).json(&json!({ "action": "move", "id": second, "to": 0 })).send().await.unwrap().json().await.unwrap();
        assert_eq!(moved["queue"]["entries"][0]["id"], second);
        let repeat: Value = http.post(format!("{base}/api/web/action")).json(&json!({ "action": "repeat", "mode": "all" })).send().await.unwrap().json().await.unwrap();
        assert_eq!(repeat["queue"]["repeat"], "all");
        let removed: Value = http.post(format!("{base}/api/web/action")).json(&json!({ "action": "remove", "id": first })).send().await.unwrap().json().await.unwrap();
        assert_eq!(removed["queue"]["entries"].as_array().unwrap().len(), 1);
        let gone = http.post(format!("{base}/api/web/action")).json(&json!({ "action": "play", "id": first })).send().await.unwrap();
        assert_eq!(gone.status(), 400);
        assert_eq!(http.get(format!("{base}/api/web/stream/{first}")).send().await.unwrap().status(), 404);

        // Saved beside the settings.
        let saved: WebQueue = serde_json::from_str(&std::fs::read_to_string(dir.join(crate::webplayer::FILE)).unwrap()).unwrap();
        assert_eq!(saved.entries.len(), 1);

        // Library search finds songs by name.
        let found: Value = http.get(format!("{base}/api/web/library?q=one")).send().await.unwrap().json().await.unwrap();
        let _ = found; // the index is only filled by a scan; the endpoint answers either way

        // Nothing outside the library, and empty adds are refused.
        let outside = http.post(format!("{base}/api/web/add")).json(&json!({ "path": dir.join("lantern-settings.json").to_string_lossy() })).send().await.unwrap();
        assert_eq!(outside.status(), 400);
        let empty = http.post(format!("{base}/api/web/add")).json(&json!({ "query": "  " })).send().await.unwrap();
        assert_eq!(empty.status(), 400);
        let station = http.post(format!("{base}/api/web/add")).json(&json!({ "uuid": "not-a-uuid" })).send().await.unwrap();
        assert_eq!(station.status(), 400);
        let clear: Value = http.post(format!("{base}/api/web/action")).json(&json!({ "action": "clear" })).send().await.unwrap().json().await.unwrap();
        assert!(clear["queue"]["entries"].as_array().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

}
