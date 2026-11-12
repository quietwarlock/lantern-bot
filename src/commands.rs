//! Discord events: prefix commands, slash commands, buttons, reactions, voice changes.
//!
//! Prefix commands (`!play`) and slash commands (`/play`) go through the same
//! [`Handler::dispatch`]. Who may do what:
//! - **Anyone:** `queue`, `np`, `help`, `history`, `playlists`, `lyrics`, `volume` with no number.
//! - **Anyone in Lantern's voice channel:** `play`, `local` (the owner's music library), and `load`. They can't pull it elsewhere.
//! - **Controllers:** everything else. A controller is the owner from the window,
//!   or a member with this server's DJ role. With no owner and no DJ role, everyone is.
//! - **Owner or server admin:** `dj`, which sets the DJ role.

use crate::i18n::t;
use crate::bot::{escape, Core, Now, PendingPick};
use crate::media::{self, Hit, Item, Kind};
use crate::queue::{clock, parse_time, LoopMode};
use crate::settings::{routine_channel_post, Settings};
use serenity::all::{
    ButtonStyle, ChannelId, CommandDataOptionValue, CommandInteraction, CommandOptionType, ComponentInteraction, Context,
    CreateActionRow, CreateAutocompleteResponse, CreateButton, CreateCommand, CreateCommandOption, CreateEmbed, CreateEmbedFooter,
    CreateInteractionResponse, CreateInteractionResponseFollowup, CreateInteractionResponseMessage, CreateMessage, EditInteractionResponse, EditMessage,
    EventHandler, GuildId, Interaction, Message, MessageId, Permissions, Reaction, ReactionType, Ready, RoleId, UserId,
    VoiceState,
};
use serenity::async_trait;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

const CHOICE_EMOJI: [&str; 4] = ["1️⃣", "2️⃣", "3️⃣", "4️⃣"];
const CANCEL_EMOJI: &str = "❌";
pub const ACCENT: u32 = 0xFF_2E_A6;
pub const CYAN: u32 = 0x2D_E2_E6;

/// Who may run a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    Anyone,
    Listener,
    Controller,
    Admin,
}

/// `radio` words that keep meaning autoplay, as before 1.10.
const AUTOPLAY_WORDS: [&str; 13] = ["", "on", "yes", "true", "1", "off", "no", "false", "0", "soundcloud", "sc", "youtube", "yt"];

/// 1.10: `radio` followed by anything other than an autoplay word (and the
/// `/radio` slash command) searches internet radio stations.
pub(crate) fn radio_means_search(command: &str, arg: &str, slash: bool) -> bool {
    command.eq_ignore_ascii_case("radio") && (slash || !AUTOPLAY_WORDS.contains(&arg.trim().to_ascii_lowercase().as_str()))
}

/// Maps a typed command or alias to its name and permission. `None` means
/// it isn't a Lantern command, so the message is ignored.
pub fn lookup(command: &str) -> Option<(&'static str, Need)> {
    let found = match command.to_ascii_lowercase().as_str() {
        "play" | "p" => ("play", Need::Listener),
        "load" => ("load", Need::Listener),
        "import" => ("import", Need::Listener),
        "export" => ("export", Need::Anyone),
        "local" | "lib" => ("local", Need::Listener),
        "lyrics" | "ly" => ("lyrics", Need::Anyone),
        "queue" | "q" => ("queue", Need::Anyone),
        "np" | "nowplaying" => ("np", Need::Anyone),
        "help" | "commands" | "" => ("help", Need::Anyone),
        "history" => ("history", Need::Anyone),
        "fav" | "favorite" | "favourite" | "favs" | "favorites" => ("fav", Need::Anyone),
        "replay" | "again" => ("replay", Need::Listener),
        "restore" => ("restore", Need::Listener),
        "playlists" => ("playlists", Need::Anyone),
        "volume" | "vol" => ("volume", Need::Controller),
        "skip" | "s" => ("skip", Need::Listener),
        "pause" => ("pause", Need::Controller),
        "resume" => ("resume", Need::Controller),
        "stop" | "leave" | "dc" => ("stop", Need::Controller),
        "remove" | "rm" => ("remove", Need::Controller),
        "move" | "mv" => ("move", Need::Controller),
        "clear" => ("clear", Need::Controller),
        "shuffle" => ("shuffle", Need::Controller),
        "loop" | "repeat" => ("loop", Need::Controller),
        "seek" => ("seek", Need::Controller),
        "forward" | "ff" => ("forward", Need::Controller),
        "rewind" | "rw" => ("rewind", Need::Controller),
        "autoplay" | "radio" => ("autoplay", Need::Controller),
        "station" | "stations" => ("station", Need::Listener),
        "sleep" => ("sleep", Need::Controller),
        "save" => ("save", Need::Controller),
        "delete" => ("delete", Need::Controller),
        "dj" => ("dj", Need::Admin),
        _ => return None,
    };
    Some(found)
}

pub(crate) struct Handler {
    pub(crate) core: Arc<Core>,
}

/// Where a command came from, so the answer goes back the same way.
pub enum From<'a> {
    Message(&'a Message),
    Slash(&'a CommandInteraction),
}

impl From<'_> {
    fn guild(&self) -> Option<GuildId> {
        match self {
            From::Message(msg) => msg.guild_id,
            From::Slash(cmd) => cmd.guild_id,
        }
    }
    fn user(&self) -> UserId {
        match self {
            From::Message(msg) => msg.author.id,
            From::Slash(cmd) => cmd.user.id,
        }
    }
    /// The Discord user name of whoever ran the command, for the log.
    fn user_name(&self) -> String {
        match self {
            From::Message(msg) => msg.author.name.clone(),
            From::Slash(cmd) => cmd.user.name.clone(),
        }
    }
    /// How the command was typed, like `!leave` or `/stop`.
    fn typed(&self, command: &str, prefix: &str) -> String {
        match self {
            From::Message(_) => format!("{prefix}{command}"),
            From::Slash(cmd) => format!("/{}", cmd.data.name),
        }
    }
    fn channel(&self) -> ChannelId {
        match self {
            From::Message(msg) => msg.channel_id,
            From::Slash(cmd) => cmd.channel_id,
        }
    }
    fn roles(&self) -> Vec<RoleId> {
        match self {
            From::Message(msg) => msg.member.as_ref().map(|m| m.roles.clone()).unwrap_or_default(),
            From::Slash(cmd) => cmd.member.as_ref().map(|m| m.roles.clone()).unwrap_or_default(),
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        *self.core.ctx.lock().unwrap_or_else(|p| p.into_inner()) = Some(ctx.clone());
        // A reconnect drops the status; publish() below sends it again.
        self.core.reset_status();
        let prefix = self.core.ui.settings().prefix;
        self.core.ui.set_live(
            "Online",
            "Nothing playing",
            &format!("Online as {}. Use {prefix}play or /play in a voice channel.", ready.user.name),
        );
        self.core.ui.log(format!("Online as {} in {} server(s)", ready.user.name, ready.guilds.len()));
        // Ready comes again after every full reconnect; Discord limits how often commands may be re-registered.
        if !self.core.slash_done.swap(true, Ordering::AcqRel) {
            if let Err(err) = serenity::model::application::Command::set_global_commands(&ctx.http, slash_commands()).await {
                self.core.slash_done.store(false, Ordering::Release);
                self.core.ui.log(format!("Slash commands were not updated: {err}"));
            }
        }
        if !self.core.ticker.swap(true, Ordering::AcqRel) {
            let core = self.core.clone();
            tokio::spawn(async move {
                while core.alive.load(Ordering::Acquire) {
                    tokio::time::sleep(Duration::from_secs(15)).await;
                    core.idle_check().await;
                    core.voice_check().await;
                }
            });
        }
        if !self.core.resume_offered.swap(true, Ordering::AcqRel) {
            let quiet = self.core.ui.settings().quiet;
            for (guild, count, text) in self.core.store.saved_queues() {
                self.core.ui.log(format!("{count} song(s) were queued in server {guild} before the restart. `restore` puts them back."));
                // The log line above is enough when Quiet is on. `restore` still works.
                if post_restore_offer(quiet) {
                    self.core.say(text.map(ChannelId::new), &crate::i18n::tf("resume_offer", &[&count.to_string()])).await;
                }
            }
        }
        self.core.publish().await;
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot || msg.guild_id.is_none() {
            return;
        }
        let settings = self.core.ui.settings();
        if self.take_typed_choice(&msg).await {
            return;
        }
        let content = msg.content.trim();
        let rest = if let Some(rest) = content.strip_prefix(settings.prefix.as_str()) {
            rest
        } else if let Some(rest) = strip_mention(content, &ctx) {
            rest
        } else {
            return;
        };
        let (command, arg) = split_cmd(rest);
        // "!!!" or "!rank" for another bot: not ours, say nothing.
        if lookup(command).is_none() {
            return;
        }
        self.dispatch(&ctx, From::Message(&msg), command, arg, &settings).await;
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(command) => {
                let arg = slash_arg(&command);
                let name = command.data.name.clone();
                // Answer first: Discord drops the interaction if nothing comes back within 3 seconds.
                let first = if lookup(&name).is_some() {
                    CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new())
                } else {
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new().content("Lantern doesn't have that command.").ephemeral(true),
                    )
                };
                let _ = command.create_response(&ctx.http, first).await;
                if lookup(&name).is_none() {
                    return;
                }
                let settings = self.core.ui.settings();
                self.dispatch(&ctx, From::Slash(&command), &name, &arg, &settings).await;
            }
            Interaction::Component(component) => self.button(&ctx, &component).await,
            Interaction::Autocomplete(command) => {
                let typed = command.data.autocomplete().map(|a| a.value.to_string()).unwrap_or_default();
                let mut response = CreateAutocompleteResponse::new();
                if command.data.name == "local" {
                    for (name, value) in library_choices(&typed) {
                        response = response.add_string_choice(name, value);
                    }
                }
                let _ = command.create_response(&ctx.http, CreateInteractionResponse::Autocomplete(response)).await;
            }
            _ => {}
        }
    }

    async fn reaction_add(&self, ctx: Context, reaction: Reaction) {
        let Some(user_id) = reaction.user_id else { return };
        if user_id == ctx.cache.current_user().id {
            return;
        }
        let ReactionType::Unicode(emoji) = &reaction.emoji else { return };
        let Some(choice) = choice_number(emoji) else { return };
        self.finish_pick(user_id, Some(reaction.message_id), reaction.channel_id, choice).await;
    }

    /// A deleted DJ role would lock everyone but the owner out, so it's forgotten.
    async fn guild_role_delete(&self, _ctx: Context, guild: GuildId, role: RoleId, _data: Option<serenity::all::Role>) {
        if !dj_role_deleted(self.core.store.guild(guild.get()).dj_role, role.get()) {
            return;
        }
        let (_, save_err) = self.core.store.update(guild.get(), |d| d.dj_role = None);
        self.core.ui.log(match save_err {
            None => "The DJ role was deleted in Discord, so Lantern no longer uses one.".to_string(),
            Some(err) => format!("The DJ role was deleted in Discord; couldn't save lantern-data.json: {err}"),
        });
    }

    async fn voice_state_update(&self, ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        let Some(guild) = new.guild_id.or(old.as_ref().and_then(|v| v.guild_id)) else { return };
        let me = ctx.cache.current_user().id;
        if new.user_id == me {
            match (old.and_then(|o| o.channel_id), new.channel_id) {
                (_, None) => self.core.kicked(guild).await,
                (Some(before), Some(after)) if before != after => {
                    self.core.ui.log(format!("Moved to another voice channel ({after})"));
                }
                _ => {}
            }
            return;
        }
        if let Some(channel) = new.channel_id {
            if bot_channel(&self.core, guild).await.is_none() {
                self.core.start_pending(guild, channel, new.user_id.get()).await;
            }
        }
        if !self.core.ui.settings().leave_alone {
            return;
        }
        let Some(channel) = bot_channel(&self.core, guild).await else { return };
        if !channel_is_empty(&ctx, guild, channel) {
            return;
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
        if bot_channel(&self.core, guild).await != Some(channel) || !channel_is_empty(&ctx, guild, channel) {
            return;
        }
        self.core.report_left_empty(guild).await;
    }
}

impl Handler {
    async fn dispatch(&self, ctx: &Context, from: From<'_>, command: &str, arg: &str, settings: &Settings) {
        let command = if radio_means_search(command, arg, matches!(from, From::Slash(_))) { "station" } else { command };
        let Some((name, mut need)) = lookup(command) else { return };
        let Some(guild) = from.guild() else {
            self.reply(ctx, &from, &t("use_lantern_commands_in_a_server")).await;
            return;
        };
        if name == "volume" && arg.trim().is_empty() {
            need = Need::Anyone;
        }
        let user = from.user();
        let controller = self.is_controller(guild, user, &from.roles(), settings);
        let allowed = match need {
            Need::Anyone => true,
            Need::Controller => controller,
            Need::Listener => {
                controller
                    || in_playback_channel(ctx, &self.core, guild, user).await
                    // 1.7: queue before Lantern joins voice.
                    || (matches!(name, "play" | "local" | "restore" | "station")
                        && voice_channel(ctx, guild, user).is_none()
                        && bot_channel(&self.core, guild).await.is_none())
            }
            Need::Admin => self.is_admin(ctx, &from, guild, settings),
        };
        if !allowed {
            let text = match need {
                Need::Listener => "Join my voice channel to add songs.",
                Need::Admin => "Only the owner or a server admin can set the DJ role.",
                _ => "Only the owner or a DJ can do that.",
            };
            self.reply(ctx, &from, text).await;
            return;
        }
        let core = &self.core;
        let answer: Result<String, String> = match name {
            "play" => return self.play(ctx, from, guild, arg, settings).await,
            "load" => return self.load(ctx, from, guild, arg).await,
            "import" => return self.import(ctx, from, guild).await,
            "export" => return self.export(ctx, &from, guild, arg).await,
            "local" => return self.play(ctx, from, guild, &format!("local:{arg}"), settings).await,
            "station" => return self.station(ctx, from, guild, arg, settings).await,
            "lyrics" => return self.lyrics(ctx, &from, guild, arg).await,
            "help" => return self.help(ctx, &from, settings).await,
            "queue" => return self.queue(ctx, &from, guild).await,
            "np" => return self.np(ctx, &from, guild).await,
            "history" => return self.history(ctx, &from, guild, arg).await,
            "fav" => return self.fav(ctx, from, guild, arg, controller).await,
            "replay" => return self.replay(ctx, from, guild, arg).await,
            "restore" => return self.restore(ctx, from, guild).await,
            "playlists" => return self.playlists(ctx, &from, guild, settings).await,
            "skip" if controller => core.skip(guild).await,
            "skip" => self.vote_skip(ctx, guild, user).await,
            "pause" => core.set_paused(guild, true).await,
            "resume" => core.set_paused(guild, false).await,
            "stop" => {
                core.leave(guild).await;
                core.ui.log(stopped_by(&from.user_name(), &from.typed(command, &settings.prefix)));
                Ok("Stopped and left the voice channel.".into())
            }
            "volume" => match arg.trim().trim_end_matches('%').parse::<i32>() {
                Ok(level) => core.set_volume(guild, level).await,
                Err(_) if arg.trim().is_empty() => Ok(format!("Volume is {}%.", core.volume_for(guild))),
                Err(_) => Err("Use a number from 0 to 200, like `volume 60`.".into()),
            },
            "remove" => match arg.trim().parse::<usize>() {
                Ok(n) => core.remove(guild, n).await,
                Err(_) => Err("Say which one, like `remove 3`. `queue` shows the numbers.".into()),
            },
            "move" => {
                let numbers: Vec<usize> = arg.split_whitespace().filter_map(|n| n.parse().ok()).collect();
                match numbers.as_slice() {
                    [from_pos, to_pos] => core.move_item(guild, *from_pos, *to_pos).await,
                    _ => Err("Say from and to, like `move 5 1`.".into()),
                }
            }
            "clear" => core.clear(guild).await,
            "shuffle" => core.shuffle(guild).await,
            "loop" => {
                if arg.trim().is_empty() {
                    Ok(core.set_loop(guild, None).await)
                } else {
                    match LoopMode::parse(arg) {
                        Some(mode) => Ok(core.set_loop(guild, Some(mode)).await),
                        None => Err("Use `loop off`, `loop track`, or `loop queue`.".into()),
                    }
                }
            }
            "seek" => match parse_time(arg) {
                Some(seconds) => core.seek(guild, seconds).await,
                None => Err("Say where to jump, like `seek 1:30`.".into()),
            },
            "forward" | "rewind" => {
                let seconds = if arg.trim().is_empty() { Some(10) } else { parse_time(arg) };
                match seconds {
                    Some(s) => {
                        let s = i64::try_from(s).unwrap_or(i64::MAX);
                        core.seek_by(guild, if name == "forward" { s } else { -s }).await
                    }
                    None => Err(format!("Say how many seconds, like `{name} 30`.")),
                }
            }
            "autoplay" => match arg.trim().to_ascii_lowercase().as_str() {
                "" => Ok(core.set_autoplay(guild, None).await),
                "on" | "yes" | "true" | "1" => Ok(core.set_autoplay(guild, Some(true)).await),
                "off" | "no" | "false" | "0" => Ok(core.set_autoplay(guild, Some(false)).await),
                "soundcloud" | "sc" => Ok(core.set_autoplay_source(guild, true).await),
                "youtube" | "yt" => Ok(core.set_autoplay_source(guild, false).await),
                _ => Err("Use `autoplay on`, `autoplay off`, `autoplay soundcloud`, or `autoplay youtube`.".into()),
            },
            "sleep" => match crate::features::parse_sleep(arg) {
                Some(request) => core.sleep_timer(guild, request, Some(from.channel())).await,
                None => Err("Say how long, like `sleep 30m`, `sleep 1h`, or `sleep end` (after this song). Up to 12 hours.".into()),
            },
            "save" => self.save(guild, arg).await,
            "delete" => core.store.delete_playlist(guild.get(), arg),
            "dj" => self.dj(ctx, &from, guild, arg),
            _ => return,
        };
        let text = answer.unwrap_or_else(|e| e);
        self.reply(ctx, &from, &text).await;
    }

    fn is_controller(&self, guild: GuildId, user: UserId, roles: &[RoleId], settings: &Settings) -> bool {
        let owner = settings.owner.trim();
        if !owner.is_empty() && owner == user.to_string() {
            return true;
        }
        let dj = self.core.store.guild(guild.get()).dj_role;
        if owner.is_empty() && dj.is_none() {
            return true;
        }
        dj.is_some_and(|role| roles.contains(&RoleId::new(role)))
    }

    fn is_admin(&self, ctx: &Context, from: &From<'_>, guild: GuildId, settings: &Settings) -> bool {
        let user = from.user();
        if !settings.owner.trim().is_empty() && settings.owner.trim() == user.to_string() {
            return true;
        }
        if let From::Slash(cmd) = from {
            if let Some(perms) = cmd.member.as_ref().and_then(|m| m.permissions) {
                return perms.administrator() || perms.manage_guild();
            }
        }
        let Some(guild) = ctx.cache.guild(guild) else { return false };
        if guild.owner_id == user {
            return true;
        }
        let everyone = RoleId::new(guild.id.get());
        let mut perms = guild.roles.get(&everyone).map(|r| r.permissions).unwrap_or_else(Permissions::empty);
        for role in from.roles() {
            if let Some(role) = guild.roles.get(&role) {
                perms |= role.permissions;
            }
        }
        perms.administrator() || perms.manage_guild()
    }

    async fn play(&self, ctx: &Context, from: From<'_>, guild: GuildId, query: &str, settings: &Settings) {
        if query.trim().is_empty() {
            self.reply(ctx, &from, &t("tell_me_what_to_play_like")).await;
            return;
        }
        // No voice channel yet: the songs wait until the person joins one (1.7).
        let voice = match voice_channel(ctx, guild, from.user()) {
            Some(voice) => Some(voice),
            None => bot_channel(&self.core, guild).await,
        };
        // The owner's music library. Only words or an autocomplete key come
        // from Discord; the path comes from the library index.
        if let Some(words) = local_query(query) {
            if words.is_empty() {
                self.reply(ctx, &from, &t("tell_me_what_to_find_like")).await;
                return;
            }
            let (found, empty) = crate::library::with(|index| (index.find(words).map(|t| t.item()), index.tracks.is_empty()));
            match found {
                Some(item) => {
                    self.enqueue(ctx, &from, guild, voice, vec![item], true).await;
                }
                None if empty => self.reply(ctx, &from, &t("the_music_library_is_empty_the")).await,
                None => self.reply(ctx, &from, &format!("Nothing in the library matches **{}**.", escape(words))).await,
            }
            return;
        }
        // Links already name the audio. A typed search gets a choice of four.
        if settings.choose_result && !is_direct(query) {
            let hits = match crate::cache::youtube_hits(query, 4).await {
                Ok(hits) => hits,
                Err(err) => {
                    self.reply(ctx, &from, &format!("Couldn't find that: {err}")).await;
                    return;
                }
            };
            let (item, held_picker) = if hits.len() == 1 {
                (hits[0].item.clone(), None)
            } else {
                match self.ask(ctx, &from, &hits, "Pick a track").await {
                    Some(pair) => pair,
                    None => return,
                }
            };
            let answered = self.enqueue(ctx, &from, guild, voice, vec![item], hits.len() == 1).await;
            // Quiet leaves the picker up until enqueue finishes, so a join or
            // limit error can replace it. A silent success then removes it.
            if let Some(message) = held_picker {
                if drop_held_picker(matches!(from, From::Slash(_)), answered) {
                    let _ = message.channel_id.delete_message(&ctx.http, message.id).await;
                }
            }
            return;
        }
        let user = from.user();
        let allow_private = settings.owner.trim() == user.to_string();
        let items = match crate::cache::resolve(query, settings.playlist_limit, allow_private).await {
            Ok(items) if !items.is_empty() => items,
            Ok(_) => {
                self.reply(ctx, &from, &t("nothing_found")).await;
                return;
            }
            Err(err) => {
                self.core.ui.log(format!("Lookup failed: {err}"));
                self.reply(ctx, &from, &format!("Couldn't find that: {err}")).await;
                return;
            }
        };
        self.enqueue(ctx, &from, guild, voice, items, true).await;
    }

    /// `radio` words (1.10): internet radio stations from Radio Browser, top 4 to pick from.
    async fn station(&self, ctx: &Context, from: From<'_>, guild: GuildId, words: &str, settings: &Settings) {
        if words.trim().is_empty() {
            let p = &settings.prefix;
            self.reply(ctx, &from, &format!("Say what to look for, like `{p}radio jazz` or `{p}radio tag:rock country:Germany`.")).await;
            return;
        }
        let voice = match voice_channel(ctx, guild, from.user()) {
            Some(voice) => Some(voice),
            None => bot_channel(&self.core, guild).await,
        };
        let stations = match crate::radiobrowser::search_words(words, 4).await {
            Ok(stations) if !stations.is_empty() => stations,
            Ok(_) => {
                self.reply(ctx, &from, &format!("No radio stations match **{}**.", escape(words.trim()))).await;
                return;
            }
            Err(err) => {
                self.core.ui.log(format!("Radio search failed: {err}"));
                self.reply(ctx, &from, &format!("Couldn't search radio stations: {err}")).await;
                return;
            }
        };
        let picks: Vec<(Hit, String)> = stations
            .iter()
            .filter_map(|s| Some((Hit { item: s.item()?, channel: s.summary() }, s.stationuuid.clone())))
            .take(4)
            .collect();
        let hits: Vec<Hit> = picks.iter().map(|(hit, _)| hit.clone()).collect();
        let (item, held_picker) = if hits.len() == 1 {
            (hits[0].item.clone(), None)
        } else {
            match self.ask(ctx, &from, &hits, "Pick a station").await {
                Some(pair) => pair,
                None => return,
            }
        };
        let uuid = picks.iter().find(|(hit, _)| hit.item.source == item.source).map(|(_, uuid)| uuid.clone());
        let answered = self.enqueue(ctx, &from, guild, voice, vec![item], hits.len() == 1).await;
        if let Some(uuid) = uuid {
            crate::radiobrowser::click(&uuid);
        }
        if let Some(message) = held_picker {
            if drop_held_picker(matches!(from, From::Slash(_)), answered) {
                let _ = message.channel_id.delete_message(&ctx.http, message.id).await;
            }
        }
    }

    /// `true` when this already answered (an error, a limit note, "waiting for
    /// voice", a Queued announce, or a dropped slash placeholder). Callers that
    /// send their own success line use `false` to know the slash command is
    /// still waiting.
    async fn enqueue(&self, ctx: &Context, from: &From<'_>, guild: GuildId, voice: Option<ChannelId>, items: Vec<Item>, announce: bool) -> bool {
        if let Some(message) = empty_enqueue_message(&items) {
            self.reply(ctx, from, message).await;
            return true;
        }
        let settings = self.core.ui.settings();
        let user = from.user().get();
        let exempt = self.is_controller(guild, from.user(), &from.roles(), &settings);
        let already = {
            let guilds = self.core.guilds.lock().await;
            guilds.get(&guild.get()).map(|s| s.queue.upcoming.iter().chain(s.pending.iter()).filter(|i| i.requested_by == Some(user)).count()).unwrap_or(0)
        };
        let checked = crate::limits::check(items, already, settings.max_per_user, settings.max_song_minutes, exempt);
        let left_out = crate::limits::note(&checked, settings.max_per_user, settings.max_song_minutes);
        let mut items = checked.kept;
        if items.is_empty() {
            self.reply(ctx, from, &left_out.unwrap_or_else(|| t("nothing_left_after_limits"))).await;
            return true;
        }
        if let Some(note) = &left_out {
            self.reply(ctx, from, note).await;
        }
        for item in &mut items {
            item.requested_by = Some(user);
        }
        // Someone not in voice adding to a queue that's already playing: that
        // queue gets the songs. Only with Lantern out of voice do they wait.
        let voice = queue_target(voice, bot_channel(&self.core, guild).await);
        let Some(voice) = voice else {
            let count = items.len();
            let first = items[0].title.clone();
            {
                let mut guilds = self.core.guilds.lock().await;
                let state = guilds.entry(guild.get()).or_default();
                state.text_channel = Some(from.channel());
                state.pending.extend(items);
            }
            self.core.ui.log(format!("Queued {first} until someone who asked joins voice"));
            let text = if count == 1 {
                crate::i18n::tf("queued_before_voice", &[&escape(&first)])
            } else {
                crate::i18n::tf("queued_many_before_voice", &[&count.to_string()])
            };
            self.reply(ctx, from, &text).await;
            return true;
        };
        let here = bot_channel(&self.core, guild).await == Some(voice);
        if !here {
            match self.core.songbird.join(guild, voice).await {
                Ok(call) => {
                    let _ = call.lock().await.deafen(true).await;
                }
                Err(err) => {
                    self.core.ui.log(format!("Couldn't join voice: {err}"));
                    self.reply(ctx, from, &t("i_couldn_t_join_that_voice")).await;
                    return true;
                }
            }
        }
        let first = items.first().map(|item| item.title.clone()).expect("checked empty");
        let count = items.len();
        let busy = self.core.guilds.lock().await.get(&guild.get()).map(|s| s.now.is_some()).unwrap_or(false);
        let length = self.core.push(guild, items, Some(from.channel())).await;
        self.core.ui.log(format!(
            "Queued {first}{}",
            if count > 1 { format!(" and {} more", count - 1) } else { String::new() }
        ));
        if enqueue_announces(announce, settings.quiet) {
            let text = if count == 1 {
                if busy {
                    format!("Queued **{}** at #{length}.", escape(&first))
                } else {
                    format!("Queued **{}**.", escape(&first))
                }
            } else {
                format!("Queued **{count}** tracks. First: **{}**.", escape(&first))
            };
            self.reply(ctx, from, &text).await;
            return true;
        } else if drop_slash_placeholder(matches!(from, From::Slash(_)), announce, settings.quiet, left_out.is_some()) {
            // `/play` was deferred. With nothing to say, drop the placeholder
            // so Discord doesn't sit on "thinking…".
            self.drop_deferred_slash(ctx, from).await;
            return true;
        }
        left_out.is_some()
    }

    async fn lyrics(&self, ctx: &Context, from: &From<'_>, guild: GuildId, arg: &str) {
        if matches!(arg.trim().to_ascii_lowercase().as_str(), "live" | "synced" | "sync") {
            return self.live_lyrics(ctx, from, guild).await;
        }
        self.plain_lyrics(ctx, from, guild, arg).await
    }

    async fn plain_lyrics(&self, ctx: &Context, from: &From<'_>, guild: GuildId, arg: &str) {
        let (query, length) = if arg.trim().is_empty() {
            let now = self.core.guilds.lock().await.get(&guild.get()).and_then(|s| s.now.as_ref().map(|n| (n.title.clone(), n.length)));
            match now {
                Some((title, length)) => (crate::lyrics::clean_title(&title), length),
                None => {
                    self.reply(ctx, from, &t("nothing_is_playing_try_lyrics_song")).await;
                    return;
                }
            }
        } else {
            (arg.trim().to_string(), None)
        };
        match crate::lyrics::search(&query, length).await {
            Ok(found) => {
                let words = if found.instrumental {
                    "*(Instrumental)*".to_string()
                } else {
                    crate::lyrics::fit(found.plain_lyrics.as_deref().unwrap_or_default())
                };
                let title = format!("{} · {}", found.track_name, found.artist_name);
                let embed = note_embed(&words)
                    .title(title.chars().take(250).collect::<String>())
                    .footer(CreateEmbedFooter::new("Lyrics from lrclib.net"));
                self.send_embed(ctx, from, embed).await;
            }
            Err(err) => self.reply(ctx, from, &format!("{err} (searched for **{}**)", escape(&query))).await,
        }
    }

    /// `lyrics live` (1.7): timed lyrics that follow the song in one message,
    /// edited every few seconds until the song changes.
    async fn live_lyrics(&self, ctx: &Context, from: &From<'_>, guild: GuildId) {
        let now = self.core.guilds.lock().await.get(&guild.get()).and_then(|s| s.now.as_ref().map(|n| (n.item.source.clone(), n.title.clone(), n.length)));
        let Some((source, title, length)) = now else {
            self.reply(ctx, from, &t("nothing_is_playing_try_lyrics_song_name")).await;
            return;
        };
        let found = match crate::lyrics::search(&crate::lyrics::clean_title(&title), length).await {
            Ok(found) => found,
            Err(err) => return self.reply(ctx, from, &err).await,
        };
        let lines = found.synced_lyrics.as_deref().map(crate::lyrics::parse_synced).unwrap_or_default();
        if lines.is_empty() {
            self.reply(ctx, from, &t("lyrics_synced_none")).await;
            return self.plain_lyrics(ctx, from, guild, "").await;
        }
        let heading = format!("{} · {}", found.track_name, found.artist_name).chars().take(250).collect::<String>();
        let embed_at = |ms: u64| note_embed(&crate::lyrics::window(&lines, ms, 2, 4)).title(heading.clone()).footer(CreateEmbedFooter::new("Live lyrics from lrclib.net"));
        let Some(mut message) = self.send_embed(ctx, from, embed_at(self.position_ms(guild).await.unwrap_or(0))).await else { return };
        let mut last = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(length.unwrap_or(600).min(1800) + 30);
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let still = self.core.guilds.lock().await.get(&guild.get()).and_then(|s| s.now.as_ref().map(|n| n.item.source == source)).unwrap_or(false);
            let Some(ms) = self.position_ms(guild).await.filter(|_| still) else { break };
            let line = crate::lyrics::current_line(&lines, ms);
            if line != last {
                last = line;
                if message.edit(&ctx.http, EditMessage::new().embed(embed_at(ms))).await.is_err() {
                    break;
                }
            }
        }
    }

    /// Where the current song is, in milliseconds.
    async fn position_ms(&self, guild: GuildId) -> Option<u64> {
        let handle = self.core.guilds.lock().await.get(&guild.get()).and_then(|s| s.handle.clone())?;
        handle.get_info().await.ok().map(|info| info.position.as_millis() as u64)
    }

    async fn load(&self, ctx: &Context, from: From<'_>, guild: GuildId, name: &str) {
        let settings = self.core.ui.settings();
        let allow_private = settings.owner.trim() == from.user().to_string();
        let items = match self.core.store.load_playlist(guild.get(), name) {
            Ok(items) => items,
            Err(err) => {
                self.reply(ctx, &from, &err).await;
                return;
            }
        };
        if let Err(err) = media::ensure_items_fetchable(&items, allow_private) {
            self.reply(ctx, &from, &err).await;
            return;
        }
        let Some(voice) = voice_channel(ctx, guild, from.user()) else {
            self.reply(ctx, &from, &t("join_a_voice_channel_first")).await;
            return;
        };
        self.enqueue(ctx, &from, guild, Some(voice), items, true).await;
    }

    /// `import` with a playlist file attached: links play, and paths play only
    /// when that file is in the owner's music library.
    async fn import(&self, ctx: &Context, from: From<'_>, guild: GuildId) {
        let attachment = match &from {
            From::Message(msg) => msg.attachments.first().cloned(),
            From::Slash(cmd) => cmd.data.resolved.attachments.values().next().cloned(),
        };
        let Some(attachment) = attachment else {
            self.reply(ctx, &from, &t("attach_a_playlist_file_m3u_m3u8")).await;
            return;
        };
        if u64::from(attachment.size) > crate::playlist_file::MAX_FILE_BYTES {
            self.reply(ctx, &from, &t("that_playlist_file_is_too_big")).await;
            return;
        }
        let Some(voice) = voice_channel(ctx, guild, from.user()) else {
            self.reply(ctx, &from, &t("join_a_voice_channel_first")).await;
            return;
        };
        let bytes = match attachment.download().await {
            Ok(bytes) => bytes,
            Err(err) => {
                self.reply(ctx, &from, &format!("Couldn't download that file: {err}")).await;
                return;
            }
        };
        let text = crate::features::decode(&bytes);
        let format = crate::playlist_file::Format::from_path(std::path::Path::new(&attachment.filename))
            .unwrap_or_else(|| crate::playlist_file::sniff(&text));
        let allow_private = self.core.ui.settings().owner.trim() == from.user().to_string();
        let (items, report) =
            crate::features::items_from_text(&text, format, None, crate::features::library_file, allow_private).await;
        let name = escape(&attachment.filename);
        if items.is_empty() {
            let why = if report.missing > 0 { " Files on someone's PC only play if they're in the owner's music library." } else { "" };
            self.reply(ctx, &from, &format!("Nothing in {name} could be played.{why}")).await;
            return;
        }
        let answered = self.enqueue(ctx, &from, guild, Some(voice), items, false).await;
        let mut note = report.sentence(&name);
        if report.missing > 0 {
            note.push_str(" (Files only play from the owner's music library.)");
        }
        // A plain "Queued N tracks" is the announce. Skipped files and dead links still get said.
        if import_followup(self.core.ui.settings().quiet, report.missing, report.failed_links) {
            self.reply(ctx, &from, &note).await;
        } else if !answered {
            self.drop_deferred_slash(ctx, &from).await;
        }
    }

    /// `export [m3u|m3u8|pls|xspf] [saved playlist]`: the queue or a saved
    /// playlist as a file. Library and PC files are left out: no paths in Discord.
    async fn export(&self, ctx: &Context, from: &From<'_>, guild: GuildId, arg: &str) {
        let (format, name) = export_args(arg);
        let items = if name.is_empty() {
            self.core.queue_items(guild).await
        } else {
            match self.core.store.load_playlist(guild.get(), &name) {
                Ok(items) => items,
                Err(err) => return self.reply(ctx, from, &err).await,
            }
        };
        if items.is_empty() {
            self.reply(ctx, from, &t("the_queue_is_empty_export_name")).await;
            return;
        }
        let (text, left_out) = crate::playlist_file::write(&items, format, false);
        if left_out == items.len() {
            self.reply(ctx, from, &t("those_are_all_files_from_the")).await;
            return;
        }
        let base = if name.is_empty() { "lantern-queue".to_string() } else { name.replace(' ', "-") };
        let file = serenity::all::CreateAttachment::bytes(text.into_bytes(), format!("{base}.{}", format.ext()));
        let mut note = format!("{} track{}.", items.len() - left_out, if items.len() - left_out == 1 { "" } else { "s" });
        if left_out > 0 {
            note.push_str(&format!(" {left_out} local file{} left out (export from the window to keep them).", if left_out == 1 { "" } else { "s" }));
        }
        match from {
            From::Message(msg) => {
                let _ = msg.channel_id.send_message(&ctx.http, CreateMessage::new().content(note).add_file(file)).await;
            }
            From::Slash(cmd) => {
                let _ = cmd.edit_response(&ctx.http, EditInteractionResponse::new().content(note).new_attachment(file)).await;
            }
        }
    }

    /// `skip` from someone who isn't the owner or a DJ: a vote, when at least
    /// three people are listening. Half of them (rounded up) skips the song.
    async fn vote_skip(&self, ctx: &Context, guild: GuildId, user: UserId) -> Result<String, String> {
        let Some(channel) = bot_channel(&self.core, guild).await else { return Err("Nothing is playing.".into()) };
        let people = listeners(ctx, guild, channel);
        if people.len() < VOTE_SKIP_MIN_LISTENERS {
            return Err(format!(
                "Only the owner or a DJ can skip. With {VOTE_SKIP_MIN_LISTENERS} or more people listening, `skip` starts a vote."
            ));
        }
        let needed = votes_needed(people.len());
        let (votes, title) = self.core.add_skip_vote(guild, user.get(), &people.iter().map(|u| u.get()).collect::<Vec<_>>()).await?;
        if votes >= needed {
            let done = self.core.skip(guild).await?;
            return Ok(format!("{done} ({votes}/{needed} votes)"));
        }
        Ok(format!("Vote to skip **{}**: {votes}/{needed}. Others in the channel can `skip` too.", escape(&title)))
    }

    async fn save(&self, guild: GuildId, name: &str) -> Result<String, String> {
        let items: Vec<Item> = {
            let guilds = self.core.guilds.lock().await;
            let state = guilds.get(&guild.get()).ok_or("Nothing to save. Play something first.")?;
            state.now.iter().map(|n| n.item.clone()).chain(state.queue.upcoming.iter().cloned()).collect()
        };
        self.core.store.save_playlist(guild.get(), name, items)
    }

    fn dj(&self, ctx: &Context, from: &From<'_>, guild: GuildId, arg: &str) -> Result<String, String> {
        let arg = arg.trim();
        if arg.is_empty() {
            return Ok(match self.core.store.guild(guild.get()).dj_role {
                Some(role) => format!("The DJ role is <@&{role}>. `dj off` removes it."),
                None => "No DJ role is set. `dj @Role` sets one.".into(),
            });
        }
        if matches!(arg.to_ascii_lowercase().as_str(), "off" | "none" | "clear" | "remove") {
            let (_, save_err) = self.core.store.update(guild.get(), |d| d.dj_role = None);
            if let Some(err) = save_err {
                return Err(format!("DJ role removed in memory, but couldn't save lantern-data.json: {err}"));
            }
            return Ok("DJ role removed.".into());
        }
        let mentioned = match from {
            From::Message(msg) => msg.mention_roles.first().copied(),
            From::Slash(_) => None,
        };
        let known: Vec<(RoleId, String)> = ctx
            .cache
            .guild(guild)
            .map(|g| g.roles.values().map(|r| (r.id, r.name.clone())).collect())
            .unwrap_or_default();
        let role = find_role(mentioned, arg, &known).ok_or("I couldn't find that role. Mention it, like `dj @DJ`.")?;
        let (_, save_err) = self.core.store.update(guild.get(), |d| d.dj_role = Some(role.get()));
        if let Some(err) = save_err {
            return Err(format!("DJ role set in memory, but couldn't save lantern-data.json: {err}"));
        }
        Ok(format!("<@&{}> members can now control Lantern in this server.", role.get()))
    }

    async fn queue(&self, ctx: &Context, from: &From<'_>, guild: GuildId) {
        let data = self.core.store.guild(guild.get());
        let autoplay = match (data.autoplay, data.soundcloud_autoplay) {
            (false, _) => "off",
            (true, false) => "on",
            (true, true) => "on (SoundCloud)",
        };
        let text = {
            let guilds = self.core.guilds.lock().await;
            match guilds.get(&guild.get()) {
                Some(state) if state.now.is_some() || !state.queue.is_empty() => {
                    let mut lines = Vec::new();
                    if let Some(now) = &state.now {
                        let time = match now.length {
                            Some(len) => format!(" ({} / {})", clock(state.position), clock(len)),
                            None => String::new(),
                        };
                        lines.push(format!("**Now:** {}{time}", escape(&now.title)));
                    }
                    for (index, item) in state.queue.upcoming.iter().take(15).enumerate() {
                        let len = item.length.map(|l| format!(" · {}", clock(l))).unwrap_or_default();
                        lines.push(format!("`{}.` {}{len}", index + 1, escape(&item.title)));
                    }
                    if state.queue.len() > 15 {
                        lines.push(format!("…and {} more", state.queue.len() - 15));
                    }
                    lines.push(format!(
                        "\nLoop: {} · Autoplay: {}",
                        state.queue.loop_mode.label(),
                        autoplay
                    ));
                    lines.join("\n")
                }
                _ => "The queue is empty.".into(),
            }
        };
        self.send_embed(ctx, from, note_embed(&text).title("Queue")).await;
    }

    async fn np(&self, ctx: &Context, from: &From<'_>, guild: GuildId) {
        match self.core.np_parts(guild).await {
            Some((_, embed, _)) => {
                self.send_embed(ctx, from, embed).await;
            }
            None => self.reply(ctx, from, &t("nothing_is_playing")).await,
        }
    }

    async fn history(&self, ctx: &Context, from: &From<'_>, guild: GuildId, arg: &str) {
        let played = self.core.store.history(guild.get());
        if played.is_empty() {
            self.reply(ctx, from, &t("nothing_has_played_here_yet")).await;
            return;
        }
        let pages = played.len().div_ceil(15);
        let page = arg.trim().parse::<usize>().unwrap_or(1).clamp(1, pages);
        let lines: Vec<String> = played
            .iter()
            .enumerate()
            .skip((page - 1) * 15)
            .take(15)
            .map(|(i, p)| format!("`{}.` {} · <t:{}:R>", i + 1, escape(&p.item.title), p.at))
            .collect();
        let mut embed = note_embed(&lines.join("\n")).title("Recently played");
        if pages > 1 {
            embed = embed.footer(CreateEmbedFooter::new(format!("Page {page} of {pages} · history {}", (page % pages) + 1)));
        }
        self.send_embed(ctx, from, embed).await;
    }

    /// `replay 3`: queue song 3 from `history` again (1 = the latest).
    async fn replay(&self, ctx: &Context, from: From<'_>, guild: GuildId, arg: &str) {
        let n = if arg.trim().is_empty() { Some(1) } else { arg.trim().parse::<usize>().ok() };
        let played = self.core.store.history(guild.get());
        let Some(item) = n.filter(|n| *n >= 1).and_then(|n| played.get(n - 1)).map(|p| p.item.clone()) else {
            self.reply(ctx, &from, &t("say_which_one_from_history_like")).await;
            return;
        };
        let Some(voice) = voice_channel(ctx, guild, from.user()) else {
            self.reply(ctx, &from, &t("join_a_voice_channel_first")).await;
            return;
        };
        self.enqueue(ctx, &from, guild, Some(voice), vec![item], true).await;
    }

    /// Puts back the queue from before a restart (1.7).
    async fn restore(&self, ctx: &Context, from: From<'_>, guild: GuildId) {
        let (items, err) = self.core.store.take_saved_queue(guild.get());
        if let Some(err) = err {
            self.core.note_data_save_failed(&err);
        }
        if items.is_empty() {
            self.reply(ctx, &from, &t("resume_none")).await;
            return;
        }
        let count = items.len();
        let voice = match voice_channel(ctx, guild, from.user()) {
            Some(voice) => Some(voice),
            None => bot_channel(&self.core, guild).await,
        };
        self.reply(ctx, &from, &crate::i18n::tf("resume_done", &[&count.to_string()])).await;
        self.enqueue(ctx, &from, guild, voice, items, false).await;
    }

    /// `fav` hearts what's playing; `fav list`, `fav play` (shuffled), `fav remove N`.
    async fn fav(&self, ctx: &Context, from: From<'_>, guild: GuildId, arg: &str, controller: bool) {
        let user = from.user();
        let (word, rest) = split_cmd(arg);
        match word.to_ascii_lowercase().as_str() {
            "" | "add" | "this" => {
                let text = self.core.favorite_now(guild, user.get()).await.unwrap_or_else(|e| e);
                self.reply(ctx, &from, &text).await;
            }
            "list" | "show" => {
                let favs = self.core.store.favorites(guild.get(), user.get());
                if favs.is_empty() {
                    self.reply(ctx, &from, &t("no_favorites_yet_press_on_the")).await;
                    return;
                }
                let pages = favs.len().div_ceil(15);
                let page = rest.trim().parse::<usize>().unwrap_or(1).clamp(1, pages);
                let lines: Vec<String> = favs
                    .iter()
                    .enumerate()
                    .skip((page - 1) * 15)
                    .take(15)
                    .map(|(i, item)| format!("`{}.` {}", i + 1, escape(&item.title)))
                    .collect();
                let mut embed = note_embed(&lines.join("\n")).title("Your favorites");
                embed = embed.footer(CreateEmbedFooter::new(format!("Page {page} of {pages} · fav play shuffles them · fav remove N")));
                self.send_embed(ctx, &from, embed).await;
            }
            "play" | "shuffle" => {
                if !controller && !in_playback_channel(ctx, &self.core, guild, user).await && bot_channel(&self.core, guild).await.is_some() {
                    self.reply(ctx, &from, &t("join_my_voice_channel_to_add")).await;
                    return;
                }
                let mut favs = self.core.store.favorites(guild.get(), user.get());
                if favs.is_empty() {
                    self.reply(ctx, &from, &t("no_favorites_yet_press_on_the_2")).await;
                    return;
                }
                fastrand::shuffle(&mut favs);
                let Some(voice) = voice_channel(ctx, guild, user) else {
                    self.reply(ctx, &from, &t("join_a_voice_channel_first")).await;
                    return;
                };
                let count = favs.len();
                let answered = self.enqueue(ctx, &from, guild, Some(voice), favs, false).await;
                if routine_channel_post(self.core.ui.settings().quiet) {
                    self.reply(ctx, &from, &format!("Shuffled your {count} favorite{} into the queue.", if count == 1 { "" } else { "s" })).await;
                } else if !answered {
                    self.drop_deferred_slash(ctx, &from).await;
                }
            }
            "remove" | "rm" | "delete" => {
                let text = match rest.trim().parse::<usize>() {
                    Ok(n) => self.core.store.remove_favorite(guild.get(), user.get(), n),
                    Err(_) => Err("Say which one from `fav list`, like `fav remove 2`.".into()),
                };
                self.reply(ctx, &from, &text.unwrap_or_else(|e| e)).await;
            }
            _ => self.reply(ctx, &from, &t("use_fav_adds_what_s_playing")).await,
        }
    }

    async fn playlists(&self, ctx: &Context, from: &From<'_>, guild: GuildId, settings: &Settings) {
        let list = self.core.store.playlists(guild.get());
        let p = &settings.prefix;
        let text = if list.is_empty() {
            format!("No saved playlists yet. `{p}save name` saves the current queue.")
        } else {
            let mut lines: Vec<String> =
                list.iter().map(|(name, n)| format!("**{name}** · {n} track{}", if *n == 1 { "" } else { "s" })).collect();
            lines.push(format!("\n`{p}load name` plays one. `{p}delete name` removes it."));
            lines.join("\n")
        };
        self.send_embed(ctx, from, note_embed(&text).title("Saved playlists")).await;
    }

    async fn help(&self, ctx: &Context, from: &From<'_>, settings: &Settings) {
        let p = &settings.prefix;
        let text = format!(
            "**Play**\n\
             `{p}play` song or link · YouTube (videos and playlists), SoundCloud, Spotify, Apple Music, MP3 links, radio\n\
             A search shows the top 4: react or type 1–4. `{p}play sc:` words searches SoundCloud\n\
             `{p}play local:` words or `{p}local` words · a song from the owner's music library\n\
             `{p}lyrics` · words for what's playing, or `{p}lyrics` song name\n\
             `{p}radio` words · find an internet radio station (also tag:jazz, country:Germany), pick 1–4\n\n\
             **Control**\n\
             `{p}pause` `{p}resume` `{p}skip` `{p}stop` (3+ listening: anyone's `{p}skip` is a vote) · `{p}volume` 0–200\n\
             `{p}seek` 1:30 · `{p}forward` 10 · `{p}rewind` 10\n\
             `{p}loop` off/track/queue · `{p}autoplay` on/off/soundcloud/youtube · `{p}sleep` 30m / end / off\n\n\
             **Queue**\n\
             `{p}fav` · `{p}fav list` · `{p}fav play` · `{p}fav remove` 2 · or ❤️ on Now playing\n\
             `{p}queue` · `{p}np` · `{p}remove` 3 · `{p}move` 5 1 · `{p}clear` · `{p}shuffle`\n\n\
             **Saved**\n\
             `{p}save` name · `{p}load` name · `{p}playlists` · `{p}delete` name · `{p}history` · `{p}replay` 3\n\
             `{p}import` + attach .m3u/.pls/.xspf · `{p}export` [pls/xspf] [name] · Winamp and VLC lists\n\n\
             **Server**\n\
             `{p}dj` @Role · gives that role full control here (owner or admin only)\n\n\
             Every command also works as a slash command, like `/play`."
        );
        self.send_embed(ctx, from, note_embed(&text).title("Lantern")).await;
    }

    /// Posts the top results and waits until that person reacts or types a number.
    /// The chosen track, and the picker message when Quiet left it on screen.
    async fn ask(&self, ctx: &Context, from: &From<'_>, hits: &[Hit], title: &str) -> Option<(Item, Option<Message>)> {
        let user = from.user();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.core
            .picks
            .lock()
            .await
            .insert(user, PendingPick { channel: from.channel(), message: None, count: hits.len(), tx });
        let Some(message) = self.send_embed(ctx, from, picker_embed(hits, title)).await else {
            drop(rx);
            drop_own_pick(&mut *self.core.picks.lock().await, user);
            return None;
        };
        if let Some(pending) = self.core.picks.lock().await.get_mut(&user) {
            pending.message = Some(message.id);
        }
        for mark in CHOICE_EMOJI.iter().take(hits.len()).chain(std::iter::once(&CANCEL_EMOJI)) {
            if message.react(&ctx.http, ReactionType::Unicode((*mark).to_string())).await.is_err() {
                self.core.ui.log("Couldn't add choice reactions. People can still type a number.");
                break;
            }
        }
        let chosen = match tokio::time::timeout(Duration::from_secs(45), rx).await {
            Ok(Ok(Some(index))) => hits.get(index).map(|hit| hit.item.clone()),
            _ => None,
        };
        drop_own_pick(&mut *self.core.picks.lock().await, user);
        let held = match picker_followup(chosen.as_ref().map(|item| item.title.as_str()), self.core.ui.settings().quiet) {
            Some(text) => {
                let _ = message.channel_id.edit_message(&ctx.http, message.id, EditMessage::new().embed(note_embed(&text))).await;
                let _ = message.delete_reactions(&ctx.http).await;
                None
            }
            None => {
                // Keep the message so a later error can replace it. Reactions go, so it can't be picked again.
                let _ = message.delete_reactions(&ctx.http).await;
                Some(message)
            }
        };
        chosen.map(|item| (item, held))
    }

    async fn finish_pick(&self, user: UserId, message: Option<MessageId>, channel: ChannelId, choice: Option<usize>) {
        let mut picks = self.core.picks.lock().await;
        let Some(pending) = picks.get(&user) else { return };
        if pending.channel != channel {
            return;
        }
        if let (Some(expected), Some(got)) = (pending.message, message) {
            if expected != got {
                return;
            }
        }
        if choice.is_some_and(|index| index >= pending.count) {
            return;
        }
        if let Some(pending) = picks.remove(&user) {
            let _ = pending.tx.send(choice);
        }
    }

    /// A bare "1".."4" answers an open picker. Anything else is a normal message.
    async fn take_typed_choice(&self, msg: &Message) -> bool {
        let Some(choice) = choice_number(msg.content.trim()) else { return false };
        if !self.core.picks.lock().await.contains_key(&msg.author.id) {
            return false;
        }
        self.finish_pick(msg.author.id, None, msg.channel_id, choice).await;
        true
    }

    /// The now-playing card's buttons. Same permission as the matching commands.
    async fn button(&self, ctx: &Context, component: &ComponentInteraction) {
        let Some(action) = component.data.custom_id.strip_prefix("lantern:") else { return };
        let private = |text: &str| {
            CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().content(text).ephemeral(true))
        };
        let Some(guild) = component.guild_id else {
            let _ = component.create_response(&ctx.http, private("Use the buttons in a server.")).await;
            return;
        };
        if action == "fav" {
            // Anyone may heart a song for themselves.
            let text = self.core.favorite_now(guild, component.user.id.get()).await.unwrap_or_else(|e| e);
            let _ = component.create_response(&ctx.http, private(&text)).await;
            return;
        }
        let settings = self.core.ui.settings();
        let roles = component.member.as_ref().map(|m| m.roles.clone()).unwrap_or_default();
        if !self.is_controller(guild, component.user.id, &roles, &settings) {
            let _ = component.create_response(&ctx.http, private("Only the owner or a DJ can use these buttons.")).await;
            return;
        }
        let core = &self.core;
        let (card, playing) = {
            let guilds = core.guilds.lock().await;
            let state = guilds.get(&guild.get());
            (state.and_then(|s| s.np_message), state.is_some_and(|s| s.now.is_some()))
        };
        match card_state(card, playing, component.message.id) {
            Card::Current => {}
            Card::Posting => {
                let _ = component.create_response(&ctx.http, private("One moment, the new Now playing message is on its way.")).await;
                return;
            }
            Card::Old => {
                // An old message must not steer the song that is playing now. Take its buttons away.
                let strip = CreateInteractionResponse::UpdateMessage(CreateInteractionResponseMessage::new().components(Vec::new()));
                let _ = component.create_response(&ctx.http, strip).await;
                let note = CreateInteractionResponseFollowup::new()
                    .content("That Now playing message is out of date. Use the newest one.")
                    .ephemeral(true);
                let _ = component.create_followup(&ctx.http, note).await;
                return;
            }
        }
        // Acknowledge before the work: stopping or redrawing the card can take longer than the 3 seconds Discord allows.
        let _ = component.create_response(&ctx.http, CreateInteractionResponse::Acknowledge).await;
        let result = match action {
            "pause" => {
                let paused = core.guilds.lock().await.get(&guild.get()).map(|s| s.paused).unwrap_or(false);
                core.set_paused(guild, !paused).await
            }
            "skip" => core.skip(guild).await,
            "stop" => {
                core.leave(guild).await;
                core.ui.log(stopped_by(&component.user.name, "the Stop button"));
                Ok("Stopped.".into())
            }
            "loop" => Ok(core.set_loop(guild, None).await),
            "shuffle" => core.shuffle(guild).await,
            _ => return,
        };
        match (&result, action) {
            (Err(err), _) => {
                let _ = component
                    .create_followup(&ctx.http, CreateInteractionResponseFollowup::new().content(err).ephemeral(true))
                    .await;
            }
            (Ok(text), action) if button_shout_out(action, settings.quiet) => {
                core.say(Some(component.channel_id), &format!("{text} (by <@{}>)", component.user.id)).await;
            }
            _ => {}
        }
    }

    /// Slash commands are deferred before the work. When Quiet has nothing to
    /// post, remove that placeholder. Prefix commands never sent one.
    async fn drop_deferred_slash(&self, ctx: &Context, from: &From<'_>) {
        if let From::Slash(cmd) = from {
            let _ = cmd.delete_response(&ctx.http).await;
        }
    }

    async fn send_embed(&self, ctx: &Context, from: &From<'_>, embed: CreateEmbed) -> Option<Message> {
        match from {
            From::Message(msg) => msg.channel_id.send_message(&ctx.http, CreateMessage::new().embed(embed)).await.ok(),
            From::Slash(cmd) => {
                cmd.edit_response(&ctx.http, EditInteractionResponse::new().embed(embed)).await.ok()?;
                cmd.get_response(&ctx.http).await.ok()
            }
        }
    }

    async fn reply(&self, ctx: &Context, from: &From<'_>, text: &str) {
        self.send_embed(ctx, from, note_embed(text)).await;
    }
}

/// How a clicked now-playing message relates to the one Lantern is keeping up to date.
#[derive(Debug, PartialEq, Eq)]
enum Card {
    Current,
    /// Left over: Lantern has a newer card, or nothing is playing at all.
    Old,
    /// A song is playing but its card hasn't been recorded yet. Leave the buttons alone.
    Posting,
}

fn card_state(current: Option<(ChannelId, MessageId)>, playing: bool, clicked: MessageId) -> Card {
    match current {
        Some((_, id)) if id == clicked => Card::Current,
        Some(_) => Card::Old,
        None if playing => Card::Posting,
        None => Card::Old,
    }
}

/// Forgets this person's open picker once its answer has been taken or has timed out.
/// A newer search by the same person may have replaced it meanwhile; that one is
/// still waiting (its receiver is alive), so it stays.
fn drop_own_pick(picks: &mut HashMap<UserId, PendingPick>, user: UserId) {
    if picks.get(&user).is_some_and(|pending| pending.tx.is_closed()) {
        picks.remove(&user);
    }
}

/// The role someone meant by `dj <something>`: a mention, an ID, or a name. An
/// ID that isn't a role in this server is refused, or nobody could control Lantern.
fn find_role(mentioned: Option<RoleId>, arg: &str, roles: &[(RoleId, String)]) -> Option<RoleId> {
    let digits: String = arg.chars().filter(|c| c.is_ascii_digit()).collect();
    let by_id = digits
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .map(RoleId::new)
        .filter(|id| roles.iter().any(|(role, _)| role == id));
    let name = arg.trim().trim_start_matches('@');
    mentioned
        .or(by_id)
        .or_else(|| roles.iter().find(|(_, role_name)| role_name.eq_ignore_ascii_case(name)).map(|(id, _)| *id))
}

// -------------------------------------------------------------------- slash

/// One slash command per prefix command, with typed options.
pub fn slash_commands() -> Vec<CreateCommand> {
    let text = |name: &str, about: &str, required: bool| {
        CreateCommandOption::new(CommandOptionType::String, name, about).required(required)
    };
    let number = |name: &str, about: &str, required: bool, min: u64, max: u64| {
        CreateCommandOption::new(CommandOptionType::Integer, name, about)
            .required(required)
            .min_int_value(min)
            .max_int_value(max)
    };
    let plain = |name: &str, about: &str| CreateCommand::new(name).description(about);
    vec![
        plain("play", "Play a song, link, or playlist").add_option(text("query", "Song name or link", true)),
        plain("local", "Play a song from the owner's music library")
            .add_option(text("song", "Title, artist, or album", true).set_autocomplete(true)),
        plain("lyrics", "Show lyrics for what's playing, or for a song")
            .add_option(text("song", "Song name (blank: what's playing; live: timed lyrics that follow the song)", false)),
        plain("skip", "Skip the current track"),
        plain("pause", "Pause"),
        plain("resume", "Resume"),
        plain("stop", "Stop, clear the queue, and leave"),
        plain("queue", "Show the queue"),
        plain("np", "Show what's playing"),
        plain("volume", "Show or set the volume").add_option(number("level", "0–200", false, 0, 200)),
        plain("remove", "Remove a song from the queue").add_option(number("position", "Number from /queue", true, 1, 10_000)),
        plain("move", "Move a song in the queue")
            .add_option(number("from", "Current number", true, 1, 10_000))
            .add_option(number("to", "New number", true, 1, 10_000)),
        plain("clear", "Empty the queue"),
        plain("shuffle", "Shuffle the queue"),
        plain("loop", "Loop off, this track, or the queue").add_option(
            text("mode", "off, track, or queue", false)
                .add_string_choice("off", "off")
                .add_string_choice("track", "track")
                .add_string_choice("queue", "queue"),
        ),
        plain("seek", "Jump to a time in the track").add_option(text("time", "Like 1:30", true)),
        plain("forward", "Skip ahead").add_option(number("seconds", "Default 10", false, 1, 36_000)),
        plain("rewind", "Go back").add_option(number("seconds", "Default 10", false, 1, 36_000)),
        plain("radio", "Find an internet radio station and play it")
            .add_option(text("search", "Station name, or tag:jazz / country:Germany", true)),
        plain("autoplay", "Keep playing related songs when the queue ends").add_option(
            text("state", "on, off, or where related songs come from", false)
                .add_string_choice("on", "on")
                .add_string_choice("off", "off")
                .add_string_choice("soundcloud", "soundcloud")
                .add_string_choice("youtube", "youtube"),
        ),
        plain("sleep", "Stop and leave after a while")
            .add_option(text("time", "Like 30m, 1h, end (after this song), or off", false)),
        plain("save", "Save the current queue as a playlist").add_option(text("name", "Playlist name", true)),
        plain("load", "Play a saved playlist").add_option(text("name", "Playlist name", true)),
        plain("playlists", "List saved playlists"),
        plain("import", "Queue a playlist file (.m3u, .m3u8, .pls, .xspf)").add_option(
            CreateCommandOption::new(CommandOptionType::Attachment, "file", "Playlist from Winamp, VLC, or Lantern").required(true),
        ),
        plain("export", "Get the queue or a saved playlist as a file")
            .add_option(
                text("format", "File type (default m3u8)", false)
                    .add_string_choice("m3u8", "m3u8")
                    .add_string_choice("m3u", "m3u")
                    .add_string_choice("pls", "pls")
                    .add_string_choice("xspf (VLC)", "xspf"),
            )
            .add_option(text("playlist", "Saved playlist (blank: the queue)", false)),
        plain("delete", "Delete a saved playlist").add_option(text("name", "Playlist name", true)),
        plain("fav", "Your favorites: add what's playing, list, play shuffled, remove")
            .add_option(
                text("action", "Blank: add what's playing", false)
                    .add_string_choice("add what's playing", "add")
                    .add_string_choice("list", "list")
                    .add_string_choice("play shuffled", "play")
                    .add_string_choice("remove", "remove"),
            )
            .add_option(number("number", "For remove: number from the list", false, 1, 200)),
        plain("history", "Recently played songs").add_option(number("page", "Page", false, 1, 10)),
        plain("restore", "Put back the queue from before Lantern restarted"),
        plain("replay", "Play a song from history again").add_option(number("number", "Number from /history (default 1, the latest)", false, 1, 50)),
        plain("dj", "Set the DJ role (owner or admin)")
            .add_option(CreateCommandOption::new(CommandOptionType::Role, "role", "Members with this role control Lantern"))
            .add_option(CreateCommandOption::new(CommandOptionType::Boolean, "off", "Remove the DJ role")),
        plain("help", "Show Lantern commands"),
    ]
}

fn dj_role_deleted(dj: Option<u64>, deleted: u64) -> bool {
    dj == Some(deleted)
}

/// Flattens slash options into the same text a prefix command would get.
/// Discord sends options in the order they were filled in, so `/move to:1 from:5`
/// would read as `move 1 5`. They're put back in the order the command lists them.
fn slash_arg(command: &CommandInteraction) -> String {
    let mut parts: Vec<(String, String)> = Vec::new();
    for option in &command.data.options {
        let text = match &option.value {
            CommandDataOptionValue::String(text) => text.clone(),
            CommandDataOptionValue::Integer(n) => n.to_string(),
            CommandDataOptionValue::Role(role) => format!("<@&{role}>"),
            CommandDataOptionValue::Boolean(true) => option.name.clone(),
            _ => continue,
        };
        parts.push((option.name.clone(), text));
    }
    ordered_slash_arg(&command.data.name, parts)
}

/// Option names per slash command, in the order they're declared.
fn declared_options(command: &str) -> Vec<String> {
    static ORDER: std::sync::OnceLock<HashMap<String, Vec<String>>> = std::sync::OnceLock::new();
    let order = ORDER.get_or_init(|| {
        slash_commands()
            .into_iter()
            .filter_map(|c| serde_json::to_value(&c).ok())
            .filter_map(|json| {
                let name = json["name"].as_str()?.to_string();
                let options = json["options"].as_array().map(|o| o.iter().filter_map(|o| o["name"].as_str().map(str::to_string)).collect()).unwrap_or_default();
                Some((name, options))
            })
            .collect()
    });
    order.get(command).cloned().unwrap_or_default()
}

fn ordered_slash_arg(command: &str, mut parts: Vec<(String, String)>) -> String {
    let order = declared_options(command);
    parts.sort_by_key(|(name, _)| order.iter().position(|o| o == name).unwrap_or(usize::MAX));
    parts.into_iter().map(|(_, text)| text).collect::<Vec<_>>().join(" ")
}

/// `export pls road trip` → (Pls, "road trip"). The format word may come first or last.
pub(crate) fn export_args(arg: &str) -> (crate::playlist_file::Format, String) {
    use crate::playlist_file::Format;
    let mut words: Vec<&str> = arg.split_whitespace().collect();
    let mut format = Format::M3u8;
    if let Some(f) = words.first().and_then(|w| Format::from_ext(w)) {
        format = f;
        words.remove(0);
    } else if let Some(f) = words.last().and_then(|w| Format::from_ext(w)) {
        format = f;
        words.pop();
    }
    (format, words.join(" "))
}

/// `local:words` → the words. `None` for anything else.
pub(crate) fn local_query(query: &str) -> Option<&str> {
    let query = query.trim();
    let head = query.get(..6)?;
    if head.eq_ignore_ascii_case("local:") {
        Some(query[6..].trim())
    } else {
        None
    }
}

/// Up to 25 autocomplete choices: (shown label, key). The key is a short
/// hash, never a path.
pub(crate) fn library_choices(typed: &str) -> Vec<(String, String)> {
    crate::library::with(|index| {
        index
            .search(typed, 25)
            .into_iter()
            .map(|track| {
                let mut label: String = track.label().chars().take(100).collect();
                if label.trim().is_empty() {
                    label = "Untitled".into();
                }
                (label, track.key())
            })
            .collect()
    })
}

// ------------------------------------------------------------------- embeds

pub fn note_embed(text: &str) -> CreateEmbed {
    CreateEmbed::new().description(text).colour(ACCENT)
}

pub(crate) fn np_embed(now: &Now, paused: bool, loop_mode: LoopMode, next: Option<&str>, queued: usize, volume: i32) -> CreateEmbed {
    let title = escape(&now.title);
    let line = match now.item.link() {
        Some(link) if now.item.kind != Kind::Search => format!("**[{title}]({link})**"),
        _ => format!("**{title}**"),
    };
    let length = match (now.item.kind, now.length) {
        (Kind::Radio, _) => "Live".to_string(),
        (_, Some(len)) => clock(len),
        _ => "?".into(),
    };
    let next = match next {
        Some(next) if queued > 1 => format!("{} (+{} more)", escape(next), queued - 1),
        Some(next) => escape(next),
        None => "Nothing queued".into(),
    };
    CreateEmbed::new()
        .title(if paused { "Paused" } else { "Now playing" })
        .description(line)
        .field("Length", length, true)
        .field("Loop", loop_mode.label(), true)
        .field("Volume", format!("{volume}%"), true)
        .field("Up next", next, false)
        .colour(if paused { CYAN } else { ACCENT })
        .footer(CreateEmbedFooter::new(format!("Lantern {}", crate::VERSION)))
}

pub fn np_buttons(paused: bool, loop_mode: LoopMode) -> Vec<CreateActionRow> {
    let button = |id: &str, label: &str, emoji: &str, style: ButtonStyle| {
        CreateButton::new(format!("lantern:{id}"))
            .label(label)
            .emoji(ReactionType::Unicode(emoji.into()))
            .style(style)
    };
    vec![CreateActionRow::Buttons(vec![
        if paused {
            button("pause", "Resume", "▶️", ButtonStyle::Success)
        } else {
            button("pause", "Pause", "⏸️", ButtonStyle::Primary)
        },
        button("skip", "Skip", "⏭️", ButtonStyle::Secondary),
        button("stop", "Stop", "⏹️", ButtonStyle::Danger),
        button(
            "loop",
            &format!("Loop: {}", loop_mode.label()),
            if loop_mode == LoopMode::Track { "🔂" } else { "🔁" },
            if loop_mode == LoopMode::Off { ButtonStyle::Secondary } else { ButtonStyle::Primary },
        ),
        button("shuffle", "Shuffle", "🔀", ButtonStyle::Secondary),
    ]),
    CreateActionRow::Buttons(vec![button("fav", "Favorite", "❤️", ButtonStyle::Secondary)])]
}

fn picker_embed(hits: &[Hit], title: &str) -> CreateEmbed {
    let lines: Vec<String> = hits
        .iter()
        .take(4)
        .enumerate()
        .map(|(index, hit)| {
            let length = hit.item.length.map(clock).unwrap_or_default();
            let meta = [hit.channel.as_str(), length.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" · ");
            let meta = if meta.is_empty() { String::new() } else { format!("\n{meta}") };
            format!("{}  **{}**{meta}", CHOICE_EMOJI[index], escape(&hit.item.title))
        })
        .collect();
    CreateEmbed::new()
        .title(title)
        .description(lines.join("\n\n"))
        .footer(CreateEmbedFooter::new("React, or type 1–4. ❌ cancels."))
        .colour(ACCENT)
}

// ------------------------------------------------------------------ helpers

fn split_cmd(rest: &str) -> (&str, &str) {
    let rest = rest.trim();
    match rest.split_once(char::is_whitespace) {
        Some((cmd, arg)) => (cmd, arg.trim()),
        None => (rest, ""),
    }
}

fn strip_mention<'a>(content: &'a str, ctx: &Context) -> Option<&'a str> {
    let me = ctx.cache.current_user().id;
    [format!("<@{me}>"), format!("<@!{me}>")]
        .iter()
        .find_map(|pattern| content.strip_prefix(pattern.as_str()))
        .map(str::trim)
}

fn voice_channel(ctx: &Context, guild: GuildId, user: UserId) -> Option<ChannelId> {
    ctx.cache.guild(guild)?.voice_states.get(&user).and_then(|state| state.channel_id)
}

async fn bot_channel(core: &Core, guild: GuildId) -> Option<ChannelId> {
    let call = core.songbird.get(guild)?;
    let current = call.lock().await.current_channel();
    current.map(|id| ChannelId::new(id.0.get()))
}

/// True when this person is in a voice channel, and in Lantern's channel if it is already in one.
async fn in_playback_channel(ctx: &Context, core: &Core, guild: GuildId, user: UserId) -> bool {
    let Some(theirs) = voice_channel(ctx, guild, user) else { return false };
    match bot_channel(core, guild).await {
        Some(ours) => ours == theirs,
        None => true,
    }
}

fn channel_is_empty(ctx: &Context, guild: GuildId, channel: ChannelId) -> bool {
    listeners(ctx, guild, channel).is_empty()
}

/// People (not bots) in a voice channel.
fn listeners(ctx: &Context, guild: GuildId, channel: ChannelId) -> Vec<UserId> {
    let Some(guild) = ctx.cache.guild(guild) else { return Vec::new() };
    guild
        .voice_states
        .values()
        .filter(|state| {
            state.channel_id == Some(channel)
                && state
                    .member
                    .as_ref()
                    .map(|m| !m.user.bot)
                    .or_else(|| guild.members.get(&state.user_id).map(|m| !m.user.bot))
                    .unwrap_or(true)
        })
        .map(|state| state.user_id)
        .collect()
}

/// Fewest listeners for a vote-skip; below this only the owner or a DJ can skip.
pub(crate) const VOTE_SKIP_MIN_LISTENERS: usize = 3;

/// Votes needed to skip: half the people listening, rounded up.
pub(crate) fn votes_needed(listeners: usize) -> usize {
    listeners.div_ceil(2).max(1)
}

/// A URL already points at audio. A plain title is a search and can offer a choice.
fn is_direct(query: &str) -> bool {
    let query = query.trim().trim_start_matches('<');
    query.starts_with("http://") || query.starts_with("https://") || crate::media::soundcloud_search_words(query).is_some()
}

/// Voice is not joined when there is nothing to queue.
/// Where new songs go: the asker's voice channel, else the one Lantern is
/// already playing in. `None` means hold them until the asker joins voice.
pub(crate) fn queue_target(user: Option<ChannelId>, bot: Option<ChannelId>) -> Option<ChannelId> {
    user.or(bot)
}

pub(crate) fn empty_enqueue_message(items: &[Item]) -> Option<&'static str> {
    items.is_empty().then_some("There's nothing in that to play.")
}

/// The `announce` reply in `enqueue` ("Queued …"). Callers that pass `false`
/// answer on their own (import, fav play, restore). Quiet drops a success announce.
pub(crate) fn enqueue_announces(announce: bool, quiet: bool) -> bool {
    announce && routine_channel_post(quiet)
}

/// Skip and shuffle buttons post "… (by @user)" into the channel. Other buttons don't.
/// Errors are a separate ephemeral reply and are not this.
pub(crate) fn button_shout_out(action: &str, quiet: bool) -> bool {
    matches!(action, "shuffle" | "skip") && routine_channel_post(quiet)
}

/// The Ready message that offers `restore`. The activity log is separate.
pub(crate) fn post_restore_offer(quiet: bool) -> bool {
    routine_channel_post(quiet)
}

/// Text that replaces a search picker.
/// `None` means delete it: Quiet is on and a song was chosen, so there is no "Queued" line.
/// Cancelling still says so.
pub(crate) fn picker_followup(title: Option<&str>, quiet: bool) -> Option<String> {
    match title {
        Some(title) if routine_channel_post(quiet) => Some(format!("Queued **{}**.", escape(title))),
        Some(_) => None,
        None => Some("No track chosen.".into()),
    }
}

/// `/play` is deferred up front. When the only reply would have been a routine
/// announce, delete that placeholder. A limit note or a prefix command is left alone.
pub(crate) fn drop_slash_placeholder(is_slash: bool, announce: bool, quiet: bool, already_replied: bool) -> bool {
    is_slash && announce && quiet && !already_replied
}

/// Quiet held the search picker. Drop it on a silent success.
/// A prefix error is its own new message, so the picker goes either way.
/// A slash error reuses that message, so it stays.
pub(crate) fn drop_held_picker(is_slash: bool, enqueue_answered: bool) -> bool {
    !is_slash || !enqueue_answered
}

/// Discord `import`'s follow-up. Quiet hides a plain "Queued N tracks".
/// Files that weren't found and links that wouldn't load are still reported.
pub(crate) fn import_followup(quiet: bool, missing: usize, failed_links: usize) -> bool {
    !quiet || missing > 0 || failed_links > 0
}

/// `Some(Some(n))` picks result n. `Some(None)` cancels. `None` is not a choice.
pub(crate) fn choice_number(text: &str) -> Option<Option<usize>> {
    match text.trim() {
        "1" | "1️⃣" => Some(Some(0)),
        "2" | "2️⃣" => Some(Some(1)),
        "3" | "3️⃣" => Some(Some(2)),
        "4" | "4️⃣" => Some(Some(3)),
        "❌" => Some(None),
        other if other.eq_ignore_ascii_case("cancel") || other.eq_ignore_ascii_case("c") => Some(None),
        _ => None,
    }
}


/// The log line for Stop (1.8.3): who stopped Lantern and how.
pub(crate) fn stopped_by(user: &str, via: &str) -> String {
    format!("Stopped by {user} via {via}")
}

#[cfg(test)]
mod tests {

    #[test]
    fn songs_from_someone_outside_voice_join_the_playing_queue() {
        let (mine, bots) = (ChannelId::new(1), ChannelId::new(2));
        assert_eq!(super::queue_target(None, Some(bots)), Some(bots));
        assert_eq!(super::queue_target(Some(mine), Some(bots)), Some(mine));
        assert_eq!(super::queue_target(None, None), None);
    }

    use super::*;

    #[test]
    fn quiet_drops_routine_announces_and_keeps_real_answers() {
        assert!(enqueue_announces(true, false));
        assert!(!enqueue_announces(true, true), "Queued … stays silent when Quiet is on");
        assert!(!enqueue_announces(false, false), "callers that reply themselves pass announce false");
        assert!(!enqueue_announces(false, true));
        assert!(import_followup(false, 0, 0));
        assert!(!import_followup(true, 0, 0), "a clean import is just another Queued line");
        assert!(import_followup(true, 2, 0));
        assert!(import_followup(true, 0, 1));

        assert!(button_shout_out("skip", false));
        assert!(button_shout_out("shuffle", false));
        assert!(!button_shout_out("skip", true));
        assert!(!button_shout_out("shuffle", true));
        assert!(!button_shout_out("pause", false));
        assert!(!button_shout_out("stop", false));
        assert!(!button_shout_out("loop", false));

        assert!(post_restore_offer(false));
        assert!(!post_restore_offer(true));

        assert_eq!(picker_followup(Some("Song"), false).as_deref(), Some("Queued **Song**."));
        assert_eq!(picker_followup(Some("A*B"), false).as_deref(), Some("Queued **A\\*B**."));
        assert_eq!(picker_followup(Some("Song"), true), None);
        assert_eq!(picker_followup(None, true).as_deref(), Some("No track chosen."));
        assert_eq!(picker_followup(None, false).as_deref(), Some("No track chosen."));

        assert!(drop_slash_placeholder(true, true, true, false));
        assert!(!drop_slash_placeholder(false, true, true, false), "a prefix command never deferred");
        assert!(!drop_slash_placeholder(true, true, false, false), "not quiet: the announce is the reply");
        assert!(!drop_slash_placeholder(true, true, true, true), "a limit note already answered");
        assert!(!drop_slash_placeholder(true, false, true, false), "the caller still replies");
        assert!(drop_held_picker(false, false));
        assert!(drop_held_picker(false, true), "a prefix error is a new message");
        assert!(drop_held_picker(true, false));
        assert!(!drop_held_picker(true, true), "a slash error replaced the picker");
    }

    #[test]
    fn empty_enqueue_is_refused_before_voice() {
        assert_eq!(empty_enqueue_message(&[]), Some("There's nothing in that to play."));
        let one = crate::media::Item {
            title: "a".into(),
            source: "https://youtu.be/a".into(),
            query: None,
            kind: crate::media::Kind::YouTube,
            length: None,
            allow_private_host: false,
            requested_by: None,
        };
        assert_eq!(empty_enqueue_message(&[one]), None);
    }

    #[test]
    fn choice_reactions_map_to_results() {
        assert_eq!(choice_number("1"), Some(Some(0)));
        assert_eq!(choice_number("4️⃣"), Some(Some(3)));
        assert_eq!(choice_number("❌"), Some(None));
        assert_eq!(choice_number("cancel"), Some(None));
        assert_eq!(choice_number("play bustin"), None);
        assert!(is_direct("https://youtu.be/0tdyU_gW6WE"));
        assert!(is_direct("<https://youtu.be/0tdyU_gW6WE>"));
        assert!(!is_direct("bustin"));
    }

    #[test]
    fn radio_searches_stations_but_old_autoplay_words_still_work() {
        for old in ["", "on", "OFF", " soundcloud ", "sc", "youtube", "yt", "yes", "0"] {
            assert!(!radio_means_search("radio", old, false), "!radio {old} is still autoplay");
        }
        assert!(radio_means_search("radio", "jazz", false));
        assert!(radio_means_search("RADIO", "tag:rock country:Germany", false));
        assert!(radio_means_search("radio", "on", true), "/radio always searches");
        assert!(!radio_means_search("autoplay", "jazz", false));
        assert_eq!(lookup("station"), Some(("station", Need::Listener)));
        assert_eq!(lookup("radio"), Some(("autoplay", Need::Controller)));
    }

    #[test]
    fn unknown_commands_are_ignored() {
        assert_eq!(lookup("rank"), None);
        assert_eq!(lookup("!!"), None);
        assert_eq!(lookup("P"), Some(("play", Need::Listener)));
        assert_eq!(lookup("ff"), Some(("forward", Need::Controller)));
        assert_eq!(lookup("dj"), Some(("dj", Need::Admin)));
        assert_eq!(lookup("q"), Some(("queue", Need::Anyone)));
    }

    #[test]
    fn every_command_has_a_slash_command() {
        let slash: Vec<String> = slash_commands()
            .iter()
            .map(|c| serde_json::to_value(c).unwrap()["name"].as_str().unwrap().to_string())
            .collect();
        for name in [
            "play", "load", "queue", "np", "help", "history", "playlists", "volume", "skip", "pause", "resume", "stop",
            "remove", "move", "clear", "shuffle", "loop", "seek", "forward", "rewind", "autoplay", "save", "delete", "dj",
            "local", "lyrics", "import", "export", "replay", "fav", "sleep", "restore",
        ] {
            assert_eq!(lookup(name).map(|l| l.0), Some(name), "{name} is a prefix command");
            assert!(slash.iter().any(|s| s == name), "/{name} is missing");
        }
        assert!(slash.iter().any(|s| s == "radio"), "/radio searches stations (1.10)");
        assert_eq!(slash.len(), 33);
    }

    #[test]
    fn buttons_on_old_cards_are_recognised() {
        let channel = ChannelId::new(5);
        let (clicked, newer) = (MessageId::new(10), MessageId::new(11));
        assert_eq!(card_state(Some((channel, clicked)), true, clicked), Card::Current);
        assert_eq!(card_state(Some((channel, newer)), true, clicked), Card::Old);
        // After a restart nothing is playing, so any card still on screen is left over.
        assert_eq!(card_state(None, false, clicked), Card::Old);
        // A song just started and its card isn't recorded yet: don't strip it.
        assert_eq!(card_state(None, true, clicked), Card::Posting);
    }

    #[test]
    fn dj_role_must_exist_in_the_server() {
        let roles = vec![(RoleId::new(10), "DJ".to_string()), (RoleId::new(20), "Mods".to_string())];
        // By ID, as the slash command sends it, and as typed.
        assert_eq!(find_role(None, "<@&10>", &roles), Some(RoleId::new(10)));
        assert_eq!(find_role(None, "20", &roles), Some(RoleId::new(20)));
        // A made-up ID would lock everyone out, so it is refused.
        assert_eq!(find_role(None, "<@&999>", &roles), None);
        assert_eq!(find_role(None, "12abc34", &roles), None);
        assert_eq!(find_role(None, "0", &roles), None);
        // By name, with or without @, any case.
        assert_eq!(find_role(None, "@dj", &roles), Some(RoleId::new(10)));
        assert_eq!(find_role(None, "mods", &roles), Some(RoleId::new(20)));
        assert_eq!(find_role(None, "nobody", &roles), None);
        // A mention in the message already names a real role.
        assert_eq!(find_role(Some(RoleId::new(30)), "<@&30>", &roles), Some(RoleId::new(30)));
        assert_eq!(find_role(None, "", &[]), None);
    }

    fn pick(count: usize) -> (PendingPick, tokio::sync::oneshot::Receiver<Option<usize>>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (PendingPick { channel: ChannelId::new(1), message: None, count, tx }, rx)
    }

    #[test]
    fn a_new_search_is_not_cancelled_by_the_old_one_finishing() {
        let user = UserId::new(42);
        let mut picks: HashMap<UserId, PendingPick> = HashMap::new();
        let (first, first_rx) = pick(4);
        picks.insert(user, first);
        // The same person searches again before answering: the first picker is replaced.
        let (second, second_rx) = pick(4);
        picks.insert(user, second);
        drop(first_rx);
        drop_own_pick(&mut picks, user);
        assert!(picks.contains_key(&user), "the newer search was thrown away");
        // When the newer one times out its receiver is gone and it is cleaned up.
        drop(second_rx);
        drop_own_pick(&mut picks, user);
        assert!(picks.is_empty());
        drop_own_pick(&mut picks, user);
    }

    #[test]
    fn slash_options_follow_the_declared_order_not_the_typed_order() {
        let typed = vec![("to".to_string(), "1".to_string()), ("from".to_string(), "5".to_string())];
        assert_eq!(ordered_slash_arg("move", typed), "5 1");
        let typed = vec![("number".to_string(), "2".to_string()), ("action".to_string(), "remove".to_string())];
        assert_eq!(ordered_slash_arg("fav", typed), "remove 2");
        let typed = vec![("playlist".to_string(), "road trip".to_string()), ("format".to_string(), "pls".to_string())];
        assert_eq!(ordered_slash_arg("export", typed), "pls road trip");
    }

    #[test]
    fn a_deleted_dj_role_is_forgotten() {
        assert!(dj_role_deleted(Some(5), 5));
        assert!(!dj_role_deleted(Some(5), 6));
        assert!(!dj_role_deleted(None, 5));
    }

    #[test]
    fn slash_choices_and_limits_are_valid_for_discord() {
        for command in slash_commands() {
            let json = serde_json::to_value(&command).unwrap();
            let name = json["name"].as_str().unwrap();
            assert!(name.len() <= 32 && name.chars().all(|c| c.is_ascii_lowercase()), "{name}");
            assert!(json["description"].as_str().unwrap().chars().count() <= 100, "{name}");
            let options = json["options"].as_array().cloned().unwrap_or_default();
            assert!(options.len() <= 25, "{name}");
            // Discord rejects a command that lists an optional option before a required one.
            let mut seen_optional = false;
            for option in &options {
                let required = option["required"].as_bool().unwrap_or(false);
                assert!(!(required && seen_optional), "/{name}: required option after an optional one");
                seen_optional |= !required;
                assert!(option["description"].as_str().unwrap().chars().count() <= 100, "{name}");
            }
        }
    }

    #[test]
    fn local_queries_are_recognised() {
        assert_eq!(local_query("local:hey jude"), Some("hey jude"));
        assert_eq!(local_query("LOCAL: Hey"), Some("Hey"));
        assert_eq!(local_query("local:"), Some(""));
        assert_eq!(local_query("locally grown"), None);
        assert_eq!(local_query("https://youtu.be/x"), None);
        assert_eq!(lookup("local"), Some(("local", Need::Listener)));
        assert_eq!(lookup("lyrics"), Some(("lyrics", Need::Anyone)));
    }

    #[test]
    fn vote_skip_needs_half_the_listeners() {
        assert_eq!(votes_needed(3), 2);
        assert_eq!(votes_needed(4), 2);
        assert_eq!(votes_needed(5), 3);
        assert_eq!(votes_needed(0), 1);
        assert_eq!(lookup("skip"), Some(("skip", Need::Listener)), "listeners may vote; controllers still skip at once");
    }

    #[test]
    fn export_arguments() {
        use crate::playlist_file::Format;
        assert_eq!(export_args(""), (Format::M3u8, String::new()));
        assert_eq!(export_args("pls road trip"), (Format::Pls, "road trip".into()));
        assert_eq!(export_args("road trip xspf"), (Format::Xspf, "road trip".into()));
        assert_eq!(export_args("chill"), (Format::M3u8, "chill".into()));
    }

    #[test]
    fn prefix_split() {
        assert_eq!(split_cmd(" play  never gonna "), ("play", "never gonna"));
        assert_eq!(split_cmd("skip"), ("skip", ""));
    }

    #[test]
    fn stop_logs_who_and_how() {
        assert_eq!(super::stopped_by("alice", "!leave"), "Stopped by alice via !leave");
        assert_eq!(super::stopped_by("jev", "the Stop button"), "Stopped by jev via the Stop button");
    }
}
