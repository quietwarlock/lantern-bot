//! The Lantern window. Closing it stops the bot. Minimizing hides it in the tray.
//!
//! Settings, including the bot token, are written next to the executable.
//! Nothing in this file is sent to Discord except through [`lantern::bot`].

#![windows_subsystem = "windows"]

mod tray;

use eframe::egui::{self, Color32, RichText, Vec2};
use lantern::bot::{self, Action};
use lantern::queue::{clock, LoopMode};
use lantern::settings::{app_dir, settings_path, Settings};
use lantern::ui_state::{GuildView, UiState};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const ACCENT: Color32 = Color32::from_rgb(255, 46, 166);
const CYAN: Color32 = Color32::from_rgb(45, 226, 230);
const MUTED: Color32 = Color32::from_rgb(154, 163, 168);
const TEXT: Color32 = Color32::from_rgb(244, 247, 248);
const BLACK: Color32 = Color32::from_rgb(11, 11, 11);

fn main() -> eframe::Result {
    std::panic::set_hook(Box::new(|info| {
        let _ = std::fs::write(app_dir().join("lantern-crash.txt"), format!("{info}"));
    }));

    let (settings, warning) = Settings::load(&settings_path());
    let ui = UiState::new(settings.clone()).with_log_file(app_dir().join("lantern-log.txt"));
    if let Some(warning) = warning {
        ui.log(warning);
    }
    if let Some(note) = lantern::i18n::load(&app_dir().join(lantern::i18n::OVERRIDE_FILE)) {
        ui.log(note);
    }
    if settings.token.trim().is_empty() {
        ui.log("Welcome! The setup wizard walks you through creating the bot and pasting its token.");
    }
    lantern::library::replace(lantern::library::load_saved(&lantern::library::index_path()));
    let handle = bot::spawn(ui.clone());
    if settings.start_on_open && !settings.token.trim().is_empty() {
        handle.start();
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Lantern")
            .with_inner_size([1040.0, 820.0])
            .with_min_inner_size([920.0, 700.0])
            .with_icon(app_icon()),
        ..Default::default()
    };
    let settings_token = settings.token.clone();
    eframe::run_native(
        "Lantern",
        options,
        Box::new(move |cc| {
            style(cc);
            let logo = load_logo(&cc.egui_ctx);
            let mut app = LanternApp {
                ui,
                handle,
                settings,
                logo,
                tray_events: tray::start(),
                in_tray: false,
                show_token: false,
                save_warning: None,
                suppress_hide_until: Instant::now(),
                selected: None,
                volume_drag: None,
                library_input: String::new(),
                scan: None,
                scan_folders: Vec::new(),
                start_with_windows: lantern::install::start_with_windows().is_some(),
                wizard: lantern::setup::needs_setup(&settings_token).then_some(lantern::setup::Step::CreateApp),
                wizard_error: None,
            };
            app.rescan();
            Ok(Box::new(app))
        }),
    )
}

struct LanternApp {
    ui: UiState,
    handle: bot::Handle,
    settings: Settings,
    /// Decoded once at start, not every frame.
    logo: Option<egui::TextureHandle>,
    tray_events: std::sync::mpsc::Receiver<tray::Event>,
    in_tray: bool,
    show_token: bool,
    /// Ignore a leftover minimized flag right after the tray restores the window.
    suppress_hide_until: Instant,
    /// The server shown in the queue panel.
    selected: Option<u64>,
    /// Volume slider value while it is being dragged: (server, level).
    volume_drag: Option<(u64, i32)>,
    /// Save failure shown as a banner until dismissed.
    save_warning: Option<String>,
    /// A folder typed into the Library box (for network shares).
    library_input: String,
    /// The library scan running in the background.
    scan: Option<std::sync::mpsc::Receiver<lantern::library::Index>>,
    /// The folders the running (or last) scan covered.
    scan_folders: Vec<String>,
    /// Whether Windows starts Lantern at sign-in (read from the Run key).
    start_with_windows: bool,
    /// The first-run setup wizard page, while it's open (1.7).
    wizard: Option<lantern::setup::Step>,
    /// Why the wizard can't go on yet. Never contains the token.
    wizard_error: Option<String>,
}

impl eframe::App for LanternApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(250));
        self.tray(ctx);
        let live = self.ui.live();
        let guilds = self.ui.guilds();
        if !self.selected.is_some_and(|id| guilds.iter().any(|g| g.id == id)) {
            self.selected = guilds.first().map(|g| g.id);
        }
        let view = self.selected.and_then(|id| guilds.iter().find(|g| g.id == id).cloned());
        let status_color = match live.connection.as_str() {
            "Online" => CYAN,
            "Playing" | "Connecting" | "Paused" => ACCENT,
            _ => MUTED,
        };

        egui::TopBottomPanel::top("header").exact_height(92.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(tex) = &self.logo {
                    ui.image((tex.id(), Vec2::splat(68.0)));
                }
                ui.vertical(|ui| {
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Lantern").size(26.0).strong().color(TEXT));
                        ui.label(RichText::new(lantern::VERSION).size(14.0).color(CYAN));
                    });
                    ui.label(RichText::new("YouTube, SoundCloud, Spotify, Apple Music, and radio").size(13.0).color(MUTED));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(16.0);
                    ui.label(RichText::new("●").size(16.0).color(status_color));
                    ui.label(RichText::new(&live.connection).strong().color(TEXT));
                });
            });
            ui.add_space(6.0);
            let rect = ui.max_rect();
            ui.painter().rect_filled(
                egui::Rect::from_min_max(rect.left_bottom(), rect.right_bottom() + Vec2::new(0.0, 2.0)),
                0.0,
                ACCENT,
            );
        });

        if let Some((version, link)) = self.ui.update() {
            egui::TopBottomPanel::top("update").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(CYAN, format!("Lantern {version} is available."));
                    ui.hyperlink_to("Open the release page", link);
                });
            });
        }
        self.wizard_window(ctx);

        egui::TopBottomPanel::bottom("settings").show(ctx, |ui| self.settings_panel(ui));

        egui::SidePanel::left("now").exact_width(330.0).show(ctx, |ui| self.now_panel(ui, &live, &guilds, view.as_ref()));

        if let Some(warning) = self.ui.take_save_warning() {
            self.save_warning = Some(warning);
        }
        if let Some(warning) = self.save_warning.clone() {
            egui::TopBottomPanel::top("save_warning").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(egui::Color32::from_rgb(255, 120, 120), warning);
                    if ui.button("Dismiss").clicked() {
                        self.save_warning = None;
                    }
                });
            });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            let queue_height = (ui.available_height() * 0.5).max(160.0);
            ui.allocate_ui(Vec2::new(ui.available_width(), queue_height), |ui| self.queue_panel(ui, view.as_ref()));
            ui.separator();
            ui.label(RichText::new("ACTIVITY").size(11.0).strong().color(ACCENT));
            ui.add_space(4.0);
            egui::ScrollArea::vertical().id_salt("log").stick_to_bottom(true).show(ui, |ui| {
                for line in self.ui.logs() {
                    ui.label(RichText::new(line).monospace().size(12.5).color(TEXT));
                }
            });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        tray::shutdown();
        // Leave voice channels cleanly before the process ends.
        self.handle.stop_and_wait(Duration::from_secs(4));
    }
}

impl LanternApp {
    fn now_panel(&mut self, ui: &mut egui::Ui, live: &lantern::ui_state::Live, guilds: &[GuildView], view: Option<&GuildView>) {
        ui.add_space(12.0);
        let has_token = !self.settings.token.trim().is_empty();
        let running = matches!(live.connection.as_str(), "Online" | "Playing" | "Connecting" | "Paused");
        ui.horizontal(|ui| {
            let start = egui::Button::new(RichText::new("Start").strong().color(BLACK)).fill(ACCENT);
            if ui.add_enabled(!running && has_token, start).on_disabled_hover_text(if has_token { "Already running" } else { "Paste your bot token first" }).clicked() {
                self.save_settings();
                self.handle.start();
            }
            if ui.add_enabled(running, egui::Button::new(RichText::new("Stop bot").strong())).clicked() {
                self.handle.stop();
            }
        });
        ui.add_space(14.0);

        if guilds.len() > 1 {
            let current = view.map(|v| v.name.clone()).unwrap_or_default();
            egui::ComboBox::from_label(RichText::new("Server").color(MUTED))
                .selected_text(current)
                .width(220.0)
                .show_ui(ui, |ui| {
                    for guild in guilds {
                        ui.selectable_value(&mut self.selected, Some(guild.id), &guild.name);
                    }
                });
            ui.add_space(8.0);
        } else if let Some(view) = view {
            ui.label(RichText::new(&view.name).color(MUTED));
        }

        ui.label(RichText::new("NOW PLAYING").size(11.0).strong().color(ACCENT));
        ui.add_space(6.0);
        let title = view.and_then(|v| v.current.clone()).unwrap_or_else(|| live.title.clone());
        ui.label(RichText::new(title).size(19.0).strong().color(TEXT));
        ui.add_space(4.0);
        match view.filter(|v| v.current.is_some()) {
            Some(v) => {
                let time = match v.length {
                    _ if v.live => format!("LIVE · {}", clock(v.position)),
                    Some(len) => format!("{} / {}", clock(v.position), clock(len)),
                    None => clock(v.position),
                };
                let state = if v.paused { "Paused" } else { "Playing" };
                ui.label(RichText::new(format!("{state} · {time}")).color(if v.paused { CYAN } else { MUTED }));
                if let Some(len) = v.length.filter(|l| *l > 0) {
                    let bar = egui::ProgressBar::new((v.position as f32 / len as f32).min(1.0)).desired_height(4.0).fill(ACCENT);
                    ui.add(bar);
                }
            }
            None => {
                ui.label(RichText::new(&live.detail).color(MUTED));
            }
        }
        ui.add_space(12.0);

        let Some(v) = view else {
            ui.label(RichText::new("Controls appear here once something is queued.").color(MUTED).size(12.0));
            return;
        };
        let id = v.id;
        let playing = v.current.is_some();
        ui.horizontal(|ui| {
            let label = if v.paused { "▶ Resume" } else { "⏸ Pause" };
            if ui.add_enabled(playing, egui::Button::new(RichText::new(label).strong())).clicked() {
                self.handle.act(id, if v.paused { Action::Resume } else { Action::Pause });
            }
            if ui.add_enabled(playing, egui::Button::new(RichText::new("⏭ Skip").strong())).clicked() {
                self.handle.act(id, Action::Skip);
            }
            if ui.button(RichText::new("⏹ Leave").strong()).on_hover_text("Stop, clear the queue, and leave the voice channel").clicked() {
                self.handle.act(id, Action::Leave);
            }
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let loop_text = format!("🔁 Loop: {}", v.loop_mode.label());
            let loop_button = egui::Button::new(RichText::new(loop_text).color(if v.loop_mode == LoopMode::Off { TEXT } else { BLACK }));
            let loop_button = if v.loop_mode == LoopMode::Off { loop_button } else { loop_button.fill(CYAN) };
            if ui.add(loop_button).on_hover_text("Off → this track → whole queue").clicked() {
                self.handle.act(id, Action::Loop(v.loop_mode.next()));
            }
            let mut autoplay = v.autoplay;
            if ui.checkbox(&mut autoplay, "Autoplay").on_hover_text("Keep playing related songs when the queue ends").changed() {
                self.handle.act(id, Action::Autoplay(autoplay));
            }
            let mut from_sc = v.soundcloud_autoplay;
            if ui.checkbox(&mut from_sc, "From SoundCloud").on_hover_text("Autoplay picks related SoundCloud songs instead of YouTube ones").changed() {
                self.handle.act(id, Action::AutoplaySoundCloud(from_sc));
            }
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Volume").color(MUTED));
            let mut level = match self.volume_drag {
                Some((guild, level)) if guild == id => level,
                _ => v.volume,
            };
            let response = ui.add(egui::Slider::new(&mut level, 0..=200).suffix("%"));
            if response.dragged() || response.changed() {
                self.volume_drag = Some((id, level));
            }
            let finished = response.drag_stopped() || (response.changed() && !response.dragged());
            if finished {
                self.handle.act(id, Action::Volume(level));
                self.volume_drag = None;
            }
        });
        ui.label(RichText::new("Applies now and is remembered for this server.").color(MUTED).size(11.5));
        ui.add_space(12.0);
        ui.label(RichText::new("RECENTLY PLAYED").size(11.0).strong().color(ACCENT));
        if v.recent.is_empty() {
            ui.label(RichText::new("Nothing yet.").color(MUTED).size(12.0));
            return;
        }
        egui::ScrollArea::vertical().id_salt("recent").auto_shrink([false, true]).show(ui, |ui| {
            for (at, title) in &v.recent {
                let clicked = ui
                    .add(egui::Label::new(RichText::new(format!("↻ {title}")).color(TEXT)).truncate().sense(egui::Sense::click()))
                    .on_hover_text("Click to queue it again")
                    .clicked();
                if clicked {
                    self.handle.act(id, Action::Replay(*at, title.clone()));
                }
            }
        });
    }

    fn queue_panel(&mut self, ui: &mut egui::Ui, view: Option<&GuildView>) {
        let guild = view.map(|v| v.id);
        ui.horizontal(|ui| {
            ui.label(RichText::new("QUEUE").size(11.0).strong().color(ACCENT));
            if let Some(v) = view {
                ui.label(RichText::new(format!("{} up next", v.upcoming.len())).color(MUTED).size(12.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let has = !v.upcoming.is_empty();
                    if ui.add_enabled(has, egui::Button::new("Clear")).clicked() {
                        self.handle.act(v.id, Action::Clear);
                    }
                    if ui.add_enabled(v.upcoming.len() > 1, egui::Button::new("🔀 Shuffle")).clicked() {
                        self.handle.act(v.id, Action::Shuffle);
                    }
                });
            }
        });
        ui.horizontal(|ui| {
            let files = ui
                .add_enabled(guild.is_some(), egui::Button::new("Add files"))
                .on_hover_text("Pick audio files. Discord only sees each track's name, not where it lives.")
                .clicked();
            let folder = ui
                .add_enabled(guild.is_some(), egui::Button::new("Add folder"))
                .on_hover_text("This folder and the folders inside it. Discord only sees track names.")
                .clicked();
            let import = ui
                .add_enabled(guild.is_some(), egui::Button::new("Import playlist"))
                .on_hover_text("Queue a .m3u, .m3u8, .pls or .xspf playlist from Winamp, VLC, or Lantern. Missing files are skipped and counted.")
                .clicked();
            let export = ui
                .add_enabled(guild.is_some(), egui::Button::new("Export queue"))
                .on_hover_text("Save what's playing and the queue as a playlist Winamp or VLC can open")
                .clicked();
            if import {
                if let (Some(id), Some(path)) = (guild, pick_playlist_file()) {
                    self.handle.act(id, Action::Import(path));
                }
            }
            if export {
                if let (Some(id), Some(path)) = (guild, pick_export_file()) {
                    self.handle.act(id, Action::Export(path));
                }
            }
            if files {
                if let (Some(id), Some(paths)) = (guild, pick_audio_files()) {
                    self.enqueue_paths(id, &paths);
                }
            }
            if folder {
                if let (Some(id), Some(path)) = (guild, pick_audio_folder()) {
                    self.enqueue_paths(id, &[path]);
                }
            }
            if guild.is_none() {
                ui.label(RichText::new("Start the bot to add files.").color(MUTED).size(12.0));
            }
        });
        ui.add_space(4.0);
        let Some(v) = view else {
            ui.label(RichText::new("Nothing queued.").color(MUTED));
            return;
        };
        if v.upcoming.is_empty() {
            ui.label(RichText::new("Nothing queued after this song.").color(MUTED));
            return;
        }
        let count = v.upcoming.len();
        egui::ScrollArea::vertical().id_salt("queue").auto_shrink([false, true]).show(ui, |ui| {
            for (index, title) in v.upcoming.iter().enumerate() {
                let position = index + 1;
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{position:>3}.")).monospace().color(MUTED));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✕").on_hover_text("Remove").clicked() {
                            self.handle.act(v.id, Action::Remove(position));
                        }
                        if ui.add_enabled(position < count, egui::Button::new("⬇").small()).on_hover_text("Move down").clicked() {
                            self.handle.act(v.id, Action::Move(position, position + 1));
                        }
                        if ui.add_enabled(position > 1, egui::Button::new("⬆").small()).on_hover_text("Move up").clicked() {
                            self.handle.act(v.id, Action::Move(position, position - 1));
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add(egui::Label::new(RichText::new(title).color(TEXT)).truncate());
                        });
                    });
                });
            }
        });
    }

    fn enqueue_paths(&self, guild: u64, paths: &[PathBuf]) {
        let (items, truncated) = lantern::media::collect_local_audio(paths);
        if items.is_empty() {
            self.ui.log("No audio files there. Lantern plays mp3, m4a, ogg, opus, flac, wav, aac, and webm.");
            return;
        }
        self.handle.act(guild, Action::Enqueue(items, truncated));
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.label(RichText::new("SETTINGS").size(11.0).strong().color(ACCENT));
        ui.horizontal(|ui| {
            ui.label(RichText::new("Bot token").color(MUTED));
            ui.add(
                egui::TextEdit::singleline(&mut self.settings.token)
                    .password(!self.show_token)
                    .desired_width(460.0)
                    .hint_text("Paste the token from the Discord Developer Portal"),
            );
            ui.checkbox(&mut self.show_token, "Show");
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("Owner ID").color(MUTED));
            ui.add(egui::TextEdit::singleline(&mut self.settings.owner).desired_width(180.0).hint_text("Optional"));
            ui.label(
                RichText::new("Blank: anyone can control Lantern, unless a server sets a DJ role with dj. Anyone in the voice channel can always add songs.")
                    .color(MUTED)
                    .size(12.0),
            );
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("Command prefix").color(MUTED));
            ui.add(egui::TextEdit::singleline(&mut self.settings.prefix).desired_width(48.0));
            ui.add_space(12.0);
            ui.label(RichText::new("Default volume").color(MUTED))
                .on_hover_text("For servers that haven't set their own volume");
            ui.add(egui::Slider::new(&mut self.settings.volume, 0..=200).suffix("%"));
            ui.add_space(12.0);
            ui.label(RichText::new("Playlist limit").color(MUTED));
            ui.add(egui::DragValue::new(&mut self.settings.playlist_limit).range(1..=100));
            ui.add_space(12.0);
            ui.label(RichText::new("Leave when idle").color(MUTED));
            ui.add(egui::DragValue::new(&mut self.settings.idle_minutes).range(0..=240).suffix(" min"))
                .on_hover_text("Leave the voice channel after this many minutes with nothing playing. 0 = never.");
        });
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.settings.stay_in_channel, "Stay in the channel after the queue ends");
            ui.checkbox(&mut self.settings.leave_alone, "Leave when everyone else leaves");
            ui.checkbox(&mut self.settings.song_status, "Show the song as the bot status");
            ui.checkbox(&mut self.settings.quiet, "Quiet (less Discord channel spam)")
                .on_hover_text("Hides Queued messages, Now playing cards, skip and shuffle button posts, and the restore offer after a restart. Listening status is the checkbox beside this one and stays on its own.");
            ui.checkbox(&mut self.settings.level_volume, "Even out volume between songs")
                .on_hover_text("Uses ReplayGain tags in your music files and YouTube's loudness figure, so songs play at a similar level. Applies from the next song.");
        });
        ui.label(
            RichText::new("Quiet only hides routine channel posts. \"Show the song as the bot status\" still sets Listening to the song.")
                .color(MUTED)
                .size(12.0),
        );
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.settings.start_on_open, "Start the bot when this window opens");
            ui.checkbox(&mut self.settings.minimize_to_tray, "Minimize to the system tray");
            ui.checkbox(&mut self.settings.choose_result, "Ask which YouTube result to play");
            ui.checkbox(&mut self.settings.youtube_backup, "YouTube backup (yt-dlp)")
                .on_hover_text("If YouTube stops working the normal way, use yt-dlp. Lantern downloads it beside the exe the first time it is needed.");
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Songs per person").color(MUTED))
                .on_hover_text("How many songs one person (not the owner or a DJ) can have waiting in the queue. 0 = no limit.");
            ui.add(egui::DragValue::new(&mut self.settings.max_per_user).range(0..=1000));
            ui.add_space(12.0);
            ui.label(RichText::new("Longest song").color(MUTED))
                .on_hover_text("Songs longer than this are refused for people who aren't the owner or a DJ. 0 = any length.");
            ui.add(egui::DragValue::new(&mut self.settings.max_song_minutes).range(0..=1440).suffix(" min"));
            ui.add_space(12.0);
            ui.checkbox(&mut self.settings.update_check, "Check for updates at startup")
                .on_hover_text("Asks GitHub whether a newer Lantern is out and says so here. Nothing is downloaded.");
        });
        self.library_panel(ui);
        ui.horizontal(|ui| {
            if ui.button("Save settings").clicked() {
                self.save_settings();
            }
            if ui.button("Setup wizard").clicked() {
                self.wizard = Some(lantern::setup::Step::CreateApp);
                self.wizard_error = None;
            }
            if ui.button("Copy diagnostics").on_hover_text("Copies a report for bug reports. The token, Discord IDs, file paths and network addresses are removed.").clicked() {
                let report = lantern::diagnostics::report(&self.ui.settings(), &self.ui.logs(), &self.ui.live().connection);
                ui.ctx().copy_text(report);
                self.ui.log("Diagnostics copied (token, IDs, paths and addresses removed).");
            }
            ui.label(RichText::new("Changes apply to the next command. Closing this window stops Lantern.").color(MUTED).size(12.0));
        });
        ui.add_space(6.0);
    }

    /// The first-run setup wizard (1.7).
    fn wizard_window(&mut self, ctx: &egui::Context) {
        use lantern::setup::{self, Step};
        let Some(step) = self.wizard else { return };
        let mut open = true;
        let mut next = None;
        egui::Window::new("Set up Lantern").collapsible(false).resizable(false).open(&mut open).anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.label(RichText::new(step.title()).size(16.0).strong().color(ACCENT));
            ui.add_space(6.0);
            match step {
                Step::CreateApp => {
                    ui.label("Open the Discord Developer Portal, press New Application, and give it a name.");
                    ui.hyperlink_to("Open the Developer Portal", setup::PORTAL_URL);
                    ui.label("On the Bot page, turn on Message Content Intent. Leave Presence and Server Members off.");
                }
                Step::Token => {
                    ui.label("On the Bot page press Reset Token and copy it. Paste it here. It is saved only in lantern-settings.json next to Lantern.exe. Never share that file.");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.settings.token).password(!self.show_token).desired_width(380.0));
                        ui.checkbox(&mut self.show_token, "Show");
                    });
                }
                Step::Invite => match setup::app_id_from_token(&self.settings.token) {
                    Some(id) => {
                        let url = setup::invite_url(id);
                        ui.label("Open this link, pick your server, and press Authorize.");
                        ui.hyperlink_to("Invite Lantern to a server", url.clone());
                        if ui.button("Copy invite link").clicked() {
                            ui.ctx().copy_text(url);
                        }
                    }
                    None => {
                        ui.label("Go back and paste the token first.");
                    }
                },
                Step::Owner => {
                    ui.label("Your Discord user ID gives you full control everywhere. In Discord: Settings > Advanced > Developer Mode, then right-click your name > Copy User ID. Leave it blank to let anyone control Lantern until a server sets a DJ role.");
                    ui.add(egui::TextEdit::singleline(&mut self.settings.owner).desired_width(220.0).hint_text("Optional"));
                }
                Step::Finish => {
                    ui.label("Press Save and start. The activity log shows \"Online as …\" when it works, or says what to fix (for example the Message Content intent).");
                }
            }
            if let Some(err) = &self.wizard_error {
                ui.colored_label(Color32::from_rgb(255, 120, 120), err);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if step != Step::CreateApp && ui.button("Back").clicked() {
                    next = Some(Some(step.back()));
                }
                if step == Step::Finish {
                    if ui.button("Save and start").clicked() {
                        next = Some(None);
                    }
                } else if ui.button("Next").clicked() {
                    let check = match step {
                        Step::Token => setup::check_token(&self.settings.token),
                        Step::Owner => setup::check_owner(&self.settings.owner),
                        _ => Ok(()),
                    };
                    match check {
                        Ok(()) => next = Some(Some(step.next())),
                        Err(err) => self.wizard_error = Some(err.to_string()),
                    }
                }
                ui.label(RichText::new(format!("Step {} of {}", step.index() + 1, Step::ALL.len())).color(MUTED));
            });
        });
        if let Some(target) = next {
            self.wizard_error = None;
            self.wizard = target;
            if target.is_none() {
                self.save_settings();
                self.handle.start();
            }
        }
        if !open {
            self.wizard = None;
        }
    }

    fn library_panel(&mut self, ui: &mut egui::Ui) {
        use lantern::library;
        self.finish_scan();
        ui.add_space(6.0);
        ui.label(RichText::new("MUSIC LIBRARY").size(11.0).strong().color(ACCENT));
        let (total, problems) = library::with(|i| (i.tracks.len(), i.problems.clone()));
        let mut remove = None;
        for folder in &self.settings.library_folders {
            ui.horizontal(|ui| {
                if ui.small_button("✕").on_hover_text("Remove this folder from the library").clicked() {
                    remove = Some(folder.clone());
                }
                let count = library::with(|i| i.count_in(folder));
                ui.label(RichText::new(folder).color(TEXT));
                match problems.get(folder) {
                    Some(problem) => {
                        ui.label(RichText::new(format!("{count} tracks · {problem}")).color(Color32::from_rgb(255, 120, 120)).size(12.0));
                    }
                    None => {
                        ui.label(RichText::new(format!("{count} tracks")).color(MUTED).size(12.0));
                    }
                }
            });
        }
        if let Some(folder) = remove {
            self.settings.library_folders.retain(|f| f != &folder);
            self.save_settings();
            self.rescan();
        }
        ui.horizontal(|ui| {
            if ui.button("Add library folder").on_hover_text("Anyone in Discord can play songs from it with /local. They only see song names.").clicked() {
                if let Some(path) = pick_audio_folder() {
                    self.add_library_folder(&path.to_string_lossy());
                }
            }
            ui.add(egui::TextEdit::singleline(&mut self.library_input).desired_width(220.0).hint_text(r"or type a path, like \\server\share"));
            if ui.add_enabled(!self.library_input.trim().is_empty(), egui::Button::new("Add")).clicked() {
                let text = std::mem::take(&mut self.library_input);
                self.add_library_folder(&text);
            }
            let scanning = self.scan.is_some();
            if ui.add_enabled(!scanning && !self.settings.library_folders.is_empty(), egui::Button::new("Rescan")).clicked() {
                self.rescan();
            }
            let status = if scanning { "Scanning…".to_string() } else { format!("{total} tracks") };
            ui.label(RichText::new(status).color(CYAN).size(12.0));
        });
        ui.add_space(6.0);
        ui.label(RichText::new("STARTUP").size(11.0).strong().color(ACCENT));
        ui.horizontal_wrapped(|ui| {
            let mut on = self.start_with_windows;
            if ui.checkbox(&mut on, "Start Lantern when I sign in to Windows").on_hover_text("Adds Lantern to your Windows startup apps for this user only.").changed() {
                let exe = std::env::current_exe().unwrap_or_else(|_| app_dir().join(lantern::install::EXE));
                match lantern::install::set_start_with_windows(on, &exe) {
                    Ok(()) => {
                        self.start_with_windows = on;
                        self.ui.log(if on { "Lantern will start when you sign in to Windows" } else { "Lantern won't start with Windows" });
                    }
                    Err(err) => self.ui.log(format!("Couldn't change Start with Windows: {err}")),
                }
            }
            if let Some(dest) = lantern::install::install_dir() {
                if lantern::install::running_from(&dest) {
                    ui.label(RichText::new(format!("Installed in {}", dest.display())).color(MUTED).size(12.0));
                } else if ui
                    .button("Install to a permanent folder")
                    .on_hover_text(format!("Copies Lantern, its settings, and its data to {}, adds a Start menu shortcut, and opens it there. This copy keeps working.", dest.display()))
                    .clicked()
                {
                    self.install_permanently(ui.ctx(), &dest);
                }
            }
        });
    }

    fn add_library_folder(&mut self, text: &str) {
        let folder = lantern::library::normalize_folder(text);
        if folder.is_empty() || self.settings.library_folders.iter().any(|f| f.eq_ignore_ascii_case(&folder)) {
            return;
        }
        self.settings.library_folders.push(folder.clone());
        self.save_settings();
        self.ui.log(format!("Added {folder} to the music library"));
        self.rescan();
    }

    /// Scans the library folders on a background thread.
    fn rescan(&mut self) {
        if self.scan.is_some() {
            return;
        }
        let folders = self.settings.library_folders.clone();
        if folders.is_empty() {
            lantern::library::replace(Default::default());
            let _ = lantern::library::save(&Default::default(), &lantern::library::index_path());
            return;
        }
        let (send, receive) = std::sync::mpsc::channel();
        self.scan = Some(receive);
        self.scan_folders = folders.clone();
        let previous = lantern::library::with(|i| i.clone());
        std::thread::spawn(move || {
            let _ = send.send(lantern::library::scan(&folders, &previous));
        });
    }

    fn finish_scan(&mut self) {
        let Some(receive) = &self.scan else { return };
        let index = match receive.try_recv() {
            Ok(index) => index,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.scan = None;
                self.ui.log("The library scan stopped unexpectedly.");
                return;
            }
        };
        self.scan = None;
        // Folders removed while the scan ran drop out.
        let mut index = index;
        let keep = &self.settings.library_folders;
        index.tracks.retain(|t| keep.contains(&t.folder));
        self.ui.log(format!("Music library: {} tracks", index.tracks.len()));
        for (folder, problem) in &index.problems {
            self.ui.log(format!("Library folder {folder}: {problem}"));
        }
        if let Err(err) = lantern::library::save(&index, &lantern::library::index_path()) {
            self.ui.log(format!("Couldn't save the library index: {err}"));
        }
        lantern::library::replace(index);
        // A folder added while that scan ran wasn't in it.
        if lantern::library::unscanned(&self.scan_folders, &self.settings.library_folders) {
            self.rescan();
        }
    }

    fn install_permanently(&mut self, ctx: &egui::Context, dest: &std::path::Path) {
        self.save_settings();
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(err) => {
                self.ui.log(format!("Couldn't find Lantern.exe: {err}"));
                return;
            }
        };
        let installed = match lantern::install::install_into(&app_dir(), &exe, dest) {
            Ok(path) => path,
            Err(err) => {
                self.ui.log(format!("Install failed: {err}"));
                return;
            }
        };
        if let Err(err) = lantern::install::start_menu_shortcut(&installed) {
            self.ui.log(format!("Installed, but the Start menu shortcut failed: {err}"));
        }
        if self.start_with_windows {
            if let Err(err) = lantern::install::set_start_with_windows(true, &installed) {
                self.ui.log(format!("Couldn't point Start with Windows at the new copy: {err}"));
            }
        }
        self.ui.log(format!("Installed to {}. Opening it there.", installed.display()));
        match std::process::Command::new(&installed).current_dir(dest).spawn() {
            // The new copy takes over; this one leaves voice and closes.
            Ok(_) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Err(err) => self.ui.log(format!("Installed, but couldn't open it: {err}")),
        }
    }

    fn save_settings(&mut self) {
        self.settings.clamp();
        match self.settings.save(&settings_path()) {
            Ok(()) => self.ui.log("Settings saved"),
            Err(err) => self.ui.note_save_failure(format!("Couldn't save lantern-settings.json: {err}")),
        }
        self.ui.set_settings(self.settings.clone());
    }

    /// Minimize hides the window. The tray icon brings it back. Quit from the tray closes it.
    fn tray(&mut self, ctx: &egui::Context) {
        let mut just_shown = false;
        while let Ok(event) = self.tray_events.try_recv() {
            match event {
                tray::Event::Show => {
                    tray::reveal();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    self.in_tray = false;
                    just_shown = true;
                    self.suppress_hide_until = Instant::now() + Duration::from_millis(800);
                }
                tray::Event::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            }
        }
        // Don't hide on the same frame we restore, or the old minimized flag puts it straight back.
        #[cfg(windows)]
        if self.settings.minimize_to_tray && !just_shown && !self.in_tray && Instant::now() >= self.suppress_hide_until {
            let minimized = ctx.input(|input| input.viewport().minimized.unwrap_or(false));
            if minimized {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                self.in_tray = true;
                self.ui.log("Hidden in the system tray");
            }
        }
        #[cfg(not(windows))]
        let _ = just_shown;
    }
}

fn style(cc: &eframe::CreationContext<'_>) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(18, 18, 20);
    visuals.window_fill = BLACK;
    visuals.extreme_bg_color = Color32::from_rgb(8, 8, 8);
    visuals.override_text_color = Some(TEXT);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(28, 28, 32);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(28, 28, 32);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(48, 22, 38);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(48, 22, 38);
    visuals.widgets.active.bg_fill = ACCENT;
    visuals.selection.bg_fill = ACCENT;
    cc.egui_ctx.set_visuals(visuals);
}

fn logo_image() -> Option<image::RgbaImage> {
    Some(image::load_from_memory(include_bytes!("../assets/logo.png")).ok()?.into_rgba8())
}

fn app_icon() -> egui::IconData {
    match logo_image() {
        Some(image) => {
            let (width, height) = image.dimensions();
            egui::IconData { rgba: image.into_raw(), width, height }
        }
        None => egui::IconData::default(),
    }
}

fn pick_audio_files() -> Option<Vec<PathBuf>> {
    rfd::FileDialog::new()
        .set_title("Add audio files")
        .add_filter("Audio", &["mp3", "m4a", "ogg", "opus", "flac", "wav", "aac", "webm"])
        .pick_files()
}

fn pick_playlist_file() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Import a playlist")
        .add_filter("Playlists", &lantern::playlist_file::EXTENSIONS)
        .pick_file()
}

fn pick_export_file() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Export the queue")
        .set_file_name("Lantern queue.m3u8")
        .add_filter("M3U8 playlist", &["m3u8"])
        .add_filter("M3U playlist (Winamp)", &["m3u"])
        .add_filter("PLS playlist (Winamp, VLC)", &["pls"])
        .add_filter("XSPF playlist (VLC)", &["xspf"])
        .save_file()
}

fn pick_audio_folder() -> Option<PathBuf> {
    rfd::FileDialog::new().set_title("Add a folder").pick_folder()
}

fn load_logo(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let image = logo_image()?;
    let size = [image.width() as usize, image.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
    Some(ctx.load_texture("lantern-logo", color, egui::TextureOptions::LINEAR))
}
