---
title: "Lantern 1.11.1 Guide"
subtitle: "Self-hosted Discord music bot for Windows"
date: "November 11, 2026"
---

# What Lantern does

Lantern is a Discord music bot that runs on your own Windows PC. It has its own window and a system-tray icon, and it needs no YouTube login. You start it, it joins your voice channel, and anyone in the server can queue music with `!play` or `/play`.

- Plays YouTube links, searches and playlists, SoundCloud, Spotify and Apple Music links (matched to the same song on YouTube), direct audio files, and internet radio.
- Queue tools: remove, move, clear, shuffle, loop, seek, forward and rewind.
- A **Now playing** card in Discord with pause/resume, skip, stop, loop and shuffle buttons. Quiet turns that card off; the window controls stay.
- Autoplay of related songs when the queue runs out, saved playlists, and a history of the last 50 songs.
- An optional owner and a per-server DJ role decide who can control playback.
- Leaves voice by itself after it has been idle for a while.
- If YouTube's normal routes stop working, it falls back to yt-dlp so songs keep playing.
- **1.11.0:** The Docker version's web page has a new look and its own web player: listen on the speakers of any computer or phone, with a queue of its own that doesn't touch Discord.
- **1.10.0:** Search internet radio stations (Radio Browser) with `radio <words>`, `/radio`, or the web page's Radio panel, and browse the music library on the web page.
- **1.8.3:** If the voice connection drops, Lantern pauses, rejoins the same channel, and carries on. There is also a Docker version for a home server (see *Running on a server with Docker*).
- **1.8.2:** Quiet (Settings) cuts routine Discord channel posts. Listening status ("Show the song as the bot status") stays on its own.
- **1.8.1:** A song whose stream ends or dies without the player noticing no longer stalls the queue, and Skip always moves on.
- **1.8.0:** SoundCloud plays again (SoundCloud now sends most songs as HLS, which Lantern downloads and joins), `play sc:` searches SoundCloud, and autoplay can keep going with related SoundCloud songs.
- **1.7.1:** bug fixes: dropped chunked downloads resume, one bad setting no longer wipes the token, an unreadable data file is never overwritten, library scans keep tracks from a share that drops, the tray icon survives an Explorer restart, songs queued from outside voice join the playing queue, and the lookup cache keeps link case.
- **1.6.1:** bug fixes: Skip/Stop answer at once while a song loads, old Shoutcast (ICY) radio plays, slash options are read in the right order, and a deleted DJ role no longer locks the controls.
- **1.6.0:** Winamp/VLC playlist import and export, favorites with a ❤️ button, a sleep timer, vote-skip, a Recently played panel, volume leveling, a smarter "Listening to" status, and a smaller exe.
- **1.5.0:** your own music library (folders on this PC or a network share) searchable from Discord with `/local`, lyrics with `/lyrics`, a *Start with Windows* checkbox, and a one-click permanent install folder.

## What it plays

| Input | How |
| --- | --- |
| YouTube link or search | Streamed in ranged pieces, so it starts fast and can seek. Uses the Vision client, with Android VR and iOS as fallbacks, then yt-dlp if all three fail. |
| YouTube playlist or mix (`list=`) | Imported up to the playlist limit (1-100). A `watch?v=...&list=RD...` link plays just that video. |
| SoundCloud track, set/playlist, or `sc:` words (also `soundcloud:` or `scsearch:`) | Looked up when the track starts so links don't expire. Most SoundCloud songs now come as HLS pieces; Lantern downloads them, joins them, and plays the whole song (seek works). If SoundCloud's own lookup fails and the YouTube backup is on, yt-dlp finds the audio instead. SoundCloud Go+ songs only give a 30-second preview, so they are skipped with a message. |
| Spotify or Apple Music link (including old `itunes.apple.com` links) | The title is searched on YouTube. The result closest in length wins (within 5 s, otherwise 15 s). |
| Direct MP3, M4A, OGG, FLAC, WAV | Streamed from the URL. Seekable if the server supports it. |
| Shoutcast / Icecast, `.pls`, `.m3u`, HLS `.m3u8` | Streamed live (MP3, AAC, Ogg; HLS with AAC, MP3 or MPEG-TS pieces). Live radio can't seek. |
| Links to your own PC or home network (`localhost`, `192.168.x.x`, `10.x`, and similar) | **Owner only** (Owner ID in Settings). Anyone else gets "Only the owner can play LAN links." This stops server members from making Lantern fetch things from inside your network. Redirects to such addresses are checked too. Saved playlists with such links also need the owner to load. |

When YouTube lists the same quality in several languages, Lantern prefers the English original and puts auto-dubbed and audio-description tracks last.

# SoundCloud (1.8.0)

- Paste a SoundCloud track, set or playlist link into `play`, or search with `play sc: artist song` (`/play` takes the same text).
- SoundCloud now sends most songs as HLS (many small pieces). Lantern downloads the pieces and joins them before the song starts, so the whole song plays and seek, forward and rewind work.
- **SoundCloud autoplay:** `autoplay soundcloud` (or `/autoplay state:soundcloud`, or tick **From SoundCloud** next to Autoplay in the window). When the queue runs out, Lantern queues a related SoundCloud song; if there's no SoundCloud song to start from, it searches SoundCloud for the last song's title. Songs in the recent history are skipped so it doesn't repeat. `autoplay youtube` switches back.
- Limits: Go+ songs only play a 30-second preview, so they're skipped (and autoplay never picks them). Private songs can't play.

# New in 1.7.0

- **Setup wizard.** On first launch (no token yet) a wizard walks you through creating the Discord app, pasting the token, inviting the bot with the right permissions, and setting your owner ID. *Setup wizard* under Settings reopens it.
- **Queue before Lantern joins.** Use `!play` or `/play` even when you're not in voice. Lantern says it will wait, and joins you to start playing as soon as you join a voice channel. `stop` clears the waiting songs.
- **Restore after restart.** Lantern remembers the current song and queue. After a restart it posts an offer in the last channel it used; `restore` puts the songs back. Quiet skips that post (the activity log still notes it) and `restore` still works.
- **Live lyrics.** `lyrics live` posts timed lyrics that follow the song, with the current line in bold. Songs without timed lyrics get the plain words.
- **Songs per person / Longest song.** Optional limits in Settings for people who aren't the owner or a DJ. 0 means off (the default).
- **Lookup cache.** Repeated searches and links within 6 hours start faster.
- **Update check.** At startup the window shows a link if a newer release is out. Nothing is downloaded. Turn it off with *Check for updates at startup*.
- **Copy diagnostics.** Under Settings. Copies a report for bug reports with the token, Discord IDs, file paths and network addresses removed. Paste it into an issue.
- **Translations.** Copy `lang/en.json` from the source next to the exe as `lantern-lang.json`, translate the values, and restart. Missing entries stay English.

# Playlists from Winamp and VLC (new in 1.6.0)

- **Import in the window.** Under the queue, **Import playlist** opens a `.m3u`, `.m3u8`, `.pls` or `.xspf` file (Winamp, VLC, or one Lantern exported). Songs saved as file paths play from this PC or the server share, and relative paths are taken from the playlist's folder. Web links in the playlist (YouTube, radio streams, MP3 links) play as if you'd used `play`. Files that have moved or been deleted are skipped, and Lantern tells you how many.
- **Export in the window.** **Export queue** saves the song that's playing plus the queue. Pick the type in the save box: `.m3u8`, `.m3u` (Winamp), `.pls` (Winamp, VLC) or `.xspf` (VLC). Local files keep their full paths, so the list opens straight in Winamp or VLC.
- **From Discord.** `!import` with a playlist file attached (or `/import`) queues it. Links play; file paths play only if that file is in your music library, so nobody can make Lantern open other files on your PC. `!export` sends the queue back as a file, `!export pls` picks the type, and `!export road trip` exports a saved playlist. Discord exports leave out local files, because Lantern never shares paths in Discord. Use the window for those.

# Favorites, sleep timer and vote-skip (new in 1.6.0)

- **Favorites.** Press ❤️ under the Now playing card, or type `!fav`, to save the song for yourself. `!fav list` shows yours, `!fav play` shuffles them into the queue, `!fav remove 2` removes one. Each person has their own list per server (up to 200).
- **Sleep timer.** `!sleep 30m`, `!sleep 1h30m`, or `!sleep end` (after this song). Lantern then stops and leaves voice. `!sleep` shows the time left and `!sleep off` cancels it. Owner or DJ, up to 12 hours.
- **Vote-skip.** When 3 or more people are in Lantern's voice channel, `!skip` from someone who isn't the owner or a DJ counts as a vote. Once half the listeners (rounded up) have voted, the song is skipped. The owner and DJs still skip at once.
- **Recently played.** The window lists the last 20 songs under Now playing. Click one to queue it again. In Discord, `!replay 3` queues song 3 from `!history`.
- **Volume leveling.** *Even out volume between songs* (on by default) uses the ReplayGain tags in your music files and YouTube's own loudness figure, so a loud song doesn't jump out after a quiet one. Loud YouTube videos are turned down; nothing from YouTube is boosted. It applies from the next song.

# Music library (1.5.0)

Folders you add in the window become a library anyone in your server can play from.

- **Add folders** in the window under *MUSIC LIBRARY*: **Add library folder** picks a folder on this PC, and the text box takes any path or network share. The ✕ next to a folder removes it. Only the window can add or remove folders.
- **Scanning.** mp3, flac, m4a, ogg and wav files are read for title, artist, album and length. Files without tags use their file name. The library is scanned when Lantern opens, when you add a folder, and when you press **Rescan**. The count shows beside each folder.
- **Playing from Discord.** Type `/local` and start typing a song, artist or album: Discord suggests up to 25 matches. Or type `!local hey jude` or `!play local:hey jude` to play the best match. Discord only ever sees song names; nobody can type a file path to make Lantern open something.
- **If the share is off or needs a login**, that folder keeps the songs from its last good scan and the window shows the reason in red. If Windows has no saved login for the share, open the share once in File Explorer, tick **Remember my credentials**, then press **Rescan**.
- **Queue from this PC (1.3.0).** The queue panel's **Add files** and **Add folder** buttons still add music straight to the queue without adding it to the library.

# Lyrics (1.5.0)

`!lyrics` (or `/lyrics`) posts the words for the song that's playing. `!lyrics song name` looks up any song. Lyrics come from lrclib.net, a free open lyrics site. Lantern strips YouTube extras like "(Official Video)" from the title and picks the version closest in length. Very long lyrics are cut to fit one message.

# YouTube backup (yt-dlp)

YouTube changes its private API now and then, and one or all of Lantern's normal YouTube routes can stop working. Since 1.2.0, Lantern falls back to yt-dlp (github.com/yt-dlp/yt-dlp) so songs keep playing.

- **Order.** Vision, then Android VR, then iOS, then yt-dlp. If YouTube refuses a route's audio link (403, 410 and so on), Lantern moves to the next route. That also applies when a link is refused as the song starts, before any audio has played.
- **Searches and playlists.** A text search that fails or finds nothing is retried through yt-dlp. A playlist or mix that the normal importer can't read is imported through yt-dlp.
- **Where yt-dlp comes from.** Lantern uses `yt-dlp.exe` from next to `Lantern.exe`, then any `yt-dlp.exe` on PATH. If neither exists, it downloads the official release next to the exe the first time a song needs it, and says so in the activity log. Nothing is downloaded before then. You can also put your own copy next to the exe.
- **Updates.** The copy next to the exe is updated in the background (`yt-dlp -U`) at most once a day. If an update fails and the copy is more than 7 days old, it's downloaded again. A yt-dlp found on PATH is never touched.
- **Which route played.** The activity log says it for every YouTube song, for example "via Android VR" or "via yt-dlp", with the reason each earlier route was skipped. Nothing extra is posted in Discord.
- **Setting.** *YouTube backup (yt-dlp)* in the window, on by default. Turn it off to use only the normal routes. Nothing is then downloaded or run.
- **Audio.** No ffmpeg is needed. yt-dlp is asked for m4a audio first, then WebM/Opus, and runs hidden with a time limit.
- **deno (1.5.0).** YouTube now hides some formats behind JavaScript. After yt-dlp is first used, Lantern downloads the official `deno.exe` next to itself in the background and gives it to yt-dlp. If that download fails, yt-dlp works as before.
- Lantern allows 120 seconds to load a song (was 45), so a slow route can't use up yt-dlp's turn.

# Discord setup

You only do this once per bot.

1. Go to the Discord Developer Portal (discord.com/developers/applications), choose **New Application**, then open **Bot**. Press **Reset Token**, copy the token, and paste it into Lantern's settings. Never share the token or send anyone `lantern-settings.json`.
2. On the same Bot page, turn on **Message Content Intent**. It's the only privileged intent Lantern needs. Presence and Server Members stay off.
3. Invite the bot with the scopes **bot** and **applications.commands**. The second scope is what makes `/play` and the other slash commands appear. Replace `YOUR_APP_ID` with the Application ID from the **General Information** page:

```
https://discord.com/oauth2/authorize?client_id=YOUR_APP_ID&scope=bot%20applications.commands&permissions=3238976
```

`3238976` is the full permission set below. Use `3230784` instead if you'd rather not give Manage Messages.

| Permission | Why |
| --- | --- |
| View Channel, Send Messages, Embed Links | Replies, the queue, and the Now playing card. |
| Read Message History, Add Reactions | The 1-4 reaction picker for searches. |
| Connect, Speak | Joining and playing in voice. |
| Manage Messages (optional) | Lets Lantern clear the picker's reactions afterwards. Everything works without it. |

These permissions must also be allowed in the channels you use. A channel override that hides the channel from the bot's role stops it there.

# Running on a server with Docker (1.8.3, web page 1.9.0, web player 1.11.0)

Lantern can also run without its window, in Docker on a home server such as Unraid. It is the same bot with the same settings, data, commands and autoplay; only the window and tray are missing.

- **One at a time.** A bot token can only be used by one Lantern. Close the Windows app (and untick *Start with Windows*) before the server copy starts, and stop the server copy before you open the Windows app again.
- **Config folder.** On Unraid it is `/mnt/user/appdata/lantern`, mounted as `/config`. It holds `lantern-settings.json` (copied from your Windows folder, token included), `lantern-data.json`, `lantern-log.txt`, and the self-updating yt-dlp.
- **Music library.** Your music share is mounted read-only as `/music` and searched with `/local` and `play local:`. It is rescanned each time the container starts.
- **The web page (1.9).** On Unraid's Docker tab, click Lantern's icon and choose **WebUI** (or open `http://<server>:8094/`). It has no sign-in, so it only works on your home network or Tailscale. Since 1.11 it has six pages: **Listen** (the web player), **Discord** (whether the bot is online, Start and Stop bot, now playing, the queue, recently played), **Library**, **Radio**, **Settings** and **Activity** (the live log). On a computer they're in a sidebar; on a phone, in a bar at the bottom. The web player's mini player stays at the bottom of every page.
- **Web player (1.11).** The **Listen** page plays music in your browser, on the speakers of the computer or phone you have the page open on. It has its own queue, separate from the Discord queue, and the two can play at the same time. Type in the search box and press Enter to search YouTube (start with `sc:` to search SoundCloud), then press ▶ to play now, the list button to play next, or **+** to add to the end; or paste a link (YouTube, SoundCloud, Spotify, Apple Music, a radio stream). On **Library** and **Radio**, ▶ plays a song, folder or station here and **+** adds it to the web queue; the speech-bubble button sends it to Discord instead. Controls: play/pause, previous (restarts the song if it's past 4 seconds), next, a seek bar (not for live radio), shuffle what's up next, repeat (off, whole queue, this song), volume and mute for this device only, drag songs or use the arrows to move them, ✕ to remove, *Clear played* and *Clear*. A song that can't play shows why and moves on after a moment; a dropped station reconnects by itself. On a phone, the lock screen shows the song with play/pause and skip. The queue is saved on the server, so it's still there tomorrow or on another device. *Even out volume* applies here too.
- **Player on the page.** Pause/resume, skip, leave, loop, autoplay and *From SoundCloud*, volume, shuffle, clear, move songs up or down, remove, and click a recently played song to queue it again. *Add* takes a link or a search (`local:` words search your music library); *Add files* takes a file or folder path inside the library, like `/music/Artist/Album`. *Import playlist* and *Export queue* work like the window.
- **Browse music (1.10).** The **Library** page (1.11; *Browse music* before) opens your library folders like a file explorer. Click a folder to open it, use the path at the top to go back, and type in the filter box to narrow the folder you're in. Songs show title, artist and length when the library scan read their tags. The speech-bubble button sends a song or folder (with the folders inside it, up to 1000 songs) to Discord, and *Send to Discord* sends the folder you're in. Songs go to the server chosen on the **Discord** page; Lantern has to be in a voice channel (or have a queue) there. The search box at the top searches the whole library by title, artist, album or file name. Only the library folders can be browsed.
- **Radio (1.10).** The **Radio** page searches internet radio stations from the free Radio Browser directory (radio-browser.info). Fill in a station name, a tag or genre (like `jazz`), a country (like `Germany` or `DE`), or any mix, and press **Search**, or tap one of the genre buttons. Each station shows its logo, country, tags, codec and bitrate. The speech-bubble button queues it in Discord (it plays at once if nothing else is); ▶ and **+** play it in the web player. A station plays live until you skip or stop it; Now playing shows LIVE instead of a length.
- **Settings on the page.** Every setting the window has, with the same limits: token, Owner ID, prefix, default volume, playlist limit, leave when idle, songs per person, longest song, stay in the channel, leave when everyone leaves, song as bot status, Quiet, even out volume, ask which YouTube result, YouTube backup, update check, start the bot when Lantern starts, minimize to tray (Windows only), and the music library folders. Press **Save settings**. The token is never shown, only "Set"; paste a new one to replace it, then press **Stop bot** and **Start**. *Setup wizard* shows the setup steps and your invite link; *Copy diagnostics* copies the redacted report.
- **Starting and stopping.** Start bot / Stop bot on the web page signs the bot in or out while the container keeps running. The container itself: Unraid's Docker tab, or `docker start lantern` / `docker stop lantern`. It starts again by itself after a server reboot, and signs in again by itself if Discord drops the session (unless you pressed Stop bot). If Discord rejects the token, it waits for a new one on the web page.
- **The log.** `lantern-log.txt` in the config folder, or the container's log in Unraid.
- **Back to Windows.** Stop the container and set it not to restart (`docker stop lantern && docker update --restart no lantern`), then open `Lantern.exe` and press **Start bot**.

# Install and update

**First install.** Put `Lantern.exe` in its own folder. Lantern creates its files next to the exe. Run it, paste the token into Settings, and press **Start bot**.

**Updating.** Close Lantern, rename the old exe as a backup (for example `Lantern-1.2.0-backup.exe`), and put the new `Lantern.exe` in the same folder. Your token, settings, playlists and history carry over, because they live in the files beside the exe. Settings files from every earlier version load unchanged.

**Permanent install folder (1.5.0).** Under *STARTUP*, **Install to a permanent folder** copies Lantern, your settings, data, library, yt-dlp and deno to `%LOCALAPPDATA%\Lantern`, adds Lantern to the Start menu, opens the new copy, and closes this one. The original copy is left alone and still works (with its own settings from then on), so you can delete it once you're happy.

**Start with Windows (1.5.0).** Tick **Start Lantern when I sign in to Windows** under *STARTUP*. It starts the copy you ticked it in (the installer moves it to the new copy). Untick to stop.

**Going back.** Close Lantern and run the backup exe from the same folder.

**SmartScreen.** The exe isn't signed, so Windows may warn you the first time. Choose **More info**, then **Run anyway**.

# The window

- **Start bot / Stop bot** connects or disconnects. If the connection fails, the activity log says why (bad token, Message Content intent off, and so on).
- **Server picker** appears when Lantern is in more than one server. Now playing, the controls and the queue follow the selected server.
- **Now playing** shows a progress bar, Pause/Resume, Skip, Leave, a Loop button, an Autoplay checkbox with a **From SoundCloud** box beside it, and a volume slider for that server that applies right away. Below it, **Recently played** lists the last 20 songs; click one to queue it again.
- **Queue** has up, down and remove buttons on each song, plus Shuffle, Clear, Add files, Add folder, **Import playlist** and **Export queue**.
- **Music library** and **Startup** sit above *Save settings* (see those sections).
- **Activity log** shows local times. It's also written to `lantern-log.txt`.
- **Save warning.** If Lantern can't write `lantern-settings.json` or `lantern-data.json` (disk full, folder read-only, file locked), a red banner appears at the top of the window until you press **Dismiss**, and the log records it. Discord commands that save (playlists, volume, DJ role, autoplay) also say when the file couldn't be written.
- **Minimize** hides the window to the tray. **Close** stops the bot and leaves voice.

# Commands

The prefix is `!` by default. Every command is also a slash command (`/play`, `/skip`, ...), and mentioning the bot works as a prefix too. Lantern ignores prefixed words that aren't its commands, so it doesn't clash with other bots.

The **Who** column uses the levels described in the next section.

| Command | Aliases | What it does | Who |
| --- | --- | --- | --- |
| `play <song or link>` | `p` | Queue a song, link or playlist. A search shows 4 results: react 1-4, type the number, or react X to cancel. | Listener |
| `skip` | `s` | Skip the current song. With 3+ people listening, a Listener's `skip` is a vote (half rounded up skips). | Controller (vote: Listener) |
| `pause`, `resume` | | Pause or resume. | Controller |
| `stop` | `leave`, `dc` | Clear the queue and leave voice. | Controller |
| `queue` | `q` | Show the queue. | Anyone |
| `np` | `nowplaying` | Show what's playing. | Anyone |
| `volume` | `vol` | Show the volume. | Anyone |
| `volume <0-200>` | `vol` | Set the volume. Remembered per server. | Controller |
| `remove <n>` | `rm` | Remove song *n* from the queue. | Controller |
| `move <from> <to>` | `mv` | Reorder the queue. | Controller |
| `clear` | | Empty the queue but keep the current song playing. | Controller |
| `shuffle` | | Shuffle the queue. | Controller |
| `loop [off/track/queue]` | `repeat` | Set the loop mode. With no argument it cycles to the next mode. | Controller |
| `seek <1:30>` | | Jump to a time. | Controller |
| `forward [s]`, `rewind [s]` | `ff`, `rw` | Jump 10 s, or the number of seconds given. | Controller |
| `autoplay [on/off/soundcloud/youtube]` | `radio` | Keep playing related songs when the queue runs out, skipping recent repeats. Off by default, saved per server. `autoplay soundcloud` turns it on and takes related songs from SoundCloud (SoundCloud's related tracks, or a SoundCloud search for the last song's title); `autoplay youtube` goes back to YouTube mixes. After a SoundCloud song, autoplay follows SoundCloud either way. After a radio station it stops. `radio` with other words searches stations (see below). | Controller |
| `save <name>` | | Save the current song and queue as a playlist. | Controller |
| `load <name>` | | Queue a saved playlist. | Listener |
| `playlists` | | List saved playlists. | Anyone |
| `delete <name>` | | Delete a saved playlist. | Controller |
| `history [page]` | | The last 50 songs played on this server. | Anyone |
| `replay [n]` | `again` | Queue song *n* from `history` again (default 1, the latest). | Listener |
| `fav` / `fav list` / `fav play` / `fav remove <n>` | `favorite` | Save what's playing to your favorites, list them, shuffle them into the queue, or remove one. ❤️ on the Now playing card does the same as `fav`. | Anyone (`fav play` needs voice) |
| `sleep <30m / 1h / end / off>` | | Stop and leave after a time or after this song. `sleep` alone shows the timer. | Controller |
| `import` + attached file | | Queue a `.m3u`, `.m3u8`, `.pls` or `.xspf` playlist. Paths play only if they're in the music library. | Listener |
| `export [type] [playlist]` | | Get the queue (or a saved playlist) as a playlist file. Local files are left out. | Anyone |
| `dj <@role / off>` | | Set or remove the server's DJ role. | Admin |
| `radio <words>` | `station` | Search internet radio stations (Radio Browser) and pick one of the top 4, like a YouTube search. `tag:jazz` searches a genre, `country:Germany` (or `country:DE`) a country, and they mix: `radio tag:rock country:US`. A name with no matches is tried as a tag. `/radio` does the same. `radio on/off/soundcloud/youtube` still sets autoplay. | Listener |
| `local <words>` | `lib` | Play the best match from the music library. Same as `play local:<words>`. `/local` suggests songs as you type. | Listener |
| `lyrics [song]` | `ly` | Lyrics for what's playing, or for the song named. `lyrics live` follows the song with timed lyrics. | Anyone |
| `restore` | | Put back the queue from before Lantern restarted. | Listener |
| `help` | `commands` | List the commands. | Anyone |

**Now playing buttons.** The card has pause/resume, skip, stop, loop and shuffle buttons, which follow the same rules as the commands, and a ❤️ Favorite button anyone can press (the reply only shows to them). Each new song replaces the card. A button on an old card (an earlier song, or from before a restart) removes itself when pressed instead of acting on the current song. Quiet posts no card and no "Skipped … (by @user)" / "Shuffled … (by @user)" from those buttons; use the commands or the window. `np` still answers when you ask.

## Who can do what

| Level | Who | Commands |
| --- | --- | --- |
| Anyone | Everyone in the server | `queue`, `np`, `help`, `history`, `playlists`, `lyrics`, `fav`, `export`, and `volume` with no number |
| Listener | Anyone in a voice channel, if Lantern is in that channel or not in one yet. Controllers can add songs from any voice channel, and Lantern moves to them. | `play`, `local`, `load`, `import`, `replay`, and `skip` as a vote |
| Controller | The owner (set in the window) and members with the DJ role. If neither is set, everyone. | Everything else |
| Admin | The owner, the server owner, and anyone with Administrator or Manage Server | `dj` |

People who aren't allowed get a short "not allowed" reply.

# Settings

All settings are in the window and saved to `lantern-settings.json`.

| Setting | What it does |
| --- | --- |
| Token | The bot token from the Developer Portal. Kept only in `lantern-settings.json`. |
| Owner ID | Your Discord user ID. Gives you full control everywhere, and is the only user who can play links to your PC or home network. Leave empty if everyone should be able to control playback (unless a DJ role is set). |
| Prefix | The text-command prefix. Default `!`. |
| Default volume | Starting volume (0-200, default 80) for servers that haven't set their own. |
| Playlist limit | How many songs a YouTube playlist or mix imports (1-100, default 25). |
| Leave when idle | Minutes of silence before Lantern leaves voice. Default 5, 0 = never. |
| Stay in the channel after the queue ends | Stay in voice when the queue ends (the idle timer still applies). On by default. |
| Leave when everyone else leaves | Leave when nobody else is left in the voice channel. Off by default. |
| Show the song as the bot status | Show "Listening to" the current song as the bot's Discord status, with "(paused)" while paused. Turning it off clears it at once. On by default. Quiet does not change this. |
| Quiet (less Discord channel spam) | Hide routine channel posts: "Queued …", Now playing cards, skip/shuffle button shout-outs, and the restore offer after a restart. Errors and replies to commands you run still show. Off by default. Listening status is the row above. |
| Even out volume between songs | Volume leveling from ReplayGain tags and YouTube's loudness figure. On by default. |
| Start the bot when this window opens | Connect automatically when Lantern opens. On by default. |
| Minimize to the system tray | Hide to the tray icon when minimized. On by default. |
| Ask which YouTube result to play | Show the 4-result picker for text searches instead of playing the first match. On by default. |
| YouTube backup (yt-dlp) | Use yt-dlp when YouTube's normal routes fail. Lantern downloads it next to the exe the first time it's needed. On by default (also for older settings files). |

Per-server things (volume, DJ role, autoplay, playlists and history) are set from Discord and saved to `lantern-data.json`.

# Files next to the exe

| File | What it is |
| --- | --- |
| `lantern-settings.json` | Window settings and the token. Keep it private. If it can't be read, it's copied to `lantern-settings.json.bak` and defaults are used. |
| `lantern-data.json` | Per-server volume, DJ role, autoplay, saved playlists, history and everyone's favorites. Backed up to `.bak` the same way. |
| `lantern-log.txt`, `lantern-log.old.txt` | The activity log. It rotates to the `.old` file at about 1 MB. |
| `yt-dlp.exe` | Only if the YouTube backup needed it and none was on PATH. You can also put your own copy here. |
| `lantern-library.json` | The music library index (song paths and tags). Safe to delete; Rescan rebuilds it. |
| `deno.exe` | Downloaded after yt-dlp is first used. Helps yt-dlp with YouTube. |
| `lantern-ytdlp-check.txt` | Empty. Its date is the last yt-dlp update check. |
| `lantern-crash.txt` | Only written if Lantern crashes. |

# Troubleshooting

| Problem | What to try |
| --- | --- |
| Start bot fails | Read the activity log. A bad token means you should reset it in the Developer Portal and paste the new one. "Intent" errors mean Message Content Intent is off. |
| `!` commands get no answer | Check Message Content Intent, the prefix in Settings, and that the bot can see and write in that channel. |
| Slash commands don't appear | The invite needs the `applications.commands` scope. Press Ctrl+R in Discord, or re-invite with the link above. |
| "Not allowed" | You're not the owner or in the DJ role. If the DJ role was deleted, a server admin can run `dj off` or set a new role. |
| A YouTube song fails | Check the activity log: it lists each route tried and why it failed. Make sure *YouTube backup (yt-dlp)* is on. Try again or use another link. If it keeps happening, tell the Lantern bot so the YouTube clients can be updated. |
| The log says YouTube wants you to sign in ("Sign in to confirm you're not a bot") | YouTube is blocking yt-dlp from your network for now. Lantern doesn't log in to YouTube, so the song fails as before. Wait a while and try again, restart your router to get a new IP, or avoid VPNs. If it persists, tell the Lantern bot. |
| yt-dlp didn't download | The first download happens only when a song needs the backup. Check the log for the download message. If your network blocks GitHub, put `yt-dlp.exe` next to `Lantern.exe` yourself. |
| A library folder shows red | The folder or share couldn't be opened. For a network share, check the server is on, open the share once in File Explorer with **Remember my credentials**, then **Rescan**. |
| `/local` finds nothing | The library is empty or still scanning. Check the track count in the window. Every word you type must be in the title, artist, album or file name. |
| An imported playlist skipped songs | Those files have moved or been deleted (the message says how many). From Discord, file paths only play if the file is in your music library; import from the window instead. |
| `skip` says only the owner or a DJ can skip | Fewer than 3 people are listening, so there's no vote. |
| No lyrics found | lrclib.net doesn't have that song. Try `!lyrics artist song`. |
| A SoundCloud track is skipped | It is a SoundCloud Go+ song (only a 30-second preview plays without the subscription), it's private or blocked in your country, or it is only offered in a format Lantern can't decode (Opus HLS). Try the same song on YouTube. |
| Search reactions stay on the picker | Give the bot Manage Messages (optional). |
| A red "Couldn't save ..." banner | Lantern couldn't write its settings or data file. Check the disk has space, the folder isn't read-only, and nothing (OneDrive, antivirus, an editor) is holding the file. Changes made since then are only in memory until a save works. |
| "Only the owner can play LAN links" | The link points at a local or private address. Only the Owner ID set in the window can play it. |
| "There's nothing in that to play" | The playlist or link had no playable songs. Lantern no longer joins voice for it. |
| Settings or playlists look reset | Look for the `.bak` file next to the exe. It's the copy of the file that couldn't be read. |
| Lantern closed by itself | Look for `lantern-crash.txt` next to the exe and send it to the Lantern bot. Never send `lantern-settings.json`, since it holds the token. |
| The log says "Voice connection dropped … rejoining" | Discord's voice link died (network blip or a Discord voice server problem). Lantern pauses the song, rejoins the same channel, and carries on. If it can't, it tries again after 5 s, 10 s, 30 s, 1 min, 2 min, then every 5 minutes. `stop` ends the retries. |
| Who stopped the music? | The log says "Stopped by <name> via !stop" (or /stop, !leave, !dc, or the Stop button). |
| Windows warns about the exe | It isn't signed. Choose **More info**, then **Run anyway**. |

# What changed

## 1.11.1 (November 2026)

- Web page: the Discord page's Queue card no longer covers the status and Now playing cards. Narrower windows show one column.

## 1.11.0 (November 2026)

- Web player on the Docker version's web page: listen in the browser on any computer or phone, with its own queue (separate from Discord; both can play at once). YouTube search and links, SoundCloud, Spotify/Apple links, library songs and folders, and radio stations, including the HLS and AAC stations 1.10.1 fixed. Play/pause, previous, next, seek, shuffle, repeat, reorder, per-device volume, lock-screen controls.
- The whole web page is redesigned: dark Lantern colors, a sidebar (a bottom bar on phones), a mini player on every page, cover art, and every setting and tool from before.

## 1.10.1 (November 2026)

- HLS-only radio stations (`.m3u8`, like iHeart stations) play: Lantern follows the playlist and plays its AAC, MP3 or MPEG-TS pieces live. Before, they joined and played nothing.
- `.pls` and `.m3u` station links are unwrapped to the stream inside, and AAC streams with older frame headers decode.
- A stream that can't be decoded now says why in the log and the channel (also with Quiet on) instead of ending silently.

## 1.10.0 (November 2026)

- Browse music on the web page: library folders like a file explorer, with a filter, and queue a song or a whole folder. Browsing can't leave the library folders.
- Internet radio search (Radio Browser): a Radio panel on the web page, and `radio <words>` / `/radio` in Discord with a pick of 4. Stations play live (LIVE, no length limit), reconnect when the stream drops, and autoplay doesn't follow them.

## 1.9.0 (November 2026)

- Web page for the Docker version, opened from Unraid's WebUI button: status, Start/Stop, player and queue controls, add/import/export, every setting (token write-only), music library folders, setup steps, diagnostics, and the live log. No sign-in.
- The Windows app, playback and autoplay are unchanged.

## 1.8.3 (November 2026)

- A dropped voice connection is noticed (driver events, a check before each song, and a check every 15 seconds). Lantern pauses the song, rejoins the same voice channel, and carries on, keeping the queue and autoplay. Failed rejoins back off up to every 5 minutes.
- Stop logs who ran it and how.
- Docker / Linux server version (`lantern-server`) with a Dockerfile and compose file.

## 1.8.2 (November 2026)

- Quiet setting. Routine music-channel posts can be turned off without turning off "Listening to" status.
- Older settings files load with Quiet off.

## 1.8.1 (November 2026)

- Stuck or dead streams advance to the next song (end-of-song and 30 s no-progress watchdog; pause is safe).
- Skip/Stop act within a quarter second even if the stream never reports done.
- A hung player check times out after 3 s.

## 1.8.0 (October 2026)

- SoundCloud plays again: HLS songs are downloaded and joined; yt-dlp is the backup.
- `play sc:` / `soundcloud:` searches SoundCloud and skips Go+ previews.
- SoundCloud autoplay (`autoplay soundcloud`, window **From SoundCloud**), with recent repeats skipped.

## 1.7.1 (October 2026)

- Bug fixes (see 1.7.1 at the top).

## 1.6.1 (October 2026)

- Skip and Stop work at once while a song is still loading.
- Old Shoutcast radio servers that answer "ICY 200 OK" now play.
- `/move`, `/fav` and `/export` read their options in the right order however you fill them in.
- Deleting the DJ role in Discord makes Lantern forget it instead of locking everyone but the owner out.
- `sleep` refuses absurdly large numbers.

## 1.6.0 (October 2026)

- Playlist import and export (.m3u, .m3u8, .pls, .xspf) in the window and Discord.
- "Listening to" status shows paused, survives reconnects, and clears at once when turned off.
- Smaller exe.
- Vote-skip with 3+ listeners; Recently played panel and `replay`; volume leveling; `sleep` timer; favorites with ❤️.

## 1.5.0 (October 2026)

- Music library: folders on this PC or a network share, tag search, `/local` with suggestions, `local` and `play local:`.
- `lyrics` / `/lyrics` from lrclib.net.
- *Start Lantern when I sign in to Windows* checkbox.
- *Install to a permanent folder* (`%LOCALAPPDATA%\Lantern`).
- deno is downloaded for yt-dlp.

## 1.4.0 (October 2026)

- YouTube playlists import past the first page, up to the playlist limit. A watch link inside a playlist starts at that song.

## 1.3.0 (October 2026)

- **Add files** and **Add folder** in the queue panel queue music from this PC. Discord sees only the track name.

## 1.2.1 (October 2026)

A hardening pass on 1.2.0 with no new features.

- Save failures are no longer silent: the log records them, the window shows a red banner until dismissed, and Discord commands that save say when the file couldn't be written.
- Links to private or local addresses (`localhost`, `127.0.0.1`, `10.x`, `192.168.x`, link-local, CGNAT `100.64.x`, and IPv6 equivalents) are refused unless the owner asks. Redirects are checked hop by hop.
- An empty playlist or link no longer makes Lantern join voice first.
- Old `itunes.apple.com` links work like `music.apple.com` links.
- Streams of unknown length that aren't live radio no longer restart from the beginning after a dropped connection (which replayed the song); they now fail with a clear message. Live radio still reconnects.
- 103 automated tests (up from 95).

## 1.2.0 (October 2026)

**New**

- YouTube backup: if YouTube's own routes (Vision, Android VR, iOS) stop working, Lantern falls back to yt-dlp. Songs, searches and playlist/mix imports all have the fallback. Setting *YouTube backup (yt-dlp)*, on by default.
- yt-dlp.exe is taken from next to Lantern.exe, then from PATH, or downloaded once when first needed. The copy next to the exe is updated at most once a day.
- The activity log says which route played each YouTube song and why earlier routes were skipped.

**Fixed**

- A YouTube audio link that YouTube refuses (403/410 or similar) no longer fails the song. Lantern tries the next route, then yt-dlp, also when the link is refused while the song is starting.

**Notes**

- Old settings files load unchanged; the new setting is on.
- New files next to the exe only if used: `yt-dlp.exe` and `lantern-ytdlp-check.txt`.
- Lantern now allows 120 seconds to load a song (was 45).
- 95 automated tests (up from 67).

## 1.1.1 (October 2026)

A hardening pass on 1.1 with no new features.

- Now playing buttons answer Discord right away, so they no longer show "interaction failed". A button on an old card removes its buttons instead of acting on the song that's playing now.
- A jump that arrived before a song's download had started could play audio from the wrong place. It now opens the right part.
- Seeking past the end of a stream (and a few odd MP4 files) now reads as the end instead of failing.
- Idle auto-leave checks again at the moment it leaves, so a song queued just then isn't thrown away.
- Pressing Stop while autoplay was looking for a song no longer lets that song start anyway.
- Searching again before answering the first picker no longer cancels the second search.
- `dj` refuses a role that doesn't exist in the server, so a made-up ID can't lock everyone out.
- Loading an empty saved playlist says so instead of silently doing nothing.
- Settings and saved data are written safely: each save uses its own temporary file and is flushed to disk, files saved by Notepad (with a byte-order mark) load, and an unreadable file is copied to `.bak` before anything overwrites it.
- The log file no longer loses lines when two parts of Lantern write at once.
- Slash commands are sent to Discord once per run instead of on every reconnect. A slash command Lantern no longer has gets a reply instead of "thinking" forever.
- 67 automated tests (up from 39), including streaming tests against a local web server.

## 1.1.0 (October 2026)

**New**

- Queue tools: remove, move, clear, shuffle, and loop (off / track / queue).
- Seek, forward and rewind on YouTube, SoundCloud and files.
- Now playing card with pause/resume, skip, stop, loop and shuffle buttons.
- YouTube playlist and mix import (1-100 songs).
- Autoplay of related songs, saved playlists, and history of the last 50 songs.
- Per-server DJ role and remembered volume.
- Leaves voice after being idle (default 5 minutes).
- Window: server picker, progress bar, queue list, Loop and Autoplay controls, live volume slider.
- Every command is also a slash command.
- Activity log saved to `lantern-log.txt` with local times.
- Spotify and Apple Music links pick the YouTube result whose length matches.

**Fixed**

- The queue no longer freezes after a stop or auto-leave while a song is loading.
- Start works again after a failed connect, and the log says why it failed.
- `!volume` is no longer reset by the next song.
- Pause, resume and skip act on the right server.
- Non-owners no longer get "owner only" for messages meant for other bots.
- All web requests time out instead of hanging. YouTube falls back to other clients.
- SoundCloud works again, and search results show the right song length.
- Stop bot and closing the window leave every voice channel. Being kicked or moved is handled.
- Songs that fail are reported in the text channel.
- A damaged settings file is kept as `.bak` instead of being lost.

**Behavior changes:** `queue`, `np`, `help`, `history` and `playlists` are open to everyone. Everything that changes playback still needs the owner or the DJ role; if neither is set, everyone can do everything, as in 1.0.

# Known limitations

- YouTube may break when it changes its player. The yt-dlp backup usually covers that; otherwise the fix is a client version bump in a new build.
- If YouTube asks yt-dlp to sign in on your network, the backup can't help until YouTube stops asking.
- A YouTube link that dies after audio has started still ends the song.
- Live radio can't seek (also in the web player). The web player sends one stream per song, so a seek starts a new stream and takes a second or two on YouTube songs. SoundCloud Go+ songs (preview only) are skipped. A SoundCloud song is downloaded whole before it starts, so a long DJ mix takes a few seconds longer to begin (up to 300 MB).
- A few edge cases are suspected but not yet confirmed. For example, a slow song lookup can delay Skip or Stop by a few seconds, and some old Shoutcast stations may not play.
