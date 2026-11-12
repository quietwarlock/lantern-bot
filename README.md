# Lantern

A self-hosted Discord music bot for Windows. It runs on your own PC with its own window and a system-tray icon, plays YouTube, SoundCloud, Spotify/Apple Music links (matched on YouTube), radio, direct links and your own music folders, and needs no YouTube login.

> The Discord bot token is saved only in `lantern-settings.json` (next to the exe, or in the Docker config folder). Keep that file private. See [SECURITY.md](SECURITY.md).

## Quick start

1. Download `Lantern.exe` from the [Releases](../../releases) page (or [build it](#build)).
2. Put it in its own folder, for example `C:\Lantern`. Lantern creates its settings, data and log files next to the exe.
3. Create a Discord bot and invite it (see [Discord setup](#discord-setup)).
4. Run `Lantern.exe`. The setup wizard opens on first launch; or paste the bot token into **Settings**, optionally your own Discord user ID as **Owner ID**, and press **Start bot**.
5. Join a voice channel in your server and type `!play <song or link>` or `/play`.

The full user guide is in [docs/GUIDE.md](docs/GUIDE.md). Features include a setup wizard, queueing before Lantern joins voice, `restore` after a restart, live lyrics, per-person limits, an update check, Copy diagnostics, and translations (`lang/en.json` → `lantern-lang.json`).

## Discord setup

1. [Discord Developer Portal](https://discord.com/developers/applications) → New Application → **Bot**. Press **Reset Token**, copy it, and paste it into Lantern's Settings. Never share the token or `lantern-settings.json`.
2. On the same Bot page, turn on the **Message Content Intent**. It's the only privileged intent needed. Presence and Server Members stay off.
3. Invite the bot with the scopes **`bot`** and **`applications.commands`** (the second one is what makes `/play` and friends appear). Replace `YOUR_APP_ID` with the Application ID from General Information:

```
https://discord.com/oauth2/authorize?client_id=YOUR_APP_ID&scope=bot%20applications.commands&permissions=3238976
```

`3238976` is the permission set below. Use `3230784` if you'd rather not give Manage Messages.

| Permission | Why |
| --- | --- |
| View Channel, Send Messages, Embed Links | Replies, the queue, and the Now playing card. |
| Read Message History, Add Reactions | The 1️⃣–4️⃣ picker for searches. |
| Connect, Speak | Joining and playing in voice. |
| Manage Messages (optional) | Lets Lantern clear the picker's reactions afterwards. Everything works without it. |

Slash commands are registered globally each time the bot starts. If they don't show up, press Ctrl+R in Discord. The permissions must also be allowed in the channels you use; a channel override that hides it from the bot's role will stop it there.

## What it plays

| Input | How |
| --- | --- |
| YouTube link or search | Innertube player request (Vision client, with Android VR and iOS as fallbacks), then [yt-dlp](#youtube-backup-yt-dlp) if all of those fail. Streamed in ranged pieces, so it starts fast and can seek. |
| YouTube playlist or mix (`list=`, or `/browse/VL…`) | Imported up to the playlist limit. A long playlist is followed past the first page. A link that also names a video starts at that song. A `watch?v=…&list=RD…` link plays just that video. |
| SoundCloud track, set, or `sc:` search | Looked up when the track starts. HLS songs are downloaded and joined; yt-dlp is the backup. Go+ previews are skipped. |
| Spotify or Apple Music link | The title is searched on YouTube. The result closest in length (within 5 s, else 15 s) wins. |
| Direct MP3, M4A, OGG, FLAC, WAV | Streamed from the URL, seekable if the server supports ranges. |
| Files on this PC | **Add files** or **Add folder** in the window. See [Files on this PC](#files-on-this-pc). |
| Shoutcast / Icecast, `.pls`, `.m3u`, HLS `.m3u8` | Streamed live (MP3, AAC, Ogg; HLS with AAC, MP3 or MPEG-TS pieces). Live radio cannot seek. |
| `http://` links to your LAN or PC | Blocked for everyone except the **owner** (Owner ID in Settings). Stops server members from making Lantern fetch `localhost`, `192.168.x.x`, and similar from your PC. |

YouTube often lists the same quality once per language. Lantern prefers the English original, then any English track, and puts autodubs and audio-description tracks last.

## Files on this PC

The queue panel can put music from this computer on the live queue. This is not a separate saved-playlist editor. `save` / `load` still snapshot whatever is queued, including these tracks.

Start the bot first. **Add files** and **Add folder** stay off until Lantern is in a server.

- **Add files.** A normal file picker. You can pick more than one. The filter is audio, but any other file you force through is skipped.
- **Add folder.** One folder. Lantern walks that folder and the folders inside it, up to 32 levels deep. A shortcut that loops back to a folder already seen is skipped.
- **Names starting with `.`** inside a folder are skipped. If you pick that file yourself with **Add files**, it is added.
- **Formats.** mp3, m4a, ogg, opus, flac, wav, aac, webm. Covers, text, and anything else are skipped. If nothing in the selection is audio, the activity log says so and the queue is left alone.
- **Order.** Case doesn't matter, and `2` comes before `10`, so an album stays in track order.
- **Limit.** One add stops at 1000 tracks. The activity log says `Stopped at 1000.` The rest of the folder is not queued.
- **What the track is called.** The file name, without the folder and without the extension. `C:\Music\Whatever\Song Title.mp3` is **Song Title** in the window and in Discord. A file with no name is called **Audio**.

### What Discord sees

People in the server only see that name.

| Place | What they see |
| --- | --- |
| Queue, now playing, history | **Song Title**. No folder, no drive letter, no `.mp3`. |
| The "Queued …" message | `Queued **Song Title**.` or `Queued **12** tracks. First: **Song Title**.` |
| A song that fails | `Couldn't play **Song Title**.` The reason is not included, because it can contain the path. |

The now-playing card is plain text, not a link. There is nothing to click that would show the file.

The path stays on this PC. The activity log uses the track name too, and strips the path out of a playback error. Lantern still has to remember the path in order to open the file. If you `save` that queue, or the song is written into history, the path is in `lantern-data.json` next to the exe. That file is not sent to Discord. `history` and `playlists` in Discord still show the track name only.

### Voice

- If Lantern is already in a voice channel on that server, the new tracks play there. It does not move to you.
- If it isn't in one, and **Owner ID** in Settings is your Discord user ID, and you are in a voice channel, Lantern joins you and deafens itself, the same as `play`.
- If neither is true, nothing is queued. The log says to join a voice channel, or to set Owner ID.
- A Discord message is only posted if Lantern already knows which text channel to use (you used a command there earlier). Playback does not wait on that.

Length isn't known ahead of time, so the window shows how far in you are and Discord shows `?` for length. Seek still tries. Pause, skip, remove, shuffle, and clear work the same as any other song.

## Music library

Folders the owner adds in the window become a searchable library. Anyone who can use `play` can play from it with `/local` (autocomplete suggests songs as you type) or `play local:<words>`.

- **Adding folders.** *Add library folder* picks a folder; the text box takes any path, including a network share (`\\server\share`). ✕ removes a folder. Only the window can change folders. Discord never sends a path: it sends search words, or the short `#key` that autocomplete picked.
- **Scanning.** mp3, flac, m4a, ogg, and wav, with title, artist, album, and length read from the tags (lofty). Untagged files use the file name. Hidden names are skipped. The library is scanned when Lantern opens, when a folder is added, and on *Rescan*. The index is kept in `lantern-library.json`, so search works straight away.
- **Search.** Every word must appear in the title, artist, album, or file name. Exact and leading title matches come first.
- **A share that's off or needs a login.** That folder keeps the tracks from its last good scan, and the window shows why in red. If Windows has no login for the share, open it once in File Explorer, tick *Remember my credentials*, then *Rescan*. A song whose file can't be opened when it's its turn fails like any other bad file.

## Lyrics

`lyrics` shows the words for what's playing (YouTube noise like "(Official Video)" is removed from the title first), and `lyrics <song>` searches for any song. Lyrics come from [lrclib.net](https://lrclib.net), a free open lyrics database; when several match, the one closest in length to the playing song wins. Long lyrics are cut to fit one Discord message.

## Startup and install folder

- **Start Lantern when I sign in to Windows** adds or removes `Lantern` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (this user only), pointing at the exe that's running.
- **Install to a permanent folder** copies Lantern.exe and its files (settings, data, library index, yt-dlp, deno) to `%LOCALAPPDATA%\Lantern`, adds a Start menu shortcut, points Start with Windows there if it was on, opens the new copy, and closes this one. The old copy (for example in Downloads) is left as it was and still works, with its own settings. Installing again only replaces files that are newer here.

## YouTube backup (yt-dlp)

YouTube changes its private API now and then, and one client or all three can stop working. When that happens Lantern falls back to [yt-dlp](https://github.com/yt-dlp/yt-dlp) so songs keep playing.

- **Order.** Vision, Android VR, iOS, then yt-dlp. A client whose audio link YouTube refuses (403, 410, and so on) counts as failed, and Lantern moves to the next. If a link is refused as the song starts, before any audio has played, Lantern tries the next route instead of skipping the song.
- **Also covered.** Text search falls back to `yt-dlp ytsearch4:` when it fails or finds nothing. A playlist is read from the playlist page first (further pages too, up to the limit). A mix uses the watch panel, because its playlist page is empty. Either one falls back to `yt-dlp --flat-playlist -J` when that fails.
- **Where it comes from.** `yt-dlp.exe` next to `Lantern.exe`, then anything named `yt-dlp.exe` on PATH. If neither exists, Lantern downloads the official release from GitHub next to the exe the first time it is needed, and says so in the activity log. Nothing is downloaded until a song needs it.
- **Staying fresh.** The copy next to the exe is updated with `yt-dlp -U` in the background at most once a day, and downloaded again if the update fails and the file is over 7 days old. A yt-dlp found on PATH is never touched. The time of the last check is kept in `lantern-ytdlp-check.txt`.
- **Which route played.** The activity log (not Discord) says it for each YouTube song, for example `Playing Song via Android VR`, or `via yt-dlp (Vision: ..., Android VR: ...)` with the reason each earlier route was skipped.
- **Setting.** *YouTube backup (yt-dlp)* in the window, on by default. Turn it off to use the normal clients only; nothing is downloaded or run. Old settings files load with it on.
- **Audio.** yt-dlp is asked for `bestaudio[ext=m4a]/bestaudio`: m4a when there is one, otherwise WebM/Opus, which Symphonia and Songbird play. No ffmpeg is needed. yt-dlp is run with no console window and a time limit.
- **JavaScript runtime.** YouTube now hides some formats behind JavaScript that yt-dlp solves with [deno](https://deno.com). After yt-dlp is first used, Lantern downloads the official `deno.exe` from deno's GitHub releases next to the exe in the background, and passes it to yt-dlp with `--js-runtimes deno:<path>`. A `deno.exe` already next to the exe or on PATH is used as is. If the download fails, yt-dlp runs without it, as in 1.4.
- If YouTube asks yt-dlp to sign in (common on some networks), the log shows YouTube's message and the song fails as before.

## Commands

The prefix defaults to `!`. Every command is also a slash command (`/play`, `/skip`, …). Mentioning the bot works as a prefix too. Lantern ignores prefixed words that aren't its commands.

| Command | Aliases | What |
| --- | --- | --- |
| `play <song or link>` | `p` | Queue a song, link, or playlist. A search shows 4 results: react 1️⃣–4️⃣, type the number, or ❌. |
| `skip` | `s` | Skip the current track. With 3+ listeners, a non-controller's skip is a vote. |
| `pause`, `resume` | | Pause or resume. |
| `stop` | `leave`, `dc` | Clear the queue and leave the voice channel. |
| `queue` | `q` | Show the queue. |
| `np` | `nowplaying` | Show what's playing. |
| `volume [0–200]` | `vol` | Show or set the volume. It's remembered per server. |
| `remove <n>` | `rm` | Remove song *n* from the queue. |
| `move <from> <to>` | `mv` | Reorder the queue. |
| `clear` | | Empty the queue and keep the current song playing. |
| `shuffle` | | Shuffle the queue. |
| `loop [off/track/queue]` | `repeat` | Set the loop mode. With no argument it cycles to the next mode. |
| `seek <1:30>` | | Jump to a time. |
| `forward [s]`, `rewind [s]` | `ff`, `rw` | Jump 10 s, or the given number of seconds. |
| `autoplay [on/off/soundcloud/youtube]` | `radio` | When the queue runs out, keep playing related songs (YouTube or SoundCloud) and skip recent repeats. Off by default. Not after a radio station. `radio` with other words searches stations. |
| `save <name>` | | Save the current song and queue as a playlist. |
| `load <name>` | | Queue a saved playlist. |
| `playlists` | | List saved playlists. |
| `delete <name>` | | Delete a saved playlist. |
| `history [page]` | | The last 50 songs played on this server. |
| `replay [n]` | `again` | Queue song *n* from `history` again (default 1, the latest). |
| `fav` / `fav list` / `fav play` / `fav remove <n>` | `favorite` | Save what's playing to your favorites, list, play or remove them. ❤️ on the Now playing card works too. |
| `sleep <30m / 1h / end / off>` | | Stop and leave after a time or after this song. |
| `import` + attached file | | Queue a `.m3u`, `.m3u8`, `.pls` or `.xspf` playlist. |
| `export [type] [playlist]` | | Get the queue or a saved playlist as a playlist file. Local files are left out. |
| `dj <@role / off>` | | Set or remove the DJ role. |
| `radio <words>` | `station` | Search internet radio stations (Radio Browser, radio-browser.info) and pick one of the top 4. `tag:jazz` and `country:Germany` narrow it. `/radio` too. |
| `local <words>` | `lib` | Play a song from the music library. Same as `play local:<words>`. `/local` suggests songs as you type. |
| `lyrics [song]` | `ly` | Lyrics for what's playing, or for the named song (lrclib.net). `lyrics live` follows the song with timed lyrics. |
| `restore` | | Put back the queue from before Lantern restarted. |
| `help` | `commands` | List the commands. |

The "Now playing" message has buttons for pause/resume, skip, stop, loop, and shuffle. Each new song replaces the message. The buttons follow the same rules as the commands. A button on an old card (an earlier song, or from before a restart) removes itself when pressed instead of acting on the current song.

### Who can do what

| Level | Who | Commands |
| --- | --- | --- |
| Anyone | Everyone in the server | `queue`, `np`, `help`, `history`, `playlists`, `lyrics`, `volume` with no number |
| Listener | Anyone in a voice channel, if Lantern is in that channel or isn't in one yet. Controllers can add songs from any voice channel, and Lantern moves to them. | `play`, `local`, `load` |
| Controller | The owner (window setting) and members with the DJ role. If neither is set, everyone. | Everything else |
| Admin | The owner, the server owner, and anyone with Administrator or Manage Server | `dj` |

Non-controllers who try a controller command get a short "not allowed" reply.

## Window

- **Start / Stop bot.** Connects or disconnects. If the connection fails, the log says why (bad token, Message Content intent off, and so on).
- **Server picker.** Appears when Lantern is in more than one server. Now playing, controls, and the queue follow the selected server.
- **Now playing** has a progress bar, Pause/Resume, Skip, Leave, a Loop button, an Autoplay checkbox, and a volume slider for that server.
- **Queue** has ⬆ / ⬇ / ✕ on each song, plus Shuffle and Clear. **Add files** and **Add folder** are documented under [Files on this PC](#files-on-this-pc).
- **Activity log** shows local times. It is also written to `lantern-log.txt`, which rotates to `lantern-log.old.txt` at about 1 MB.
- **Settings:** token, owner ID, prefix, default volume, playlist limit (1–100), "leave when idle" in minutes (default 5, 0 = never), songs per person, longest song, stay in channel, leave when alone, song as status, Quiet (fewer channel posts; does not turn off Listening status), even out volume, start on open, minimize to tray, choose from search results, YouTube backup (yt-dlp), and the update check.
- **Music library** and **Startup**: see [Music library](#music-library) and [Startup and install folder](#startup-and-install-folder).
- Minimizing hides the window to the tray. Closing the window stops the bot and leaves voice.

## Files next to the exe

| File | What |
| --- | --- |
| `lantern-settings.json` | Window settings and the token. A file that can't be read is copied to `.bak` and defaults are used. 1.0 and 1.1 files load unchanged. |
| `lantern-data.json` | Per-server volume, DJ role, autoplay, saved playlists, and history. A file that can't be read is copied to `.bak` the same way. |
| `lantern-log.txt`, `lantern-log.old.txt` | The activity log. |
| `yt-dlp.exe` | Only if the YouTube backup needed it and none was on PATH. You can also put your own copy here. |
| `lantern-library.json` | The music library index (paths and tags). Rebuilt by Rescan; safe to delete. |
| `deno.exe` | Only after yt-dlp has been used: the JavaScript runtime yt-dlp uses for YouTube. |
| `lantern-ytdlp-check.txt` | Empty. Its date is the last yt-dlp update check. |
| `lantern-crash.txt` | Only written if the program crashes. |

## Layout

| File | Job |
| --- | --- |
| `src/main.rs` | The window: settings, now playing, queue, log, tray. |
| `src/bot.rs` | Bot runtime, per-server player loop, voice events, idle leave. |
| `src/commands.rs` | Prefix and slash commands, permissions, buttons, embeds. |
| `src/queue.rs` | Queue operations, loop modes, time parsing. |
| `src/store.rs` | `lantern-data.json`: per-server settings, playlists, history. |
| `src/media.rs` | Turns a query or a file on this PC into something playable (YouTube, SoundCloud, Spotify, Apple, radio, local files). |
| `src/stream.rs` | Seekable HTTP audio source with read-ahead and reconnects. |
| `src/ytdlp.rs` | Finds, downloads, updates, and runs yt-dlp; reads its output. |
| `src/settings.rs` | `lantern-settings.json` load, migration, and backup. |
| `src/ui_state.rs` | State shared between the bot thread and the window, plus the log file. |
| `src/tray.rs` | Windows tray icon. |
| `src/server.rs` | `lantern-server`: no window, for Docker and Linux. |
| `src/voice.rs` | Notices a dropped voice connection; the rejoin itself is in `bot.rs`. |
| `build.rs` | Embeds `assets/lantern.ico` into the Windows exe. If no resource compiler is found it prints a warning and goes on. |

## Build

On Windows, install Rust and run `cargo build --release`. The program is `target/release/lantern.exe`. Keep it in its own folder, since its files are created beside it.

To cross-compile from Linux:

```
rustup target add x86_64-pc-windows-gnu
sudo apt install mingw-w64 cmake g++
cargo build --release --target x86_64-pc-windows-gnu
```

The output is `target/x86_64-pc-windows-gnu/release/lantern.exe`.

To run the tests without the window: `cargo test --lib --no-default-features`. They need no internet (the streaming and download tests use a small web server on localhost, and the yt-dlp tests use stand-ins). Tests that talk to YouTube, SoundCloud, Spotify, or a real yt-dlp are marked `#[ignore]`; run them with `-- --ignored`.

## Docker (Unraid or any Linux server)

`lantern-server` is Lantern without the window or tray. It reads the same `lantern-settings.json` and `lantern-data.json`, starts the bot (unless *Start the bot when Lantern starts* is off), writes `lantern-log.txt` (also shown by `docker logs`), and serves a web page that does what the window does, plus a web player for listening in the browser.

**Run only one Lantern per bot token.** Stop and close the Windows app (and turn off *Start with Windows*) before starting the container, or the two fight over the same bot.

Build the image from this folder:

```
docker build -t lantern:1.11.1 .
```

Set up the config folder once (Unraid: `/mnt/user/appdata/lantern`). Copy `lantern-settings.json` and `lantern-data.json` from the Windows folder into it, then give it to Unraid's `nobody` user:

```
mkdir -p /mnt/user/appdata/lantern
chown -R 99:100 /mnt/user/appdata/lantern
chmod 600 /mnt/user/appdata/lantern/lantern-settings.json
```

Start it with `docker compose up -d` using [docker-compose.yml](docker-compose.yml), or without Compose:

```
docker run -d --name lantern --restart unless-stopped --user 99:100 \
  -e TZ=Etc/UTC -e LANTERN_LIBRARY=/music \
  -p 8094:8080 \
  -l net.unraid.docker.managed=dockerman \
  -l 'net.unraid.docker.webui=http://[IP]:[PORT:8080]/' \
  -l net.unraid.docker.icon=http://YOUR-SERVER-IP:8094/icon.png \
  -v /mnt/user/appdata/lantern:/config -v /mnt/user/music:/music:ro \
  lantern:1.11.1
```

Pick any free host port instead of `8094`.

| Setting | What it does |
| --- | --- |
| `/config` volume | Settings, data, log, library index, and yt-dlp. |
| `DISCORD_TOKEN` | Optional. Replaces the token in `lantern-settings.json`. Kept in memory, never written. |
| `LANTERN_LIBRARY` | Music library folders inside the container, comma separated (for example `/music`). Replaces the folders in the settings file, since Windows paths don't exist here. Mount the share read-only. The library is scanned at every start. |
| `TZ` | Time zone for the log. |
| `LANTERN_WEB_PORT` | Port of the web page inside the container. Default `8080`; `off` turns the page off. |

- **yt-dlp and deno** are in the image. On first start yt-dlp is copied into `/config`, where Lantern's daily `yt-dlp -U` keeps it current across image updates. deno is on `PATH`.
- **Check the config without connecting:** `docker run --rm -v /mnt/user/appdata/lantern:/config lantern:1.11.1 --check` reports what it found (never the token) and exits.
- **Stopping:** `docker stop lantern` leaves voice channels cleanly. If Discord drops the session, the server signs in again after 30 seconds, unless *Stop bot* was pressed on the web page. A rejected token or intent waits for a new token and *Start* on the web page (with the page turned off, it exits with code 2).
- **Back to Windows:** `docker stop lantern && docker update --restart no lantern`, then open `Lantern.exe` and press Start.
- Window-only things (tray, Start with Windows, the permanent install folder, file pickers) don't exist in the server build. *Add files* on the page takes a path inside a music library folder instead.

### Web page

Open `http://<server>:<host port>/`. On Unraid: Docker tab → click Lantern's icon → **WebUI**. There is **no sign-in**, so keep the port on your home network or Tailscale; don't forward it from the internet. Pages: **Listen**, **Discord**, **Library**, **Radio**, **Settings** and **Activity** (a sidebar on a computer, a bar at the bottom on a phone), with the web player's mini player always at the bottom.

- **Listen (web player):** plays in the browser, on that computer's or phone's speakers, from a queue of its own that never touches the Discord queue (both can play at once). Search YouTube (`sc:` for SoundCloud) and pick Play now / Play next / Add, or paste any link Lantern takes; add library songs and folders from **Library** and stations from **Radio**. Play/pause, previous, next, seek (not live radio), shuffle what's up next, repeat off / whole queue / this song, drag or move songs, remove, clear played, clear. Volume is per device. The queue is saved in `lantern-webplayer.json`. Audio is sent as one Ogg Opus stream (`/api/web/stream/<id>`), decoded with the same code as the Discord player, so every source and radio format Discord plays works here too. Phone lock-screen controls work through the browser's media session.

- **Status and player:** Start/Stop bot, server picker, now playing with progress, pause/resume, skip, leave, loop, autoplay and *From SoundCloud*, per-server volume, recently played (click to queue again), and the update notice.
- **Queue:** shuffle, clear, move up/down, remove; add by link or search (`local:` words search the library), add a file or folder from the library by path, import a playlist file, export the queue.
- **Settings:** every setting the window has, checked and saved to `lantern-settings.json` the same way and applied the same way (the next command; a new token after Stop bot, then Start). The token is write-only: the page shows only whether one is set. With `DISCORD_TOKEN` set, the variable stays in use and is never written to the file. `LANTERN_LIBRARY` resets the library folders at every container start.
- **Browse music:** click through the library folders (breadcrumbs, folders then songs with title/artist/length from the library index, a filter box) and queue a song, a folder, or the folder you're in into the server chosen in the picker. Only the library folders can be browsed; `..` and symlinks that lead outside are refused.
- **Radio:** search internet radio stations (Radio Browser) by name, tag/genre and country; each shows its logo, country, tags, codec and bitrate; *Queue* plays it live in the chosen server. The server re-looks the station up by its ID, so the page never sends a stream address.
- **Library, setup, diagnostics, log:** add/remove/rescan library folders, the setup steps with your invite link, the redacted diagnostics report, and the live activity log.
- Every change is a JSON `POST`, so another web site can't press the buttons from your browser.

### Unraid template

So the Docker tab's **Edit** screen matches the container, save this as `/boot/config/plugins/dockerMan/templates-user/my-lantern.xml` (change the host port and icon address to yours). Leave the token out; it lives in `lantern-settings.json`.

```xml
<?xml version="1.0"?>
<Container version="2">
  <Name>lantern</Name>
  <Repository>lantern:1.11.1</Repository>
  <Network>bridge</Network>
  <Shell>sh</Shell>
  <Privileged>false</Privileged>
  <Overview>Lantern, a self-hosted Discord music bot (server build with a web page).</Overview>
  <Category>MediaServer:Music</Category>
  <WebUI>http://[IP]:[PORT:8080]/</WebUI>
  <Icon>http://YOUR-SERVER-IP:8094/icon.png</Icon>
  <ExtraParams>--user 99:100 --restart unless-stopped</ExtraParams>
  <Config Name="Web page" Target="8080" Default="8094" Mode="tcp" Type="Port" Display="always" Required="true" Mask="false">8094</Config>
  <Config Name="Config" Target="/config" Default="/mnt/user/appdata/lantern" Mode="rw" Type="Path" Display="always" Required="true" Mask="false">/mnt/user/appdata/lantern</Config>
  <Config Name="Music" Target="/music" Default="/mnt/user/music" Mode="ro" Type="Path" Display="always" Required="false" Mask="false">/mnt/user/music</Config>
  <Config Name="TZ" Target="TZ" Default="Etc/UTC" Mode="" Type="Variable" Display="always" Required="false" Mask="false">Etc/UTC</Config>
  <Config Name="LANTERN_LIBRARY" Target="LANTERN_LIBRARY" Default="/music" Mode="" Type="Variable" Display="always" Required="false" Mask="false">/music</Config>
</Container>
```

To build the server program without Docker: `cargo build --release --no-default-features --features server --bin lantern-server` (needs `cmake`). Set `LANTERN_DIR` to choose the config folder.

## Colors

The window and the Discord embeds use black, magenta `#FF2EA6`, and cyan `#2DE2E6`.

## License

MIT. See [LICENSE](LICENSE).
