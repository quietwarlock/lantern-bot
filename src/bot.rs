//! Discord connection, per-server state, and the playback loop.
//!
//! [`spawn`] starts a background thread with its own async runtime. The window
//! talks to it through [`Handle`]. Each server gets a [`GuildState`] and, while
//! something is queued, one player task. Commands, buttons, and the window
//! all call the same operations on [`Core`], so they behave identically.

use crate::commands::Handler;
use crate::settings::routine_channel_post;
use crate::media::{self, Item, Kind};
use crate::queue::{clock, Ended, LoopMode, Queue};
use crate::store::Store;
use crate::ui_state::{GuildView, UiState};
use serenity::all::{ActivityData, ChannelId, Context, GatewayIntents, GuildId, MessageId, UserId};
use serenity::builder::{CreateMessage, EditMessage};
use serenity::cache::Cache;
use serenity::client::Client;
use serenity::gateway::ShardManager;
use serenity::http::Http;
use songbird::serenity::SerenityInit;
use songbird::tracks::{PlayMode, TrackHandle};
use songbird::Songbird;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};

/// Longest wait for a track's stream link before it is skipped. Three
/// YouTube clients can use 15 s each, and yt-dlp up to 40 s more.
const LOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// A link compared without case, query, or trailing slash, so the same SoundCloud page matches.
pub(crate) fn source_key(source: &str) -> String {
    source.split(['?', '#']).next().unwrap_or(source).trim_end_matches('/').to_ascii_lowercase()
}

/// The first song that wasn't played recently.
pub(crate) fn fresh_pick(items: Vec<Item>, recent: &HashSet<String>) -> Option<Item> {
    items.into_iter().find(|i| !recent.contains(&source_key(&i.source)))
}

/// Search words for a title: drops bracketed tags like `(Official Video)` or `[HD]`.
pub(crate) fn search_words(title: &str) -> String {
    let mut out = String::new();
    let mut depth = 0i32;
    for c in title.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let words = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if words.is_empty() { title.trim().to_string() } else { words }
}

/// Autoplay stops after this many songs in a row fail.
pub(crate) const AUTOPLAY_FAIL_LIMIT: u32 = 3;

/// A stream that stops this early in a song this long didn't really play:
/// YouTube cut it off. Treated as a failure, not a finished song.
pub(crate) fn ended_early(played_secs: u64, length: Option<u64>) -> bool {
    length.is_some_and(|len| len > 30) && played_secs < 10
}

/// How long `get_info` may take before the tick is treated as no progress.
pub(crate) const INFO_TIMEOUT: Duration = Duration::from_secs(3);

/// Watches a playing track's position so a stream that has ended (or died)
/// without songbird ever reporting it done still lets the queue advance.
#[derive(Debug, Default)]
pub(crate) struct Watchdog {
    last_ms: u64,
    best_ms: u64,
    still: Duration,
}

impl Watchdog {
    /// Silence allowed near (or past) the known end before the song counts as finished.
    pub(crate) const NEAR_END: Duration = Duration::from_secs(5);
    /// Silence allowed mid-song, after a real start, before the stream counts as dead.
    pub(crate) const DEAD: Duration = Duration::from_secs(30);

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Feeds one tick. `position_ms` is None when get_info didn't answer.
    /// Returns true when the song should be treated as finished.
    pub(crate) fn tick(&mut self, position_ms: Option<u64>, paused: bool, length_secs: Option<u64>, step: Duration) -> bool {
        if paused {
            self.still = Duration::ZERO;
            return false;
        }
        match position_ms {
            Some(ms) if ms != self.last_ms => {
                self.last_ms = ms;
                self.best_ms = self.best_ms.max(ms);
                self.still = Duration::ZERO;
            }
            _ => self.still += step,
        }
        let pos = self.best_ms / 1000;
        if let Some(len) = length_secs.filter(|l| *l > 0) {
            if pos >= len + 5 {
                return true;
            }
            if pos + 3 >= len && self.still >= Self::NEAR_END {
                return true;
            }
        }
        self.best_ms >= 10_000 && self.still >= Self::DEAD
    }
}

/// Reconnect waits for a live radio stream that dropped (1.10). After
/// [`RadioRetry::STEADY`] of good play the count starts over.
#[derive(Debug, Default)]
pub(crate) struct RadioRetry {
    tries: usize,
    last: Option<Instant>,
}

impl RadioRetry {
    pub(crate) const WAITS: [Duration; 5] =
        [Duration::from_secs(2), Duration::from_secs(5), Duration::from_secs(10), Duration::from_secs(20), Duration::from_secs(30)];
    pub(crate) const STEADY: Duration = Duration::from_secs(60);

    /// The wait before the next reconnect, or None once it keeps dropping.
    pub(crate) fn next(&mut self, now: Instant) -> Option<Duration> {
        if self.last.is_some_and(|at| now.saturating_duration_since(at) >= Self::STEADY) {
            self.tries = 0;
        }
        let wait = Self::WAITS.get(self.tries).copied()?;
        self.tries += 1;
        self.last = Some(now + wait);
        Some(wait)
    }
}

/// Why Songbird gave up on a track. The track may be gone before the next
/// poll sees `Errored`, so the reason is caught from its error event (1.10.1).
#[derive(Clone, Default)]
pub(crate) struct TrackError(Arc<std::sync::Mutex<Option<String>>>);

impl TrackError {
    fn watch(&self, handle: &TrackHandle) {
        let _ = handle.add_event(songbird::Event::Track(songbird::TrackEvent::Error), self.clone());
    }

    fn take(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).take()
    }
}

#[serenity::async_trait]
impl songbird::EventHandler for TrackError {
    async fn act(&self, ctx: &songbird::EventContext<'_>) -> Option<songbird::Event> {
        if let songbird::EventContext::Track(tracks) = ctx {
            for (state, _) in tracks.iter() {
                if let PlayMode::Errored(err) = &state.playing {
                    *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(err.to_string());
                }
            }
        }
        None
    }
}

/// Songbird's error text, said plainly when the audio couldn't be read.
pub(crate) fn plain_play_error(err: &str) -> String {
    let lower = err.to_ascii_lowercase();
    if lower.contains("probe") || lower.contains("unsupported") || lower.contains("no supported") || lower.contains("parse") || lower.contains("decod") {
        format!("Lantern can't decode this stream's audio ({err})")
    } else {
        err.to_string()
    }
}

/// Autoplay carries on from songs, not from a radio station.
pub(crate) fn autoplay_follows(last_was_radio: bool) -> bool {
    !last_was_radio
}

/// Counts songs that failed in a row. True when autoplay should stop.
pub(crate) fn note_result(streak: &mut u32, ended: Ended) -> bool {
    match ended {
        Ended::Failed => *streak += 1,
        Ended::Finished => *streak = 0,
        Ended::Skipped => {}
    }
    *streak >= AUTOPLAY_FAIL_LIMIT
}

/// Waiting songs start when one of the people who queued them joins voice.
pub(crate) fn take_pending_for(pending: &[Item], user: u64) -> bool {
    pending.iter().any(|item| item.requested_by == Some(user))
}

/// Something the window asks a server's player to do.
#[derive(Clone, Debug)]
pub enum Action {
    Pause,
    Resume,
    Skip,
    Leave,
    Remove(usize),
    Move(usize, usize),
    Clear,
    Shuffle,
    Loop(LoopMode),
    Autoplay(bool),
    /// Autoplay draws from SoundCloud (true) or YouTube (false).
    AutoplaySoundCloud(bool),
    Volume(i32),
    /// Audio files picked in the window. `truncated` means the folder had more than the limit.
    Enqueue(Vec<Item>, bool),
    /// Queue a playlist file (.m3u, .m3u8, .pls, .xspf) picked in the window.
    Import(std::path::PathBuf),
    /// Save the current song and queue to a playlist file.
    Export(std::path::PathBuf),
    /// Queue a song again from Recently played: (when it played, its title).
    Replay(i64, String),
    /// A link or search typed into the web page (1.9). `local:` words search the
    /// music library. Private-network links are refused, as for non-owners.
    Play(String),
}

/// How many recently played songs the window lists.
pub const RECENT_SHOWN: usize = 20;

enum Control {
    Start,
    Stop(Option<std::sync::mpsc::Sender<()>>),
    /// The gateway stopped on its own. Carries the session number and a plain reason.
    Ended(u64, String),
    /// The web page (1.9) waits for the answer the window only logs.
    Guild(u64, Action, Option<tokio::sync::oneshot::Sender<String>>),
}

#[derive(Clone)]
pub struct Handle {
    tx: mpsc::UnboundedSender<Control>,
}

impl Handle {
    pub fn start(&self) {
        let _ = self.tx.send(Control::Start);
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Control::Stop(None));
    }

    /// Stops the bot and waits (up to `wait`) until it has left every voice channel.
    pub fn stop_and_wait(&self, wait: Duration) {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        if self.tx.send(Control::Stop(Some(done_tx))).is_ok() {
            let _ = done_rx.recv_timeout(wait);
        }
    }

    pub fn act(&self, guild: u64, action: Action) {
        let _ = self.tx.send(Control::Guild(guild, action, None));
    }

    /// Like [`Self::act`], and returns what the window would log (1.9, web page).
    pub async fn act_wait(&self, guild: u64, action: Action) -> String {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Control::Guild(guild, action, Some(tx))).is_err() {
            return "Lantern isn't running.".into();
        }
        rx.await.unwrap_or_else(|_| "Lantern stopped before it answered.".into())
    }
}

static SESSIONS: AtomicU64 = AtomicU64::new(0);

pub fn spawn(ui: UiState) -> Handle {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let tx_thread = tx.clone();
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(err) => {
                ui.log(format!("Could not start the bot thread: {err}"));
                return;
            }
        };
        runtime.block_on(async move {
            let log = ui.clone();
            crate::ytdlp::start(move |line| log.log(line));
            if ui.settings().update_check {
                let ui = ui.clone();
                tokio::spawn(async move {
                    match crate::update::check().await {
                        Ok(Some((version, link))) => {
                            ui.log(format!("Lantern {version} is available: {link}"));
                            ui.set_update(version, link);
                        }
                        Ok(None) => {}
                        Err(err) => ui.log(format!("Couldn't check for updates ({err})")),
                    }
                });
            }
            let mut running: Option<Arc<Core>> = None;
            while let Some(msg) = rx.recv().await {
                match msg {
                    Control::Start => {
                        if running.is_some() {
                            continue;
                        }
                        match start_client(ui.clone(), tx_thread.clone()).await {
                            Ok(core) => running = Some(core),
                            Err(err) => {
                                ui.log(format!("Could not start: {err}"));
                                ui.set_live("Offline", "Nothing playing", &err);
                            }
                        }
                    }
                    Control::Stop(done) => {
                        if let Some(core) = running.take() {
                            ui.log("Stopping…");
                            core.shutdown().await;
                            ui.log("Stopped. Left every voice channel.");
                        }
                        ui.set_guilds(Vec::new());
                        ui.set_live("Offline", "Nothing playing", "Lantern is stopped.");
                        if let Some(done) = done {
                            let _ = done.send(());
                        }
                    }
                    Control::Ended(session, reason) => {
                        if running.as_ref().is_some_and(|core| core.session == session) {
                            if let Some(core) = running.take() {
                                core.alive.store(false, Ordering::Release);
                            }
                            ui.log(&reason);
                            ui.set_guilds(Vec::new());
                            ui.set_live("Offline", "Nothing playing", &reason);
                        }
                    }
                    Control::Guild(guild, action, reply) => match &running {
                        Some(core) => {
                            let core = core.clone();
                            tokio::spawn(async move {
                                let text = core.window_action(GuildId::new(guild), action).await;
                                core.ui.log(text.clone());
                                if let Some(reply) = reply {
                                    let _ = reply.send(text);
                                }
                            });
                        }
                        None => {
                            ui.log("Start the bot first.");
                            if let Some(reply) = reply {
                                let _ = reply.send("Start the bot first.".into());
                            }
                        }
                    },
                }
            }
        });
    });
    Handle { tx }
}

async fn start_client(ui: UiState, control: mpsc::UnboundedSender<Control>) -> Result<Arc<Core>, String> {
    let token = ui.settings().token.trim().to_string();
    if token.is_empty() {
        return Err("Add your bot token in Settings, then Start.".into());
    }
    if serenity::utils::token::validate(&token).is_err() {
        return Err("That doesn't look like a bot token. Copy it again from the Developer Portal → Bot → Reset Token.".into());
    }
    ui.set_live("Connecting", "Nothing playing", "Signing in to Discord…");
    ui.log("Connecting…");
    let intents = GatewayIntents::GUILDS
        | GatewayIntents::GUILD_VOICE_STATES
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::GUILD_MESSAGE_REACTIONS
        | GatewayIntents::MESSAGE_CONTENT;
    let (store, warning) = Store::open(Store::path());
    if let Some(warning) = warning {
        ui.log(warning);
    }
    let session = SESSIONS.fetch_add(1, Ordering::Relaxed) + 1;
    let songbird = Songbird::serenity();
    let http = Arc::new(Http::new(&token));
    let core = Arc::new(Core {
        session,
        ui: ui.clone(),
        http: http.clone(),
        songbird: songbird.clone(),
        cache: OnceLock::new(),
        shard: OnceLock::new(),
        ctx: std::sync::Mutex::new(None),
        guilds: Mutex::new(HashMap::new()),
        picks: Mutex::new(HashMap::new()),
        store,
        alive: AtomicBool::new(true),
        ticker: AtomicBool::new(false),
        slash_done: AtomicBool::new(false),
            resume_offered: AtomicBool::new(false),
        status: std::sync::Mutex::new(None),
    });
    let mut client = Client::builder(&token, intents)
        .event_handler(Handler { core: core.clone() })
        .register_songbird_with(songbird)
        .await
        .map_err(|e| e.to_string())?;
    let _ = core.cache.set(client.cache.clone());
    let _ = core.shard.set(client.shard_manager.clone());
    tokio::spawn(async move {
        let reason = match client.start().await {
            Ok(()) => "Discord connection closed.".to_string(),
            Err(err) => explain_gateway_error(&err),
        };
        let _ = control.send(Control::Ended(session, reason));
    });
    Ok(core)
}

/// Turns a gateway error into something the owner can act on.
fn explain_gateway_error(err: &serenity::Error) -> String {
    use serenity::gateway::GatewayError;
    match err {
        serenity::Error::Gateway(GatewayError::InvalidAuthentication) => {
            "Discord rejected the bot token. Developer Portal → your app → Bot → Reset Token, paste the new token here, Save, then Start.".into()
        }
        serenity::Error::Gateway(GatewayError::DisallowedGatewayIntents) => {
            "Discord refused the Message Content intent. Developer Portal → your app → Bot → turn on Message Content Intent, then Start.".into()
        }
        serenity::Error::Gateway(GatewayError::InvalidGatewayIntents) => {
            "Discord said the requested intents are invalid. Check Developer Portal → Bot → Privileged Gateway Intents.".into()
        }
        other => format!("Discord disconnected: {other}. Press Start to try again."),
    }
}

/// The track that is loading or playing in one server.
#[derive(Clone)]
pub(crate) struct Now {
    pub item: Item,
    pub title: String,
    pub length: Option<u64>,
    pub loading: bool,
    pub requested_in: Option<ChannelId>,
}

#[derive(Default)]
pub(crate) struct GuildState {
    pub queue: Queue,
    pub now: Option<Now>,
    pub handle: Option<TrackHandle>,
    pub paused: bool,
    pub position: u64,
    /// A player task is running for this server.
    pub worker: bool,
    /// Bumped by stop and leave, so a track that was loading at that moment is dropped.
    pub epoch: u64,
    /// Skip was pressed for the current track (loading or playing).
    pub skip_requested: bool,
    /// Where `play` was last used. Now-playing cards and errors go here.
    pub text_channel: Option<ChannelId>,
    pub np_message: Option<(ChannelId, MessageId)>,
    pub last_youtube: Option<String>,
    /// The last SoundCloud page played, for SoundCloud autoplay.
    pub last_soundcloud: Option<String>,
    /// Title of the last song played (any source), for the autoplay search fallback.
    pub last_title: Option<String>,
    /// The last song played came from SoundCloud.
    pub last_was_soundcloud: bool,
    /// The last thing played was a live radio station (autoplay doesn't follow one).
    pub last_was_radio: bool,
    pub idle_since: Option<Instant>,
    /// Volume leveling for the current song (see [`media::Playable::gain`]).
    pub gain: Option<f32>,
    /// Sleep timer: bumped to cancel a running one.
    pub sleep_token: u64,
    /// When the sleep timer fires (for `sleep` with no argument).
    pub sleep_at: Option<Instant>,
    /// Sleep timer set to "end of this song": leave when it finishes.
    pub sleep_after_song: bool,
    /// Who voted to skip the current song (cleared when the next one starts).
    pub skip_votes: HashSet<u64>,
    /// When Lantern last left on purpose, so the "disconnected" notice isn't sent for it.
    pub left_at: Option<Instant>,
    /// Songs queued while Lantern wasn't in voice (1.7). They start when
    /// someone who queued one joins a voice channel.
    pub pending: Vec<Item>,
    /// The voice connection died and hasn't come back yet (1.8.3).
    pub voice_down: bool,
    /// A rejoin task is running for this server.
    pub rejoining: bool,
    /// Lantern paused the song because voice dropped (not the listener's pause).
    pub voice_paused: bool,
    /// The `Call` that has [`crate::voice::VoiceWatch`] attached (its address; 0 = none).
    pub watched_call: usize,
    /// Periodic checks in a row that found Lantern missing from voice while playing.
    pub voice_missing: u8,
}

pub(crate) struct PendingPick {
    pub channel: ChannelId,
    pub message: Option<MessageId>,
    pub count: usize,
    pub tx: tokio::sync::oneshot::Sender<Option<usize>>,
}

/// Everything one bot session shares: Discord handles, server states, saved data.
pub(crate) struct Core {
    pub session: u64,
    pub ui: UiState,
    pub http: Arc<Http>,
    pub songbird: Arc<Songbird>,
    pub cache: OnceLock<Arc<Cache>>,
    pub shard: OnceLock<Arc<ShardManager>>,
    pub ctx: std::sync::Mutex<Option<Context>>,
    pub guilds: Mutex<HashMap<u64, GuildState>>,
    pub picks: Mutex<HashMap<UserId, PendingPick>>,
    pub store: Store,
    pub alive: AtomicBool,
    pub ticker: AtomicBool,
    /// Slash commands were sent to Discord this session.
    pub slash_done: AtomicBool,
    /// The restart offer for kept queues was made this session.
    pub resume_offered: AtomicBool,
    /// The "Listening to" status last sent. `None`: not sent yet (or Discord
    /// reconnected), so the next update always goes out.
    pub status: std::sync::Mutex<Option<Option<String>>>,
}

impl Core {
    pub fn note_data_save_failed(&self, reason: &str) {
        self.ui.note_save_failure(format!("Couldn't save lantern-data.json: {reason}"));
    }

    pub fn volume_for(&self, guild: GuildId) -> i32 {
        self.store.guild(guild.get()).volume.unwrap_or_else(|| self.ui.settings().volume)
    }

    /// The song's leveling gain, or 1.0 when leveling is off in Settings.
    fn leveling(&self, gain: f32) -> f32 {
        if self.ui.settings().level_volume { gain } else { 1.0 }
    }

    /// What Songbird plays at: the server's volume times the song's leveling.
    fn level_for(&self, guild: GuildId, gain: f32) -> f32 {
        self.volume_for(guild) as f32 / 100.0 * self.leveling(gain)
    }

    /// Keeps the bot's "Listening to" status in step with what's playing.
    /// Sent only when it changes, so the per-second refresh doesn't flood Discord.
    /// Only `song_status` applies. Quiet does not turn this off.
    fn set_activity(&self, wanted: Option<String>) {
        let wanted = presence_activity(self.ui.settings().song_status, wanted);
        {
            let mut last = self.status.lock().unwrap_or_else(|p| p.into_inner());
            if last.as_ref() == Some(&wanted) {
                return;
            }
            let ctx = self.ctx.lock().unwrap_or_else(|p| p.into_inner()).clone();
            let Some(ctx) = ctx else { return };
            *last = Some(wanted.clone());
            ctx.set_activity(wanted.as_deref().map(ActivityData::listening));
        }
    }

    /// Forget the sent status, after Discord reconnects.
    pub fn reset_status(&self) {
        *self.status.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    pub async fn say(&self, channel: Option<ChannelId>, text: &str) {
        if let Some(channel) = channel {
            let _ = channel
                .send_message(&self.http, CreateMessage::new().embed(crate::commands::note_embed(text)))
                .await;
        }
    }

    pub(crate) async fn text_channel(&self, guild: GuildId) -> Option<ChannelId> {
        self.guilds.lock().await.get(&guild.get()).and_then(|s| s.text_channel)
    }

    /// Someone joined a voice channel while Lantern isn't in one. If they
    /// queued songs before (1.7), join them there and start playing.
    pub async fn start_pending(self: &Arc<Self>, guild: GuildId, channel: ChannelId, user: u64) {
        let (items, text) = {
            let mut guilds = self.guilds.lock().await;
            let Some(state) = guilds.get_mut(&guild.get()) else { return };
            if !take_pending_for(&state.pending, user) {
                return;
            }
            (std::mem::take(&mut state.pending), state.text_channel)
        };
        match self.songbird.join(guild, channel).await {
            Ok(call) => {
                let _ = call.lock().await.deafen(true).await;
            }
            Err(err) => {
                self.ui.log(format!("Couldn't join voice for the waiting songs: {err}"));
                self.guilds.lock().await.entry(guild.get()).or_default().pending = items;
                return;
            }
        }
        let count = items.len();
        self.ui.log(format!("Joined voice for {count} waiting song(s)"));
        self.push(guild, items, text).await;
        self.say(text, &crate::i18n::tf("joined_for_queue", &[&count.to_string()])).await;
    }

    /// Adds tracks and starts the player if it isn't running.
    pub async fn push(self: &Arc<Self>, guild: GuildId, items: Vec<Item>, text: Option<ChannelId>) -> usize {
        let start = {
            let mut guilds = self.guilds.lock().await;
            let state = guilds.entry(guild.get()).or_default();
            if text.is_some() {
                state.text_channel = text;
            }
            state.queue.push(items);
            state.idle_since = None;
            let start = !state.worker;
            state.worker = true;
            (start, state.queue.len())
        };
        if start.0 {
            let core = self.clone();
            tokio::spawn(async move { core.player(guild).await });
        }
        self.publish().await;
        start.1
    }

    async fn player(self: Arc<Self>, guild: GuildId) {
        loop {
            let exit = self.play_queue(guild).await;
            if self.player_exit(guild, exit).await {
                return;
            }
        }
    }

    /// Tidies up after the queue loop stops. Returns false when songs arrived
    /// meanwhile and the loop should run again.
    async fn player_exit(&self, guild: GuildId, exit: Exit) -> bool {
        let (still_here, restart) = {
            let mut guilds = self.guilds.lock().await;
            let state = guilds.entry(guild.get()).or_default();
            state.now = None;
            state.handle = None;
            state.paused = false;
            state.position = 0;
            state.idle_since = Some(Instant::now());
            // Songs added while this player was shutting down would otherwise
            // sit in the queue with nobody to play them.
            let restart = !state.queue.is_empty() && self.alive.load(Ordering::Acquire) && self.songbird.get(guild).is_some();
            state.worker = restart;
            (exit == Exit::QueueEnded, restart)
        };
        if restart {
            return false;
        }
        self.clear_np(guild).await;
        if still_here && !self.ui.settings().stay_in_channel {
            self.leave(guild).await;
        }
        self.publish().await;
        true
    }

    async fn play_queue(self: &Arc<Self>, guild: GuildId) -> Exit {
        let mut fail_streak = 0u32;
        loop {
            if !self.alive.load(Ordering::Acquire) {
                return Exit::Cancelled;
            }
            // The epoch is read before anything slow, so a stop during an autoplay
            // lookup is noticed instead of the lookup's song starting anyway.
            let (next, epoch, text) = {
                let mut guilds = self.guilds.lock().await;
                let Some(state) = guilds.get_mut(&guild.get()) else { return Exit::Cancelled };
                if state.sleep_after_song {
                    state.sleep_after_song = false;
                    let text = state.text_channel;
                    drop(guilds);
                    let core = self.clone();
                    tokio::spawn(async move { core.sleep_now(guild, text).await });
                    return Exit::Cancelled;
                }
                (state.queue.pop_next(), state.epoch, state.text_channel)
            };
            let item = match next {
                Some(item) => item,
                None if fail_streak >= AUTOPLAY_FAIL_LIMIT => {
                    self.ui.log(format!("Autoplay stopped: {fail_streak} songs in a row failed to play."));
                    self.say(text, "Autoplay stopped: the last few songs wouldn't play. Queue a song to carry on.").await;
                    return Exit::QueueEnded;
                }
                None => match self.autoplay_pick(guild).await {
                    Some(item) => item,
                    None => return Exit::QueueEnded,
                },
            };
            {
                let mut guilds = self.guilds.lock().await;
                let Some(state) = guilds.get_mut(&guild.get()) else { return Exit::Cancelled };
                if state.epoch != epoch {
                    return Exit::Cancelled;
                }
                state.now = Some(Now {
                    item: item.clone(),
                    title: item.title.clone(),
                    length: item.length,
                    loading: true,
                    requested_in: text,
                });
                state.skip_requested = false;
                state.skip_votes.clear();
                state.handle = None;
                state.paused = false;
                state.position = 0;
                state.idle_since = None;
            }
            self.publish().await;

            // Skip and Stop answer at once, even while a slow lookup is running.
            let loaded = match self.unless_skipped(guild, epoch, tokio::time::timeout(LOAD_TIMEOUT, media::materialize(&item))).await {
                Some(Ok(result)) => result,
                Some(Err(_)) => Err("it took too long to load".into()),
                None => Err("skipped".into()),
            };
            let skipped = {
                let guilds = self.guilds.lock().await;
                let Some(state) = guilds.get(&guild.get()) else { return Exit::Cancelled };
                if state.epoch != epoch {
                    return Exit::Cancelled;
                }
                state.skip_requested
            };
            if skipped {
                self.finish(guild, item, Ended::Skipped).await;
                continue;
            }
            let mut playable = match loaded {
                Ok(playable) => playable,
                Err(err) => {
                    let (log, discord) = play_failure(item.kind == Kind::File, &item.title, &item.source, &err);
                    self.ui.log(log);
                    self.say(text, &discord).await;
                    note_result(&mut fail_streak, Ended::Failed);
                    self.finish(guild, item, Ended::Failed).await;
                    continue;
                }
            };
            // 1.8.3: never start a song into a dead voice connection.
            if !self.voice_ready(guild, epoch).await {
                return Exit::Cancelled;
            }
            let Some(call) = self.songbird.get(guild) else { return Exit::Cancelled };
            let mut handle = call.lock().await.play_only_input(playable.input());
            let track_error = TrackError::default();
            track_error.watch(&handle);
            let _ = handle.set_volume(self.level_for(guild, playable.gain));
            let title = playable.title.clone();
            {
                let mut guilds = self.guilds.lock().await;
                let Some(state) = guilds.get_mut(&guild.get()) else {
                    let _ = handle.stop();
                    return Exit::Cancelled;
                };
                if state.epoch != epoch {
                    let _ = handle.stop();
                    return Exit::Cancelled;
                }
                state.handle = Some(handle.clone());
                state.gain = Some(playable.gain);
                state.now = Some(Now {
                    item: item.clone(),
                    title: title.clone(),
                    length: playable.length.or(item.length),
                    loading: false,
                    requested_in: text,
                });
                if playable.youtube_id.is_some() {
                    state.last_youtube = playable.youtube_id.clone();
                }
                state.last_was_soundcloud = item.kind == Kind::SoundCloud;
                state.last_was_radio = item.kind == Kind::Radio;
                if state.last_was_soundcloud {
                    state.last_soundcloud = Some(item.source.clone());
                }
                state.last_title = Some(title.clone());
            }
            let mut played = item.clone();
            if played.kind == Kind::YouTube {
                played.title = title.clone();
            }
            if let Some(err) = self.store.add_history(guild.get(), &played) {
                self.note_data_save_failed(&err);
            }
            self.ui.log(format!("Playing {title}{}", playable.via_note()));
            self.announce_np(guild).await;
            self.publish().await;

            let mut ticks = 0u32;
            let mut retried_early_end = false;
            let mut last_position = 0u64;
            let mut watchdog = Watchdog::default();
            let mut radio_retry = RadioRetry::default();
            let live = item.kind == Kind::Radio;
            // Some audio has played (the position moved), so a stop is a drop, not a bad stream.
            let mut heard = false;
            let step = Duration::from_millis(250);
            let ended = loop {
                tokio::time::sleep(step).await;
                ticks = (ticks + 1) % 4;
                // Skip / stop / leave must win on every tick, not only once the track reports done.
                if self.guilds.lock().await.get(&guild.get()).is_none_or(|s| s.skip_requested || s.epoch != epoch) {
                    let _ = handle.stop();
                    break Ended::Finished;
                }
                let info = match tokio::time::timeout(INFO_TIMEOUT, handle.get_info()).await {
                    Ok(info) => info,
                    Err(_) => {
                        if watchdog.tick(None, false, playable.length.or(item.length), INFO_TIMEOUT) {
                            let _ = handle.stop();
                            if live {
                                if let Some((next, started)) = self.reconnect_radio(guild, epoch, &call, &item, &mut radio_retry, "it stopped answering").await {
                                    playable = next;
                                    handle = started;
                                    track_error.watch(&handle);
                                    watchdog.reset();
                                    continue;
                                }
                                break self.radio_gave_up(text, &title).await;
                            }
                            self.ui.log(format!("{title} stopped answering; moving on"));
                            break Ended::Finished;
                        }
                        continue;
                    }
                };
                match info {
                    Ok(info) => match &info.playing {
                        PlayMode::Errored(err) => {
                            // Refused before any audio played: try the next YouTube route.
                            if info.position.is_zero() {
                                if let Some((next, started)) = self.next_route(guild, epoch, &call, &playable, &err.to_string()).await {
                                    playable = next;
                                    handle = started;
                                    track_error.watch(&handle);
                                    watchdog.reset();
                                    continue;
                                }
                            }
                            if live && (last_position > 0 || !info.position.is_zero()) {
                                let why = err.to_string();
                                if let Some((next, started)) = self.reconnect_radio(guild, epoch, &call, &item, &mut radio_retry, &why).await {
                                    playable = next;
                                    handle = started;
                                    track_error.watch(&handle);
                                    watchdog.reset();
                                    continue;
                                }
                                if !self.guilds.lock().await.get(&guild.get()).is_some_and(|s| s.epoch == epoch && !s.skip_requested) {
                                    break Ended::Finished;
                                }
                            }
                            let (log, discord) = play_failure(item.kind == Kind::File, &title, &item.source, &plain_play_error(&err.to_string()));
                            self.ui.log(log);
                            self.say(text, &discord).await;
                            break Ended::Failed;
                        }
                        mode if mode.is_done() => {
                            let played = info.position.as_secs().max(last_position);
                            let length = playable.length.or(item.length);
                            let skipped = self.guilds.lock().await.get(&guild.get()).is_none_or(|s| s.skip_requested || s.epoch != epoch);
                            if live && !skipped && !heard {
                                let why = track_error.take().map_or_else(|| "the station sent no audio Lantern could play".to_string(), |e| plain_play_error(&e));
                                break self.track_failed(text, &item, &title, &why).await;
                            }
                            if live && !skipped {
                                // A live stream never really ends: it dropped.
                                if let Some((next, started)) = self.reconnect_radio(guild, epoch, &call, &item, &mut radio_retry, "the stream ended").await {
                                    playable = next;
                                    handle = started;
                                    track_error.watch(&handle);
                                    watchdog.reset();
                                    continue;
                                }
                                break self.radio_gave_up(text, &title).await;
                            }
                            if skipped || !ended_early(played, length) {
                                break Ended::Finished;
                            }
                            let why = format!("the stream ended after {played}s of {}", clock(length.unwrap_or(0)));
                            if playable.via.is_some() {
                                // Cached YouTube session data may be stale; start fresh.
                                media::forget_youtube_session();
                                if !retried_early_end {
                                    retried_early_end = true;
                                    if let Some((next, started)) = self.next_route(guild, epoch, &call, &playable, &why).await {
                                        playable = next;
                                        handle = started;
                                        track_error.watch(&handle);
                                        last_position = 0;
                                        watchdog.reset();
                                        continue;
                                    }
                                }
                            }
                            let (log, discord) = play_failure(item.kind == Kind::File, &title, &item.source, &why);
                            self.ui.log(log);
                            self.say(text, &discord).await;
                            break Ended::Failed;
                        }
                        mode => {
                            let paused = matches!(mode, PlayMode::Pause);
                            let length = playable.length.or(item.length);
                            if watchdog.tick(Some(info.position.as_millis() as u64), paused, length, step) {
                                let _ = handle.stop();
                                if live && !heard {
                                    let why = track_error.take().map_or_else(|| "the station sent no audio Lantern could play".to_string(), |e| plain_play_error(&e));
                                    break self.track_failed(text, &item, &title, &why).await;
                                }
                                if live {
                                    if let Some((next, started)) = self.reconnect_radio(guild, epoch, &call, &item, &mut radio_retry, "no sound for 30 s").await {
                                        playable = next;
                                        handle = started;
                                        track_error.watch(&handle);
                                        watchdog.reset();
                                        continue;
                                    }
                                    break self.radio_gave_up(text, &title).await;
                                }
                                self.ui.log(format!("{title} stopped at {} of {}; moving on", clock(info.position.as_secs()), clock(length.unwrap_or(0))));
                                break Ended::Finished;
                            }
                            last_position = info.position.as_secs();
                            heard |= !info.position.is_zero();
                            let mut guilds = self.guilds.lock().await;
                            if let Some(state) = guilds.get_mut(&guild.get()) {
                                state.position = info.position.as_secs();
                            }
                        }
                    },
                    // Songbird dropped the track: it ended, or it failed before a tick saw why (1.10.1).
                    Err(_) => {
                        if self.guilds.lock().await.get(&guild.get()).is_none_or(|s| s.skip_requested || s.epoch != epoch) {
                            break Ended::Finished;
                        }
                        let error = track_error.take();
                        if live && heard {
                            let why = error.map_or_else(|| "the stream ended".to_string(), |e| plain_play_error(&e));
                            if let Some((next, started)) = self.reconnect_radio(guild, epoch, &call, &item, &mut radio_retry, &why).await {
                                playable = next;
                                handle = started;
                                track_error.watch(&handle);
                                watchdog.reset();
                                continue;
                            }
                            break self.radio_gave_up(text, &title).await;
                        }
                        match error {
                            Some(err) => break self.track_failed(text, &item, &title, &plain_play_error(&err)).await,
                            None if live => break self.track_failed(text, &item, &title, "the station sent no audio Lantern could play").await,
                            None => break Ended::Finished,
                        }
                    }
                }
                if ticks == 0 {
                    self.publish().await;
                }
            };
            let (cancelled, skipped) = {
                let guilds = self.guilds.lock().await;
                match guilds.get(&guild.get()) {
                    Some(state) => (state.epoch != epoch, state.skip_requested),
                    None => (true, false),
                }
            };
            if cancelled {
                return Exit::Cancelled;
            }
            let ended = if ended == Ended::Finished && skipped { Ended::Skipped } else { ended };
            note_result(&mut fail_streak, ended);
            self.finish(guild, item, ended).await;
        }
    }

    /// A live radio stream that dropped is opened again after a short wait
    /// ([`RadioRetry`]), unless Skip or Stop comes first.
    async fn reconnect_radio(
        self: &Arc<Self>,
        guild: GuildId,
        epoch: u64,
        call: &Arc<Mutex<songbird::Call>>,
        item: &Item,
        retry: &mut RadioRetry,
        why: &str,
    ) -> Option<(media::Playable, TrackHandle)> {
        let current = |state: &GuildState| state.epoch == epoch && !state.skip_requested;
        let mut why = why.to_string();
        loop {
            if !self.guilds.lock().await.get(&guild.get()).is_some_and(current) {
                return None;
            }
            let Some(wait) = retry.next(Instant::now()) else {
                self.ui.log(format!("{} kept dropping; giving up ({why})", item.title));
                return None;
            };
            self.ui.log(format!("{} dropped ({why}); reconnecting in {} s", item.title, wait.as_secs()));
            self.unless_skipped(guild, epoch, tokio::time::sleep(wait)).await?;
            let next = match self.unless_skipped(guild, epoch, tokio::time::timeout(LOAD_TIMEOUT, media::materialize(item))).await? {
                Ok(Ok(next)) => next,
                Ok(Err(err)) => {
                    why = err;
                    continue;
                }
                Err(_) => {
                    why = "it took too long to answer".into();
                    continue;
                }
            };
            if !self.voice_ready(guild, epoch).await {
                return None;
            }
            let handle = call.lock().await.play_only_input(next.input());
            let _ = handle.set_volume(self.level_for(guild, next.gain));
            let mut guilds = self.guilds.lock().await;
            return match guilds.get_mut(&guild.get()).filter(|state| current(state)) {
                Some(state) => {
                    state.handle = Some(handle.clone());
                    state.gain = Some(next.gain);
                    self.ui.log(format!("Reconnected {}", item.title));
                    Some((next, handle))
                }
                None => {
                    let _ = handle.stop();
                    None
                }
            };
        }
    }

    /// Logs and posts why a track didn't play (errors post even with Quiet on).
    async fn track_failed(&self, text: Option<ChannelId>, item: &Item, title: &str, why: &str) -> Ended {
        let (log, discord) = play_failure(item.kind == Kind::File, title, &item.source, why);
        self.ui.log(log);
        self.say(text, &discord).await;
        Ended::Failed
    }

    /// The radio stream couldn't be reopened: say so and move on.
    async fn radio_gave_up(&self, text: Option<ChannelId>, title: &str) -> Ended {
        self.say(text, &format!("{title} stopped and wouldn't reconnect.")).await;
        Ended::Failed
    }

    /// A YouTube stream that failed before any audio played is looked up again
    /// by the next route (next client, then yt-dlp) and started in its place.
    async fn next_route(
        &self,
        guild: GuildId,
        epoch: u64,
        call: &Arc<Mutex<songbird::Call>>,
        failed: &media::Playable,
        why: &str,
    ) -> Option<(media::Playable, TrackHandle)> {
        failed.via?;
        let current = |state: &GuildState| state.epoch == epoch && !state.skip_requested;
        if !self.guilds.lock().await.get(&guild.get()).is_some_and(current) {
            return None;
        }
        let next = match self.unless_skipped(guild, epoch, tokio::time::timeout(LOAD_TIMEOUT, media::retry_youtube(failed))).await? {
            Ok(Ok(next)) => next,
            Ok(Err(err)) => {
                self.ui.log(format!("{} wouldn't start ({why}) and nothing else worked: {err}", failed.title));
                return None;
            }
            Err(_) => return None,
        };
        self.ui.log(format!("{} wouldn't start ({why}). Trying again{}", failed.title, next.via_note()));
        let handle = call.lock().await.play_only_input(next.input());
        let _ = handle.set_volume(self.level_for(guild, next.gain));
        let mut guilds = self.guilds.lock().await;
        match guilds.get_mut(&guild.get()).filter(|state| current(state)) {
            Some(state) => {
                state.handle = Some(handle.clone());
                state.gain = Some(next.gain);
                Some((next, handle))
            }
            None => {
                let _ = handle.stop();
                None
            }
        }
    }

    /// Runs `work` unless Skip or Stop comes first; then it's dropped and
    /// this returns None right away.
    pub(crate) async fn unless_skipped<T>(&self, guild: GuildId, epoch: u64, work: impl std::future::Future<Output = T>) -> Option<T> {
        let cancelled = async {
            loop {
                tokio::time::sleep(Duration::from_millis(150)).await;
                let gone = self.guilds.lock().await.get(&guild.get()).is_none_or(|s| s.skip_requested || s.epoch != epoch);
                if gone {
                    return;
                }
            }
        };
        tokio::select! {
            out = work => Some(out),
            _ = cancelled => None,
        }
    }

    /// Applies the loop mode and clears the finished track.
    async fn finish(&self, guild: GuildId, item: Item, ended: Ended) {
        let mut guilds = self.guilds.lock().await;
        if let Some(state) = guilds.get_mut(&guild.get()) {
            state.queue.after_track(item, ended);
            state.handle = None;
            state.gain = None;
            state.now = None;
            state.paused = false;
        }
    }

    /// A related song when the queue runs out and autoplay is on. Songs from
    /// the play history are skipped so it doesn't repeat itself.
    async fn autoplay_pick(&self, guild: GuildId) -> Option<Item> {
        let data = self.store.guild(guild.get());
        if !data.autoplay {
            return None;
        }
        let (seed, sc_seed, title, last_sc, last_radio, text) = {
            let guilds = self.guilds.lock().await;
            let state = guilds.get(&guild.get())?;
            (state.last_youtube.clone(), state.last_soundcloud.clone(), state.last_title.clone(), state.last_was_soundcloud, state.last_was_radio, state.text_channel)
        };
        if !autoplay_follows(last_radio) {
            self.ui.log("Autoplay doesn't follow a radio station; the queue is done");
            return None;
        }
        if data.soundcloud_autoplay || (last_sc && sc_seed.is_some()) {
            return self.autoplay_soundcloud(guild, sc_seed.filter(|_| last_sc), title, text).await;
        }
        let Some(seed) = seed else {
            self.say(text, "Autoplay needs a YouTube song to start from. Play one and it will carry on from there.").await;
            return None;
        };
        let recent: HashSet<String> = self
            .store
            .history(guild.get())
            .into_iter()
            .filter_map(|p| media::youtube_id(&p.item.source))
            .chain(std::iter::once(seed.clone()))
            .collect();
        match media::youtube_list(&format!("RD{seed}"), Some(&seed), 30).await {
            Ok(items) => {
                let pick = items
                    .into_iter()
                    .find(|i| media::youtube_id(&i.source).is_some_and(|id| !recent.contains(&id)));
                if pick.is_none() {
                    self.say(text, "Autoplay ran out of new songs.").await;
                }
                pick
            }
            Err(err) => {
                self.ui.log(format!("Autoplay failed: {err}"));
                self.say(text, &format!("Autoplay couldn't find a related song: {err}")).await;
                None
            }
        }
    }

    /// SoundCloud autoplay: tracks related to the last SoundCloud song, or a
    /// SoundCloud search for the last song's title. Recently played songs are skipped.
    async fn autoplay_soundcloud(&self, guild: GuildId, seed: Option<String>, title: Option<String>, text: Option<ChannelId>) -> Option<Item> {
        let mut recent: HashSet<String> = self.store.history(guild.get()).into_iter().map(|p| source_key(&p.item.source)).collect();
        recent.extend(seed.iter().map(|s| source_key(s)));
        let mut why = None;
        if let Some(page) = &seed {
            match media::soundcloud_related(page, 30).await {
                Ok(items) => {
                    if let Some(pick) = fresh_pick(items, &recent) {
                        return Some(pick);
                    }
                }
                Err(err) => why = Some(err),
            }
        }
        if let Some(title) = title.filter(|t| !t.trim().is_empty()) {
            match media::soundcloud_hits(&search_words(&title), 20).await {
                Ok(items) => {
                    if let Some(pick) = fresh_pick(items, &recent) {
                        return Some(pick);
                    }
                }
                Err(err) => why = Some(err),
            }
        } else if seed.is_none() {
            self.say(text, "SoundCloud autoplay needs a song to start from. Play one and it will carry on from there.").await;
            return None;
        }
        match why {
            Some(err) => {
                self.ui.log(format!("SoundCloud autoplay failed: {err}"));
                self.say(text, &format!("Autoplay couldn't find a related SoundCloud song: {err}")).await;
            }
            None => self.say(text, "Autoplay ran out of new SoundCloud songs.").await,
        }
        None
    }

    // ------------------------------------------------------------------ actions

    pub async fn skip(&self, guild: GuildId) -> Result<String, String> {
        let text = {
            let mut guilds = self.guilds.lock().await;
            let state = guilds.get_mut(&guild.get()).ok_or("Nothing is playing.")?;
            let title = state.now.as_ref().map(|n| n.title.clone()).ok_or("Nothing is playing.")?;
            state.skip_requested = true;
            if let Some(handle) = &state.handle {
                let _ = handle.stop();
            }
            format!("Skipped **{}**.", escape(&title))
        };
        self.ui.log(text.replace("**", ""));
        Ok(text)
    }

    /// Records a vote to skip. Votes from people who have since left the
    /// channel no longer count. Returns the vote count and the song title.
    pub async fn add_skip_vote(&self, guild: GuildId, user: u64, present: &[u64]) -> Result<(usize, String), String> {
        let mut guilds = self.guilds.lock().await;
        let state = guilds.get_mut(&guild.get()).ok_or("Nothing is playing.")?;
        let title = state.now.as_ref().map(|n| n.title.clone()).ok_or("Nothing is playing.")?;
        state.skip_votes.insert(user);
        state.skip_votes.retain(|u| present.contains(u));
        Ok((state.skip_votes.len(), title))
    }

    pub async fn set_paused(&self, guild: GuildId, pause: bool) -> Result<String, String> {
        let text = {
            let mut guilds = self.guilds.lock().await;
            let state = guilds.get_mut(&guild.get()).ok_or("Nothing is playing.")?;
            let title = state.now.as_ref().map(|n| n.title.clone()).ok_or("Nothing is playing.")?;
            let handle = state.handle.clone().ok_or("The track is still loading. Try again in a moment.")?;
            if state.paused == pause {
                return Ok(if pause { "Already paused." } else { "Already playing." }.into());
            }
            let result = if pause { handle.pause() } else { handle.play() };
            result.map_err(|_| "That track already ended.".to_string())?;
            state.paused = pause;
            format!("{} **{}**.", if pause { "Paused" } else { "Resumed" }, escape(&title))
        };
        self.ui.log(text.replace("**", ""));
        self.refresh_np(guild).await;
        self.publish().await;
        Ok(text)
    }

    /// Leaves the voice channel and forgets the queue. Safe to call twice.
    pub async fn leave(&self, guild: GuildId) {
        self.leave_if(guild, false).await;
    }

    /// Like [`leave`](Self::leave). With `only_if_idle` it returns false and
    /// changes nothing when something was queued or started since the caller last looked.
    async fn leave_if(&self, guild: GuildId, only_if_idle: bool) -> bool {
        {
            let mut guilds = self.guilds.lock().await;
            if let Some(state) = guilds.get_mut(&guild.get()) {
                if only_if_idle && !is_idle(state) {
                    return false;
                }
                state.queue.clear();
                if !only_if_idle {
                    state.pending.clear();
                }
                state.epoch += 1;
                state.skip_requested = true;
                if let Some(handle) = state.handle.take() {
                    let _ = handle.stop();
                }
                state.now = None;
                state.paused = false;
                state.idle_since = None;
                state.left_at = Some(Instant::now());
                state.sleep_token += 1;
                state.sleep_at = None;
                state.sleep_after_song = false;
                state.voice_down = false;
                state.voice_paused = false;
                state.watched_call = 0;
                state.voice_missing = 0;
            }
        }
        let _ = self.songbird.remove(guild).await;
        self.clear_np(guild).await;
        self.publish().await;
        true
    }

    pub async fn set_volume(&self, guild: GuildId, level: i32) -> Result<String, String> {
        if !(0..=200).contains(&level) {
            return Err("Use a number from 0 to 200.".into());
        }
        let (_, save_err) = self.store.update(guild.get(), |data| data.volume = Some(level));
        if let Some(err) = save_err {
            return Err(format!("Volume set to {level}% in memory, but couldn't save lantern-data.json: {err}"));
        }
        if let Some(state) = self.guilds.lock().await.get(&guild.get()) {
            if let Some(handle) = &state.handle {
                let _ = handle.set_volume(level as f32 / 100.0 * self.leveling(state.gain.unwrap_or(1.0)));
            }
        }
        self.refresh_np(guild).await;
        self.publish().await;
        Ok(format!("Volume set to {level}%. This server keeps it."))
    }

    pub async fn set_loop(&self, guild: GuildId, mode: Option<LoopMode>) -> String {
        let mode = {
            let mut guilds = self.guilds.lock().await;
            let state = guilds.entry(guild.get()).or_default();
            state.queue.loop_mode = mode.unwrap_or_else(|| state.queue.loop_mode.next());
            state.queue.loop_mode
        };
        self.refresh_np(guild).await;
        self.publish().await;
        match mode {
            LoopMode::Off => "Loop is off.".into(),
            LoopMode::Track => "Looping this track.".into(),
            LoopMode::Queue => "Looping the whole queue.".into(),
        }
    }

    /// `autoplay soundcloud` / `autoplay youtube`: turns autoplay on and picks where related songs come from.
    pub async fn set_autoplay_source(&self, guild: GuildId, soundcloud: bool) -> String {
        let (_, save_err) = self.store.update(guild.get(), |data| {
            data.autoplay = true;
            data.soundcloud_autoplay = soundcloud;
        });
        self.publish().await;
        let base = if soundcloud {
            "Autoplay is on, from SoundCloud. When the queue ends I'll keep playing related SoundCloud songs."
        } else {
            "Autoplay is on, from YouTube. When the queue ends I'll keep playing related songs."
        };
        match save_err {
            Some(err) => format!("{base} Couldn't save lantern-data.json: {err}"),
            None => base.into(),
        }
    }

    /// The window's "From SoundCloud" box: changes the source without turning autoplay on or off.
    pub async fn set_soundcloud_autoplay(&self, guild: GuildId, on: bool) -> String {
        let (_, save_err) = self.store.update(guild.get(), |data| data.soundcloud_autoplay = on);
        self.publish().await;
        let base = if on { "Autoplay will use SoundCloud." } else { "Autoplay will use YouTube." };
        match save_err {
            Some(err) => format!("{base} Couldn't save lantern-data.json: {err}"),
            None => base.into(),
        }
    }

    pub async fn set_autoplay(&self, guild: GuildId, on: Option<bool>) -> String {
        let (on, save_err) = self.store.update(guild.get(), |data| {
            data.autoplay = on.unwrap_or(!data.autoplay);
            data.autoplay
        });
        let base = if on {
            "Autoplay is on. When the queue ends I'll keep playing related songs."
        } else {
            "Autoplay is off."
        };
        self.publish().await;
        if let Some(err) = save_err {
            return format!("{base} Couldn't save lantern-data.json: {err}");
        }
        base.into()
    }

    pub async fn shuffle(&self, guild: GuildId) -> Result<String, String> {
        let count = {
            let mut guilds = self.guilds.lock().await;
            let state = guilds.get_mut(&guild.get()).ok_or("The queue is empty.")?;
            if state.queue.len() < 2 {
                return Err("Add a few more songs first.".into());
            }
            state.queue.shuffle();
            state.queue.len()
        };
        self.refresh_np(guild).await;
        self.publish().await;
        Ok(format!("Shuffled {count} songs."))
    }

    pub async fn remove(&self, guild: GuildId, position: usize) -> Result<String, String> {
        let item = {
            let mut guilds = self.guilds.lock().await;
            let state = guilds.get_mut(&guild.get()).ok_or("The queue is empty.")?;
            state.queue.remove(position)?
        };
        self.refresh_np(guild).await;
        self.publish().await;
        Ok(format!("Removed **{}**.", escape(&item.title)))
    }

    pub async fn move_item(&self, guild: GuildId, from: usize, to: usize) -> Result<String, String> {
        let item = {
            let mut guilds = self.guilds.lock().await;
            let state = guilds.get_mut(&guild.get()).ok_or("The queue is empty.")?;
            state.queue.move_item(from, to)?
        };
        self.refresh_np(guild).await;
        self.publish().await;
        Ok(format!("Moved **{}** to #{to}.", escape(&item.title)))
    }

    pub async fn clear(&self, guild: GuildId) -> Result<String, String> {
        let count = {
            let mut guilds = self.guilds.lock().await;
            let state = guilds.get_mut(&guild.get()).ok_or("The queue is already empty.")?;
            state.queue.clear()
        };
        if count == 0 {
            return Err("The queue is already empty.".into());
        }
        self.refresh_np(guild).await;
        self.publish().await;
        Ok(format!("Cleared {count} song{} from the queue.", if count == 1 { "" } else { "s" }))
    }

    /// Jumps to an absolute position in seconds.
    pub async fn seek(&self, guild: GuildId, target: u64) -> Result<String, String> {
        let (handle, now) = {
            let guilds = self.guilds.lock().await;
            let state = guilds.get(&guild.get()).ok_or("Nothing is playing.")?;
            let now = state.now.clone().ok_or("Nothing is playing.")?;
            (state.handle.clone(), now)
        };
        if now.item.kind == Kind::Radio {
            return Err("Live radio can't skip ahead or back.".into());
        }
        let handle = handle.ok_or("The track is still loading. Try again in a moment.")?;
        if let Some(length) = now.length {
            if target >= length {
                return Err(format!("That's past the end. This track is {} long.", clock(length)));
            }
        }
        let landed = handle
            .seek_async(Duration::from_secs(target))
            .await
            .map_err(|_| "This track can't jump to a position.".to_string())?;
        if let Some(state) = self.guilds.lock().await.get_mut(&guild.get()) {
            state.position = landed.as_secs();
        }
        self.publish().await;
        Ok(format!("Jumped to {}.", clock(landed.as_secs())))
    }

    /// Moves forward (positive) or back (negative) by some seconds.
    pub async fn seek_by(&self, guild: GuildId, delta: i64) -> Result<String, String> {
        let handle = {
            let guilds = self.guilds.lock().await;
            guilds.get(&guild.get()).and_then(|s| s.handle.clone())
        };
        let handle = handle.ok_or("Nothing is playing.")?;
        let position = handle.get_info().await.map_err(|_| "Nothing is playing.".to_string())?.position.as_secs();
        self.seek(guild, position.saturating_add_signed(delta)).await
    }

    /// The window's buttons.
    pub async fn window_action(self: &Arc<Self>, guild: GuildId, action: Action) -> String {
        let result = match action {
            Action::Pause => self.set_paused(guild, true).await,
            Action::Resume => self.set_paused(guild, false).await,
            Action::Skip => self.skip(guild).await,
            Action::Leave => {
                self.leave(guild).await;
                Ok("Stopped and left the voice channel.".into())
            }
            Action::Remove(n) => self.remove(guild, n).await,
            Action::Move(from, to) => self.move_item(guild, from, to).await,
            Action::Clear => self.clear(guild).await,
            Action::Shuffle => self.shuffle(guild).await,
            Action::Loop(mode) => Ok(self.set_loop(guild, Some(mode)).await),
            Action::Autoplay(on) => Ok(self.set_autoplay(guild, Some(on)).await),
            Action::AutoplaySoundCloud(on) => Ok(self.set_soundcloud_autoplay(guild, on).await),
            Action::Volume(level) => self.set_volume(guild, level).await,
            Action::Enqueue(items, truncated) => Ok(self.queue_local(guild, items, truncated).await),
            Action::Import(path) => self.import_file(guild, &path).await,
            Action::Export(path) => self.export_file(guild, &path).await,
            Action::Replay(at, title) => match self.store.history(guild.get()).into_iter().find(|p| p.at == at && p.item.title == title) {
                Some(played) => Ok(self.queue_local(guild, vec![played.item], false).await),
                None => Err("That song is no longer in the history.".into()),
            },
            Action::Play(query) => self.play_from_web(guild, &query).await,
        };
        result.unwrap_or_else(|e| e).replace("**", "")
    }

    // ------------------------------------------------------------ now playing

    async fn announce_np(&self, guild: GuildId) {
        self.clear_np(guild).await;
        // Quiet: no Now playing card. The clear above removes one left from before.
        if !routine_channel_post(self.ui.settings().quiet) {
            return;
        }
        let Some((channel, embed, buttons)) = self.np_parts(guild).await else { return };
        let message = CreateMessage::new().embed(embed).components(buttons);
        if let Ok(sent) = channel.send_message(&self.http, message).await {
            if let Some(state) = self.guilds.lock().await.get_mut(&guild.get()) {
                state.np_message = Some((channel, sent.id));
            }
        }
    }

    /// Redraws the now-playing card after pause, loop, volume, or queue changes.
    pub async fn refresh_np(&self, guild: GuildId) {
        if !routine_channel_post(self.ui.settings().quiet) {
            self.clear_np(guild).await;
            return;
        }
        let target = self.guilds.lock().await.get(&guild.get()).and_then(|s| s.np_message);
        let Some((channel, message)) = target else { return };
        let Some((_, embed, buttons)) = self.np_parts(guild).await else { return };
        let _ = channel.edit_message(&self.http, message, EditMessage::new().embed(embed).components(buttons)).await;
    }

    pub async fn np_parts(
        &self,
        guild: GuildId,
    ) -> Option<(ChannelId, serenity::builder::CreateEmbed, Vec<serenity::builder::CreateActionRow>)> {
        let volume = self.volume_for(guild);
        let guilds = self.guilds.lock().await;
        let state = guilds.get(&guild.get())?;
        let now = state.now.as_ref()?;
        let channel = now.requested_in.or(state.text_channel)?;
        let next = state.queue.upcoming.front().map(|i| i.title.clone());
        let embed = crate::commands::np_embed(now, state.paused, state.queue.loop_mode, next.as_deref(), state.queue.len(), volume);
        let buttons = crate::commands::np_buttons(state.paused, state.queue.loop_mode);
        Some((channel, embed, buttons))
    }

    async fn clear_np(&self, guild: GuildId) {
        let old = self.guilds.lock().await.get_mut(&guild.get()).and_then(|s| s.np_message.take());
        if let Some((channel, message)) = old {
            let _ = channel.delete_message(&self.http, message).await;
        }
    }

    // ----------------------------------------------------------- housekeeping

    /// Sends the window a fresh snapshot of every active server.
    pub async fn publish(&self) {
        let default_volume = self.ui.settings().volume;
        let names = self.cache.get().cloned();
        let mut views: Vec<GuildView> = {
            let guilds = self.guilds.lock().await;
            guilds
                .iter()
                .filter(|(id, s)| s.now.is_some() || !s.queue.is_empty() || self.songbird.get(GuildId::new(**id)).is_some())
                .map(|(id, s)| {
                    let data = self.store.guild(*id);
                    let name = names
                        .as_ref()
                        .and_then(|c| c.guild(GuildId::new(*id)).map(|g| g.name.clone()))
                        .unwrap_or_else(|| format!("Server {id}"));
                    GuildView {
                        id: *id,
                        name,
                        current: s.now.as_ref().map(|n| {
                            if n.loading { format!("{} (loading…)", n.title) } else { n.title.clone() }
                        }),
                        paused: s.paused,
                        position: s.position,
                        length: s.now.as_ref().and_then(|n| n.length),
                        live: s.now.as_ref().is_some_and(|n| n.item.kind == Kind::Radio),
                        upcoming: s.queue.upcoming.iter().take(200).map(|i| i.title.clone()).collect(),
                        loop_mode: s.queue.loop_mode,
                        autoplay: data.autoplay,
                        soundcloud_autoplay: data.soundcloud_autoplay,
                        volume: data.volume.unwrap_or(default_volume),
                        recent: data.history.iter().rev().take(RECENT_SHOWN).map(|p| (p.at, p.item.title.clone())).collect(),
                    }
                })
                .collect()
        };
        views.sort_by(|a, b| a.name.cmp(&b.name));
        // Keep each queue for `restore` after a restart (1.7). Not while
        // shutting down: leaving voice then must not wipe it.
        if self.alive.load(Ordering::Acquire) {
            let snapshots: Vec<(u64, Vec<Item>, Option<u64>)> = {
                let guilds = self.guilds.lock().await;
                guilds
                    .iter()
                    .map(|(id, s)| (*id, crate::store::queue_snapshot(s.now.as_ref().map(|n| &n.item), s.queue.upcoming.iter()), s.text_channel.map(|c| c.get())))
                    .collect()
            };
            for (id, items, text) in snapshots {
                if let Some(err) = self.store.set_saved_queue(id, items, text) {
                    self.note_data_save_failed(&err);
                }
            }
        }
        let song = views
            .iter()
            .find(|v| v.current.is_some() && !v.paused)
            .or_else(|| views.iter().find(|v| v.current.is_some()))
            .and_then(|v| status_text(v.current.as_deref()?, v.paused));
        self.set_activity(song);
        let playing = views.iter().any(|v| v.current.is_some() && !v.paused);
        if self.alive.load(Ordering::Acquire) {
            self.ui.set_connection(if playing { "Playing" } else { "Online" });
        }
        self.ui.set_guilds(views);
    }

    /// Every 15 seconds: leave servers that have had nothing playing for too long.
    pub async fn idle_check(&self) {
        let minutes = self.ui.settings().idle_minutes;
        if minutes == 0 {
            return;
        }
        let limit = Duration::from_secs(u64::from(minutes) * 60);
        let mut leave = Vec::new();
        {
            let mut guilds = self.guilds.lock().await;
            for (id, state) in guilds.iter_mut() {
                let connected = self.songbird.get(GuildId::new(*id)).is_some();
                if !connected || state.now.is_some() || state.worker {
                    state.idle_since = None;
                    continue;
                }
                let since = *state.idle_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= limit {
                    leave.push((*id, state.text_channel));
                }
            }
        }
        for (id, channel) in leave {
            // Someone may have played something since the list was made.
            if self.leave_if(GuildId::new(id), true).await {
                self.ui.log(format!("Left a voice channel after {minutes} idle minutes"));
                self.say(channel, &format!("Nothing played for {minutes} minutes, so I left. `play` brings me back.")).await;
            }
        }
    }

    /// The name of a voice channel for the log, or its number if the cache doesn't know it.
    fn channel_name(&self, guild: GuildId, channel: ChannelId) -> String {
        self.cache
            .get()
            .and_then(|cache| cache.guild(guild).and_then(|g| g.channels.get(&channel).map(|c| c.name.clone())))
            .unwrap_or_else(|| channel.get().to_string())
    }

    /// Whether Discord (as far as the cache knows) has Lantern in a voice
    /// channel in this server. `None` when the cache can't tell.
    fn in_voice_per_discord(&self, guild: GuildId) -> Option<bool> {
        let cache = self.cache.get()?;
        let me = cache.current_user().id;
        let g = cache.guild(guild)?;
        Some(g.voice_states.get(&me).is_some_and(|v| v.channel_id.is_some()))
    }

    /// Attaches [`crate::voice::VoiceWatch`] to this server's `Call` once.
    async fn watch_voice(self: &Arc<Self>, guild: GuildId, call: &Arc<Mutex<songbird::Call>>) {
        let key = Arc::as_ptr(call) as usize;
        {
            let mut guilds = self.guilds.lock().await;
            let state = guilds.entry(guild.get()).or_default();
            if state.watched_call == key {
                return;
            }
            state.watched_call = key;
        }
        let mut call = call.lock().await;
        for event in [songbird::CoreEvent::DriverDisconnect, songbird::CoreEvent::DriverConnect, songbird::CoreEvent::DriverReconnect] {
            call.add_global_event(event.into(), crate::voice::VoiceWatch { core: Arc::downgrade(self), guild });
        }
    }

    /// Before each new song: make sure the voice connection is alive. If it
    /// dropped, wait for the rejoin. Returns false when Stop, Leave, or the
    /// bot stopping ends the wait.
    async fn voice_ready(self: &Arc<Self>, guild: GuildId, epoch: u64) -> bool {
        let Some(call) = self.songbird.get(guild) else { return false };
        self.watch_voice(guild, &call).await;
        let channel = call.lock().await.current_channel().map(|c| ChannelId::new(c.0.get()));
        let down = self.guilds.lock().await.get(&guild.get()).is_some_and(|s| s.voice_down);
        match channel {
            Some(channel) if !down && self.in_voice_per_discord(guild) == Some(false) => {
                self.voice_dropped(guild, channel, "Discord doesn't list me in the channel").await;
            }
            Some(_) => {}
            None if !down => return true,
            None => {}
        }
        loop {
            {
                let guilds = self.guilds.lock().await;
                let Some(state) = guilds.get(&guild.get()) else { return false };
                if state.epoch != epoch || !self.alive.load(Ordering::Acquire) || self.songbird.get(guild).is_none() {
                    return false;
                }
                if !state.voice_down {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// The voice connection died: pause the song and start rejoining.
    pub(crate) async fn voice_dropped(self: &Arc<Self>, guild: GuildId, channel: ChannelId, why: &str) {
        let start = {
            let mut guilds = self.guilds.lock().await;
            let Some(state) = guilds.get_mut(&guild.get()) else { return };
            state.voice_down = true;
            state.voice_missing = 0;
            !std::mem::replace(&mut state.rejoining, true)
        };
        if start {
            self.ui.log(format!("Voice connection dropped ({why}); rejoining {}", self.channel_name(guild, channel)));
            let core = self.clone();
            tokio::spawn(async move { core.rejoin(guild, channel).await });
        }
    }

    /// Songbird got the voice connection back by itself.
    pub(crate) async fn voice_back(&self, guild: GuildId) {
        if let Some(state) = self.guilds.lock().await.get_mut(&guild.get()) {
            if !state.rejoining {
                state.voice_down = false;
            }
            state.voice_missing = 0;
        }
    }

    /// Rejoins `channel` until it works, Stop/Leave happens, or the bot
    /// stops. First try: ask Songbird to reconnect with the same session.
    /// After that: leave and join fresh. Waits longer after each failure.
    async fn rejoin(self: Arc<Self>, guild: GuildId, channel: ChannelId) {
        let name = self.channel_name(guild, channel);
        {
            let mut guilds = self.guilds.lock().await;
            if let Some(state) = guilds.get_mut(&guild.get()) {
                if !state.paused {
                    if let Some(handle) = &state.handle {
                        if handle.pause().is_ok() {
                            state.voice_paused = true;
                        }
                    }
                }
            }
        }
        tokio::time::sleep(crate::voice::SETTLE).await;
        let mut attempt = 0u32;
        loop {
            let wanted = self.alive.load(Ordering::Acquire)
                && self.songbird.get(guild).is_some()
                && self.guilds.lock().await.get(&guild.get()).is_some_and(|s| s.voice_down);
            if !wanted {
                if let Some(state) = self.guilds.lock().await.get_mut(&guild.get()) {
                    state.rejoining = false;
                    state.voice_down = false;
                    state.voice_paused = false;
                }
                return;
            }
            attempt += 1;
            if attempt > 1 {
                // A fresh voice session: leave, then join again. The leave is
                // marked as ours so it isn't taken for a kick (which clears the queue).
                if let Some(state) = self.guilds.lock().await.get_mut(&guild.get()) {
                    state.left_at = Some(Instant::now());
                }
                if let Some(call) = self.songbird.get(guild) {
                    let _ = call.lock().await.leave().await;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let result = match tokio::time::timeout(crate::voice::JOIN_TIMEOUT, self.songbird.join(guild, channel)).await {
                Ok(Ok(call)) => {
                    let _ = call.lock().await.deafen(true).await;
                    Ok(())
                }
                Ok(Err(err)) => Err(err.to_string()),
                Err(_) => Err("no answer in 30 seconds".to_string()),
            };
            match result {
                Ok(()) => {
                    let resumed = {
                        let mut guilds = self.guilds.lock().await;
                        let Some(state) = guilds.get_mut(&guild.get()) else { return };
                        state.voice_down = false;
                        state.rejoining = false;
                        state.voice_missing = 0;
                        let title = state.now.as_ref().map(|n| n.title.clone());
                        if std::mem::take(&mut state.voice_paused) {
                            if let Some(handle) = &state.handle {
                                let _ = handle.play();
                            }
                        }
                        title
                    };
                    self.ui.log(match resumed {
                        Some(title) => format!("Rejoined {name}; carrying on with {title}"),
                        None => format!("Rejoined {name}"),
                    });
                    self.publish().await;
                    return;
                }
                Err(err) => {
                    let wait = crate::voice::rejoin_delay(attempt);
                    self.ui.log(format!("Couldn't rejoin {name} (try {attempt}: {err}); trying again in {}s", wait.as_secs()));
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    /// Every 15 seconds: catch a dead voice connection that sent no event.
    pub async fn voice_check(self: &Arc<Self>) {
        let calls: Vec<_> = self.songbird.iter().map(|(id, call)| (GuildId::new(id.0.get()), call)).collect();
        for (guild, call) in calls {
            let playing = self.guilds.lock().await.get(&guild.get()).is_some_and(|s| s.now.is_some() && !s.rejoining);
            if !playing {
                continue;
            }
            self.watch_voice(guild, &call).await;
            let Some(channel) = call.lock().await.current_channel().map(|c| ChannelId::new(c.0.get())) else { continue };
            let missing = self.in_voice_per_discord(guild) == Some(false);
            let (down, count) = {
                let mut guilds = self.guilds.lock().await;
                let Some(state) = guilds.get_mut(&guild.get()) else { continue };
                state.voice_missing = if missing { state.voice_missing.saturating_add(1) } else { 0 };
                (state.voice_down, state.voice_missing)
            };
            if down {
                self.voice_dropped(guild, channel, "still down").await;
            } else if count >= crate::voice::MISSING_CHECKS {
                self.voice_dropped(guild, channel, "Discord doesn't list me in the channel").await;
            }
        }
    }

    /// Lantern was disconnected from voice by someone else.
    pub async fn kicked(&self, guild: GuildId) {
        let notice = {
            let guilds = self.guilds.lock().await;
            let recent = guilds
                .get(&guild.get())
                .and_then(|s| s.left_at)
                .is_some_and(|at| at.elapsed() < Duration::from_secs(10));
            if recent || self.songbird.get(guild).is_none() {
                return;
            }
            guilds.get(&guild.get()).and_then(|s| s.text_channel)
        };
        self.ui.log("Disconnected from voice by someone else; cleared the queue");
        self.leave(guild).await;
        self.say(notice, "I was disconnected from the voice channel, so I cleared the queue.").await;
    }

    /// Leave every channel and close the gateway.
    async fn shutdown(&self) {
        self.alive.store(false, Ordering::Release);
        let mut ids: HashSet<u64> = self.guilds.lock().await.keys().copied().collect();
        ids.extend(self.songbird.iter().map(|(id, _)| id.0.get()));
        for id in ids {
            self.leave(GuildId::new(id)).await;
        }
        if let Some(shard) = self.shard.get() {
            shard.shutdown_all().await;
        }
    }

    pub async fn report_left_empty(&self, guild: GuildId) {
        let channel = self.text_channel(guild).await;
        self.ui.log("Left an empty voice channel");
        self.leave(guild).await;
        self.say(channel, "Everyone left, so I left too.").await;
    }
}

/// What `set_activity` sends. `None` clears Listening.
/// Quiet is not an argument: presence follows `song_status` only.
pub(crate) fn presence_activity(song_status: bool, wanted: Option<String>) -> Option<String> {
    if song_status { wanted } else { None }
}

/// The "Listening to …" text for a song. `None` while it is still loading.
pub(crate) fn status_text(current: &str, paused: bool) -> Option<String> {
    if current.ends_with("(loading…)") {
        return None;
    }
    let title: String = current.chars().take(110).collect();
    Some(if paused { format!("{title} (paused)") } else { title })
}

/// Nothing playing, loading, or waiting, and no player task about to start.
fn is_idle(state: &GuildState) -> bool {
    state.now.is_none() && !state.worker && state.queue.is_empty()
}

#[derive(PartialEq, Eq)]
enum Exit {
    QueueEnded,
    Cancelled,
}

/// Stops song titles from breaking Discord formatting.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '*' | '_' | '`' | '~' | '|' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// What Discord is told when a track fails. Local files never include the path or the raw error.
pub(crate) fn play_failure(local: bool, title: &str, source: &str, err: &str) -> (String, String) {
    let detail = if local { scrub_path(err, source) } else { err.to_string() };
    let log = format!("Couldn't play {title}: {detail}");
    let discord = if local {
        format!("Couldn't play **{}**.", escape(title))
    } else {
        format!("Couldn't play **{}**: {err}", escape(title))
    };
    (log, discord)
}

fn scrub_path(err: &str, source: &str) -> String {
    if source.is_empty() {
        return err.to_string();
    }
    let mut out = err.replace(source, "that file");
    let forward = source.replace('\\', "/");
    if forward != source {
        out = out.replace(&forward, "that file");
    }
    out
}

impl Core {
    /// Puts files from the window onto this server's queue and starts them
    /// if Lantern is already in voice, or if the owner is.
    pub(crate) async fn queue_local(self: &Arc<Self>, guild: GuildId, items: Vec<Item>, truncated: bool) -> String {
        if items.is_empty() {
            return "No audio files there.".into();
        }
        if let Err(err) = self.ensure_voice(guild).await {
            return err;
        }
        let first = items[0].title.clone();
        let count = items.len();
        self.push(guild, items, None).await;
        if routine_channel_post(self.ui.settings().quiet) {
            let channel = self.text_channel(guild).await;
            let discord = if count == 1 {
                format!("Queued **{}**.", escape(&first))
            } else {
                format!("Queued **{count}** tracks. First: **{}**.", escape(&first))
            };
            self.say(channel, &discord).await;
        }
        let mut note = if count == 1 {
            format!("Queued {first}.")
        } else {
            format!("Queued {count} tracks. First: {first}.")
        };
        if truncated {
            note.push_str(&format!(" Stopped at {}.", media::LOCAL_TRACK_LIMIT));
        }
        note
    }

    /// Joins the owner's current voice channel when Lantern isn't in one yet.
    async fn ensure_voice(&self, guild: GuildId) -> Result<(), String> {
        if self.songbird.get(guild).is_some() {
            return Ok(());
        }
        let Some(channel) = self.owner_channel(guild) else {
            return Err("Lantern isn't in a voice channel. Join one in Discord first. Set Owner ID in Settings if you want the window to join you.".into());
        };
        match self.songbird.join(guild, channel).await {
            Ok(call) => {
                let _ = call.lock().await.deafen(true).await;
                Ok(())
            }
            Err(_) => Err("Couldn't join the voice channel. Check that Lantern can Connect and Speak there.".into()),
        }
    }

    /// A link or search from the web page (1.9). Same lookups as `play` with
    /// "Ask which YouTube result to play" off: the first match is queued.
    async fn play_from_web(self: &Arc<Self>, guild: GuildId, query: &str) -> Result<String, String> {
        let query = query.trim();
        if query.is_empty() {
            return Err("Type a link or something to search for.".into());
        }
        if let Some(words) = crate::commands::local_query(query) {
            if words.is_empty() {
                return Err("Type what to find after local:".into());
            }
            let (found, empty) = crate::library::with(|index| (index.find(words).map(|t| t.item()), index.tracks.is_empty()));
            return match found {
                Some(item) => Ok(self.queue_local(guild, vec![item], false).await),
                None if empty => Err("The music library is empty.".into()),
                None => Err(format!("Nothing in the library matches {words}.")),
            };
        }
        let limit = self.ui.settings().playlist_limit;
        match crate::cache::resolve(query, limit, false).await {
            Ok(items) if !items.is_empty() => Ok(self.queue_local(guild, items, false).await),
            Ok(_) => Err("Nothing found.".into()),
            Err(err) => Err(format!("Couldn't find that: {err}")),
        }
    }

    pub(crate) async fn ensure_voice_pub(&self, guild: GuildId) -> Result<(), String> {
        self.ensure_voice(guild).await
    }

    fn owner_channel(&self, guild: GuildId) -> Option<ChannelId> {
        let owner: u64 = self.ui.settings().owner.trim().parse().ok()?;
        let cache = self.cache.get()?;
        let guild = cache.guild(guild)?;
        guild.voice_states.get(&UserId::new(owner)).and_then(|state| state.channel_id)
    }
}

#[cfg(test)]
mod tests {
    use super::Watchdog;
    use std::time::Duration;
    const STEP: Duration = Duration::from_millis(250);

    #[test]
    fn live_radio_has_no_end_only_a_dead_stream_counts() {
        // Hours of a live stream with no length never trip the end-of-song rules.
        let mut w = Watchdog::default();
        let mut ms = 0u64;
        while ms < 3 * 3_600_000 { assert!(!w.tick(Some(ms), false, None, STEP)); ms += 250; }
        // A stream that goes silent for 30 s counts as dropped (and radio reconnects).
        let mut fired = false;
        for _ in 0..121 { if w.tick(Some(ms), false, None, STEP) { fired = true; break; } }
        assert!(fired);
        // Paused radio is never dropped.
        let mut w = Watchdog::default();
        for i in 0..80 { w.tick(Some(i * 250), false, None, STEP); }
        for _ in 0..1000 { assert!(!w.tick(Some(20_000), true, None, STEP)); }
    }

    #[test]
    fn radio_reconnects_back_off_then_give_up_and_reset_after_steady_play() {
        use super::RadioRetry;
        use std::time::Instant;
        let start = Instant::now();
        let mut retry = RadioRetry::default();
        let waits: Vec<u64> = std::iter::from_fn(|| retry.next(start)).map(|d| d.as_secs()).collect();
        assert_eq!(waits, [2, 5, 10, 20, 30]);
        assert_eq!(retry.next(start), None, "it keeps dropping: give up");
        // Played fine for a minute after the last reconnect: start over.
        assert_eq!(retry.next(start + Duration::from_secs(30 + 61)), Some(Duration::from_secs(2)));
        // Dropped again soon after: the next wait is longer.
        assert_eq!(retry.next(start + Duration::from_secs(30 + 65)), Some(Duration::from_secs(5)));
    }

    #[test]
    fn decode_failures_are_said_plainly() {
        assert!(super::plain_play_error("Failed to create audio: Parsing error: probe: no suitable format reader found").starts_with("Lantern can't decode"));
        assert_eq!(super::plain_play_error("the station stopped sending audio"), "the station stopped sending audio");
    }

    #[test]
    fn autoplay_does_not_follow_radio() {
        assert!(!super::autoplay_follows(true));
        assert!(super::autoplay_follows(false));
    }

    #[test]
    fn a_track_stuck_past_its_length_advances() {
        let mut w = Watchdog::default();
        let mut ms = 0;
        while ms < 207_000 { assert!(!w.tick(Some(ms), false, Some(207), STEP)); ms += 250; }
        let mut fired = false;
        for _ in 0..40 { if w.tick(Some(207_000), false, Some(207), STEP) { fired = true; break; } }
        assert!(fired, "stalled at the end for 10 s must finish");
    }

    #[test]
    fn position_running_past_length_finishes() {
        let mut w = Watchdog::default();
        assert!(!w.tick(Some(200_000), false, Some(207), STEP));
        assert!(w.tick(Some(213_000), false, Some(207), STEP));
    }

    #[test]
    fn get_info_that_never_answers_ends_a_started_song() {
        let mut w = Watchdog::default();
        assert!(!w.tick(Some(60_000), false, Some(207), STEP));
        let mut n = 0;
        while !w.tick(None, false, Some(207), super::INFO_TIMEOUT) { n += 1; assert!(n < 20); }
    }

    #[test]
    fn pause_and_slow_start_never_trigger() {
        let mut w = Watchdog::default();
        assert!(!w.tick(Some(206_500), false, Some(207), STEP));
        for _ in 0..1000 { assert!(!w.tick(Some(206_500), true, Some(207), STEP)); }
        let mut w = Watchdog::default();
        for _ in 0..1000 { assert!(!w.tick(Some(0), false, Some(207), STEP)); }
        let mut w = Watchdog::default();
        assert!(!w.tick(Some(60_000), false, Some(207), STEP));
        for _ in 0..100 { assert!(!w.tick(Some(60_000), false, Some(207), STEP)); }
    }

    #[test]
    fn a_stream_cut_off_in_the_first_seconds_is_a_failure() {
        use super::ended_early;
        assert!(ended_early(2, Some(240)), "2 s of a 4 min song");
        assert!(ended_early(0, Some(31)));
        assert!(!ended_early(10, Some(240)), "10 s played is a real start");
        assert!(!ended_early(3, Some(5)), "a 5 s clip that finished");
        assert!(!ended_early(2, Some(30)));
        assert!(!ended_early(2, None), "unknown length: can't tell");
    }

    #[test]
    fn autoplay_stops_after_three_failures_in_a_row() {
        use super::{note_result, Ended};
        let mut streak = 0;
        assert!(!note_result(&mut streak, Ended::Failed));
        assert!(!note_result(&mut streak, Ended::Failed));
        assert!(!note_result(&mut streak, Ended::Skipped), "a skip doesn't reset or count");
        assert!(note_result(&mut streak, Ended::Failed));
        assert!(!note_result(&mut streak, Ended::Finished), "a song that played resets it");
        assert_eq!(streak, 0);
    }

    use super::*;
    use crate::media::{Item, Kind};
    use crate::settings::Settings;

    fn core() -> Arc<Core> {
        Arc::new(Core {
            session: 1,
            ui: UiState::new(Settings::default()),
            http: Arc::new(Http::new("test")),
            songbird: Songbird::serenity(),
            cache: OnceLock::new(),
            shard: OnceLock::new(),
            ctx: std::sync::Mutex::new(None),
            guilds: Mutex::new(HashMap::new()),
            picks: Mutex::new(HashMap::new()),
            store: Store::memory(),
            alive: AtomicBool::new(true),
            ticker: AtomicBool::new(false),
            slash_done: AtomicBool::new(false),
            resume_offered: AtomicBool::new(false),
            status: std::sync::Mutex::new(None),
        })
    }

    #[test]
    fn waiting_songs_start_for_whoever_queued_them() {
        let mut a = song("a");
        a.requested_by = Some(7);
        let pending = vec![song("b"), a];
        assert!(take_pending_for(&pending, 7));
        assert!(!take_pending_for(&pending, 8));
        assert!(!take_pending_for(&[], 7));
    }

    fn song(title: &str) -> Item {
        Item { title: title.into(), source: format!("https://youtu.be/{title}"), query: None, kind: Kind::YouTube, length: None, allow_private_host: false, requested_by: None }
    }

    fn playing() -> Now {
        Now { item: song("now"), title: "now".into(), length: None, loading: false, requested_in: None }
    }

    #[tokio::test]
    async fn idle_leave_changes_nothing_if_a_song_arrived_meanwhile() {
        let core = core();
        let guild = GuildId::new(7);
        for (name, setup) in [
            ("queued song", Box::new(|s: &mut GuildState| s.queue.push([song("a")])) as Box<dyn Fn(&mut GuildState)>),
            ("player starting", Box::new(|s: &mut GuildState| s.worker = true)),
            ("track loading", Box::new(|s: &mut GuildState| s.now = Some(playing()))),
        ] {
            let mut state = GuildState::default();
            setup(&mut state);
            core.guilds.lock().await.insert(guild.get(), state);
            assert!(!core.leave_if(guild, true).await, "left while busy: {name}");
            let guilds = core.guilds.lock().await;
            let state = &guilds[&guild.get()];
            assert_eq!(state.epoch, 0, "{name}");
            assert!(state.left_at.is_none(), "{name}");
            assert_eq!(state.queue.len(), usize::from(name == "queued song"), "{name}");
        }
    }

    #[tokio::test]
    async fn skip_and_stop_cut_a_slow_load_short() {
        let core = core();
        let guild = GuildId::new(9);
        core.guilds.lock().await.insert(guild.get(), GuildState::default());
        let started = Instant::now();
        let waiter = {
            let core = core.clone();
            tokio::spawn(async move { core.unless_skipped(guild, 0, tokio::time::sleep(Duration::from_secs(60))).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        core.guilds.lock().await.get_mut(&guild.get()).unwrap().skip_requested = true;
        assert!(waiter.await.unwrap().is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
        // Stop (a new epoch) also ends it; finished work is returned as is.
        core.guilds.lock().await.get_mut(&guild.get()).unwrap().skip_requested = false;
        assert_eq!(core.unless_skipped(guild, 0, async { 5 }).await, Some(5));
        core.guilds.lock().await.get_mut(&guild.get()).unwrap().epoch = 1;
        assert!(core.unless_skipped(guild, 0, tokio::time::sleep(Duration::from_secs(60))).await.is_none());
    }

    #[tokio::test]
    async fn idle_leave_happens_when_nothing_is_going_on() {
        let core = core();
        let guild = GuildId::new(7);
        core.guilds.lock().await.insert(guild.get(), GuildState::default());
        assert!(core.leave_if(guild, true).await);
        assert_eq!(core.guilds.lock().await[&guild.get()].epoch, 1);
        // A server that was never used is fine too, and a second leave is harmless.
        core.leave(GuildId::new(8)).await;
        core.leave(guild).await;
    }

    #[tokio::test]
    async fn stop_clears_the_queue_and_invalidates_loading_tracks() {
        let core = core();
        let guild = GuildId::new(9);
        let mut state = GuildState::default();
        state.queue.push([song("a"), song("b")]);
        state.now = Some(playing());
        core.guilds.lock().await.insert(guild.get(), state);
        core.leave(guild).await;
        let guilds = core.guilds.lock().await;
        let state = &guilds[&guild.get()];
        assert!(state.queue.is_empty() && state.now.is_none());
        assert_eq!(state.epoch, 1, "a track that was loading must see the epoch change");
    }

    #[tokio::test]
    async fn queue_commands_on_an_unused_server_say_so_instead_of_panicking() {
        let core = core();
        let guild = GuildId::new(10);
        assert!(core.skip(guild).await.is_err());
        assert!(core.set_paused(guild, true).await.is_err());
        assert!(core.remove(guild, 1).await.is_err());
        assert!(core.move_item(guild, 1, 2).await.is_err());
        assert!(core.clear(guild).await.is_err());
        assert!(core.shuffle(guild).await.is_err());
        assert!(core.seek(guild, 30).await.is_err());
        assert!(core.seek_by(guild, -10).await.is_err());
        assert!(core.set_volume(guild, 201).await.is_err());
        assert!(core.set_volume(guild, -1).await.is_err());
        assert!(core.set_volume(guild, 200).await.is_ok());
        assert_eq!(core.volume_for(guild), 200);
    }

    #[tokio::test]
    async fn queue_edits_validate_numbers() {
        let core = core();
        let guild = GuildId::new(11);
        let mut state = GuildState::default();
        state.queue.push([song("a"), song("b"), song("c")]);
        core.guilds.lock().await.insert(guild.get(), state);
        assert!(core.remove(guild, 0).await.is_err());
        assert!(core.remove(guild, usize::MAX).await.is_err());
        assert!(core.move_item(guild, 1, 4).await.is_err());
        assert!(core.move_item(guild, 3, 1).await.is_ok());
        assert!(core.remove(guild, 3).await.is_ok());
        assert_eq!(core.clear(guild).await.unwrap(), "Cleared 2 songs from the queue.");
        assert_eq!(core.window_action(guild, Action::Clear).await, "The queue is already empty.");
    }

    #[tokio::test]
    async fn skip_votes_count_people_once_and_drop_leavers() {
        let core = core();
        let guild = GuildId::new(13);
        assert!(core.add_skip_vote(guild, 1, &[1, 2, 3]).await.is_err(), "nothing playing");
        let state = GuildState { now: Some(playing()), ..Default::default() };
        core.guilds.lock().await.insert(guild.get(), state);
        assert_eq!(core.add_skip_vote(guild, 1, &[1, 2, 3]).await.unwrap().0, 1);
        assert_eq!(core.add_skip_vote(guild, 1, &[1, 2, 3]).await.unwrap().0, 1, "same person twice");
        assert_eq!(core.add_skip_vote(guild, 2, &[2, 3, 4]).await.unwrap().0, 1, "person 1 left");
    }

    #[test]
    fn listening_status_follows_song_status_not_quiet() {
        let song = Some("Song (paused)".to_string());
        assert_eq!(presence_activity(true, song.clone()).as_deref(), Some("Song (paused)"));
        assert_eq!(presence_activity(false, song), None);
        // Quiet can be on while song_status stays on. presence_activity has no quiet input.
        let both = crate::settings::Settings { quiet: true, song_status: true, ..Default::default() };
        assert!(both.quiet && both.song_status);
        assert_eq!(presence_activity(both.song_status, Some("Song".into())).as_deref(), Some("Song"));
        assert!(!crate::settings::routine_channel_post(both.quiet));
    }

    #[test]
    fn status_follows_the_song() {
        assert_eq!(status_text("Song", false).as_deref(), Some("Song"));
        assert_eq!(status_text("Song", true).as_deref(), Some("Song (paused)"));
        assert_eq!(status_text("Song (loading…)", false), None);
        assert_eq!(status_text(&"x".repeat(300), false).unwrap().chars().count(), 110);
    }

    #[test]
    fn titles_are_escaped_for_discord() {
        assert_eq!(escape("A*B_C"), "A\\*B\\_C");
        assert_eq!(escape("plain"), "plain");
    }

    #[test]
    fn a_local_file_failure_does_not_show_the_path() {
        let path = r"D:\Music\Secret Folder\Song Title.mp3";
        let (log, discord) = play_failure(true, "Song Title", path, &format!("open {path} failed"));
        assert_eq!(discord, "Couldn't play **Song Title**.");
        assert!(!discord.contains("Secret") && !discord.contains("D:\\"));
        assert!(!log.contains("Secret") && !log.contains("D:\\"));
        assert!(log.contains("Song Title"));
    }

    #[tokio::test]
    async fn files_are_not_queued_when_lantern_is_not_in_voice() {
        let core = core();
        let guild = GuildId::new(12);
        let item = Item {
            title: "Song Title".into(),
            source: r"D:\Music\Secret Folder\Song Title.mp3".into(),
            query: None,
            kind: Kind::File,
            length: None,
            allow_private_host: false,
            requested_by: None,
        };
        let msg = core.window_action(guild, Action::Enqueue(vec![item], false)).await;
        assert!(!msg.contains("Secret") && !msg.contains("D:\\"), "{msg}");
        assert!(msg.contains("voice"));
        assert!(core.guilds.lock().await.get(&guild.get()).map(|s| s.queue.is_empty()).unwrap_or(true));
    }

    #[test]
    fn soundcloud_autoplay_skips_recent_songs() {
        let sc = |url: &str| Item { title: url.into(), source: url.into(), query: None, kind: Kind::SoundCloud, length: None, allow_private_host: false, requested_by: None };
        let recent: HashSet<String> = [source_key("https://soundcloud.com/A/One/"), source_key("https://soundcloud.com/a/two?in=x")].into_iter().collect();
        let items = vec![sc("https://soundcloud.com/a/one"), sc("https://soundcloud.com/a/TWO"), sc("https://soundcloud.com/a/three")];
        assert_eq!(fresh_pick(items, &recent).unwrap().source, "https://soundcloud.com/a/three");
        assert!(fresh_pick(vec![sc("https://soundcloud.com/a/one")], &recent).is_none());
    }

    #[test]
    fn autoplay_search_words_drop_tags() {
        assert_eq!(search_words("Daft Punk - One More Time (Official Video) [HD]"), "Daft Punk - One More Time");
        assert_eq!(search_words("(Intro)"), "(Intro)");
        assert_eq!(search_words("  Plain  title "), "Plain title");
    }

    #[test]
    fn soundcloud_autoplay_setting_defaults_off() {
        let data: crate::store::GuildData = serde_json::from_str(r#"{"autoplay": true}"#).unwrap();
        assert!(data.autoplay);
        assert!(!data.soundcloud_autoplay);
    }

    #[tokio::test]
    async fn stop_forgets_a_dropped_voice_connection() {
        let core = core();
        let guild = GuildId::new(11);
        let state = GuildState { voice_down: true, voice_paused: true, watched_call: 42, voice_missing: 1, ..Default::default() };
        core.guilds.lock().await.insert(guild.get(), state);
        core.leave(guild).await;
        let guilds = core.guilds.lock().await;
        let state = &guilds[&guild.get()];
        assert!(!state.voice_down && !state.voice_paused);
        assert_eq!((state.watched_call, state.voice_missing), (0, 0), "the next Call gets a fresh watch");
    }

    #[tokio::test]
    async fn voice_coming_back_by_itself_only_counts_without_a_rejoin_running() {
        let core = core();
        let guild = GuildId::new(12);
        core.guilds.lock().await.insert(guild.get(), GuildState { voice_down: true, ..Default::default() });
        core.voice_back(guild).await;
        assert!(!core.guilds.lock().await[&guild.get()].voice_down, "Songbird reconnected on its own");
        core.guilds.lock().await.insert(guild.get(), GuildState { voice_down: true, rejoining: true, ..Default::default() });
        core.voice_back(guild).await;
        assert!(core.guilds.lock().await[&guild.get()].voice_down, "the rejoin task decides when it's back");
    }

    #[tokio::test]
    async fn a_drop_with_nothing_registered_starts_no_rejoin_and_songs_wait_for_nothing() {
        let core = core();
        let guild = GuildId::new(13);
        // No GuildState: nothing to rejoin for.
        core.voice_dropped(guild, ChannelId::new(5), "test").await;
        assert!(core.guilds.lock().await.get(&guild.get()).is_none());
        // Not in voice at all: a new song doesn't wait, it just finds no call.
        assert!(!core.voice_ready(guild, 0).await);
    }

    #[tokio::test]
    async fn a_second_drop_while_rejoining_starts_no_second_rejoin() {
        let core = core();
        let guild = GuildId::new(14);
        core.guilds.lock().await.insert(guild.get(), GuildState { rejoining: true, ..Default::default() });
        core.voice_dropped(guild, ChannelId::new(5), "test").await;
        let guilds = core.guilds.lock().await;
        assert!(guilds[&guild.get()].voice_down && guilds[&guild.get()].rejoining);
        drop(guilds);
        let lines = core.ui.logs();
        assert!(!lines.iter().any(|l| l.contains("rejoining")), "only the first drop is logged and acted on");
    }
}
