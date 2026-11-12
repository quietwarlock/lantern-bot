//! Lantern, a self-hosted Discord music bot with a Windows window.
//!
//! The window is the `lantern` binary in `main.rs`. Everything Discord and
//! audio can do lives in this library so it can be tested without opening one.
//!
//! - [`bot`] runs the Discord session, per-server state, and the player loop.
//! - [`commands`] handles prefix commands, slash commands, buttons, and voice events.
//! - [`media`] turns a link, a search, or a file on this PC into a stream Songbird can read.
//! - [`stream`] is the seekable HTTP audio source.
//! - [`radiobrowser`] searches internet radio stations (radio-browser.info, 1.10).
//! - [`queue`] holds the pure queue rules (remove, move, shuffle, loop).
//! - [`store`] saves per-server settings, playlists, and history.
//! - [`settings`] is the JSON file saved next to the exe. The token is not compiled in.
//! - [`ui_state`] is the log and snapshot shared by the window and the bot.
//! - [`library`] scans the owner's music folders and searches them.
//! - [`lyrics`] looks up song words on lrclib.net.
//! - [`install`] covers Start with Windows and the permanent install folder.
//! - [`playlist_file`] reads and writes .m3u, .m3u8, .pls and .xspf playlists.
//! - [`features`] holds 1.6 additions: playlist import/export and friends.
//! - [`voice`] notices a dropped voice connection and rejoins (1.8.3).
//! - `web` (server build only) is the browser page Unraid's WebUI button opens (1.9).
//! - `webplayer` and `transcode` (server build only) are the browser's own player and queue (1.11).
//! - [`ytdlp`] finds, downloads, updates, and runs yt-dlp, the YouTube backup.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod bot;
pub mod cache;
pub mod commands;
pub mod diagnostics;
pub mod hls;
pub mod i18n;
pub mod setup;
pub mod library;
pub mod limits;
pub mod lyrics;
pub mod install;
pub mod media;
pub mod playlist_file;
pub mod features;
pub mod queue;
pub mod radiobrowser;
pub mod settings;
pub mod store;
pub mod stream;
pub mod ui_state;
pub mod update;
pub mod voice;
#[cfg(feature = "server")]
pub mod transcode;
#[cfg(feature = "server")]
pub mod web;
#[cfg(feature = "server")]
pub mod webplayer;
pub mod ytdlp;
