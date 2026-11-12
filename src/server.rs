//! Lantern without a window, for Linux and Docker (1.8.3).
//!
//! Reads `lantern-settings.json` from `LANTERN_DIR` (Docker: `/config`),
//! writes `lantern-log.txt` there and to standard output, and starts the bot
//! at once. `DISCORD_TOKEN` replaces the token from the settings file (kept
//! in memory only, never written). `LANTERN_LIBRARY` replaces the music
//! library folders (comma separated, e.g. `/music`).
//!
//! 1.9: a web page on `LANTERN_WEB_PORT` (default 8080; `off` turns it off)
//! does what the window does: Start/Stop, the player, the queue, every
//! setting, and the log. With the page on, a missing or rejected token no
//! longer ends the program; paste a new one there and press Start.

use lantern::bot;
use lantern::settings::{app_dir, settings_path, Settings};
use lantern::ui_state::UiState;
use lantern::web::{self, BotControl, Library, Web};
use std::sync::Arc;
use std::time::{Duration, Instant};

const HELP: &str = "Lantern server (no window)

Usage: lantern-server [--check | --version | --help]

  --check    Read the config, report what was found, and exit (no Discord connection).
             Exit code 0 when a bot token is set and looks valid, otherwise 1.

Environment:
  LANTERN_DIR      Folder for lantern-settings.json, lantern-data.json and lantern-log.txt (default: beside the program).
  DISCORD_TOKEN    Bot token. Overrides the token in lantern-settings.json. Never written to disk.
  LANTERN_LIBRARY  Music library folders, comma separated (e.g. /music). Overrides the settings file.
  LANTERN_WEB_PORT Port for the web page (default 8080). `off` turns the page off. No sign-in: keep it on your home network.
";

/// How long to wait before signing in again after Discord drops the session.
const RESTART_AFTER: Duration = Duration::from_secs(30);

/// Where the token came from, for the log. Never the token itself.
fn token_source(from_env: bool, token: &str) -> &'static str {
    match (from_env, token.trim().is_empty()) {
        (_, true) => "missing",
        (true, false) => "from DISCORD_TOKEN",
        (false, false) => "from lantern-settings.json",
    }
}

/// `LANTERN_LIBRARY` split into folders.
fn library_from_env(text: &str) -> Vec<String> {
    text.split([',', ';']).map(str::trim).filter(|f| !f.is_empty()).map(String::from).collect()
}

/// The bot can't start because of the token; trying again won't help.
fn token_problem(detail: &str) -> bool {
    let detail = detail.to_lowercase();
    detail.contains("token") || detail.contains("intent") || detail.contains("401") || detail.contains("unauthorized")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("Lantern {}", lantern::VERSION);
        return;
    }
    let check = args.iter().any(|a| a == "--check");
    let dir = app_dir();
    if let Err(err) = std::fs::create_dir_all(&dir) {
        eprintln!("Can't use the config folder {}: {err}", dir.display());
        std::process::exit(1);
    }
    let crash = dir.join("lantern-crash.txt");
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("Lantern crashed: {info}");
        let _ = std::fs::write(&crash, format!("{info}"));
    }));

    let found = settings_path().is_file();
    let (mut settings, warning) = Settings::load(&settings_path());
    let env_token = std::env::var("DISCORD_TOKEN").ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    let from_env = env_token.is_some();
    if let Some(token) = env_token {
        settings.token = token;
    }
    let library_env = std::env::var("LANTERN_LIBRARY").ok();
    if let Some(folders) = &library_env {
        settings.library_folders = library_from_env(folders);
    }
    let web_port = web::port_from_env(std::env::var(web::PORT_ENV).ok().as_deref());

    let ui = UiState::new(settings.clone()).with_stdout();
    let ui = if check { ui } else { ui.with_log_file(dir.join("lantern-log.txt")) };
    // The window's tray hint means nothing here (shown on the web page).
    ui.set_live("Offline", "Nothing playing", "Lantern is stopped.");
    ui.log(format!("Lantern {} server (no window). Config folder: {}", lantern::VERSION, dir.display()));
    if let Some(warning) = warning {
        ui.log(warning);
    }
    if let Some(note) = lantern::i18n::load(&dir.join(lantern::i18n::OVERRIDE_FILE)) {
        ui.log(note);
    }
    let path_var = std::env::var_os("PATH");
    let ytdlp = lantern::ytdlp::find(&dir, path_var.as_deref());
    let deno = lantern::ytdlp::find_deno(&dir, path_var.as_deref());
    ui.log(format!(
        "Settings: {} · token {} · prefix {} · library folders: {} · yt-dlp: {} · deno: {}",
        if found { "lantern-settings.json loaded" } else { "no lantern-settings.json, using defaults" },
        token_source(from_env, &settings.token),
        settings.prefix,
        if settings.library_folders.is_empty() { "none".to_string() } else { settings.library_folders.join(", ") },
        ytdlp.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "not found".into()),
        deno.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "not found".into()),
    ));

    let token = settings.token.trim().to_string();
    let token_ok = !token.is_empty() && serenity::utils::token::validate(&token).is_ok();
    if check {
        if let Err(problem) = &web_port {
            ui.log(problem.clone());
        }
        ui.log(if token_ok { "Check: ready to start." } else { "Check: no usable bot token (set DISCORD_TOKEN or the token in lantern-settings.json)." });
        std::process::exit(if token_ok { 0 } else { 1 });
    }
    let web_port = match web_port {
        Ok(port) => port,
        Err(problem) => {
            ui.log(format!("{problem} Using {}.", web::DEFAULT_PORT));
            Some(web::DEFAULT_PORT)
        }
    };
    if token.is_empty() && web_port.is_none() {
        ui.log("No bot token. Set DISCORD_TOKEN, or put the token in lantern-settings.json in the config folder.");
        std::process::exit(2);
    }

    lantern::library::replace(lantern::library::load_saved(&lantern::library::index_path()));
    let library = Library::new(ui.clone(), lantern::library::index_path());
    if !settings.library_folders.is_empty() {
        library.rescan();
    }

    let control = Arc::new(BotControl::new(bot::spawn(ui.clone())));
    // Without the page there is no Start button, so the bot always starts.
    if token.is_empty() {
        ui.log("No bot token yet. Open the web page, paste it in Settings, and press Start.");
    } else if settings.start_on_open || web_port.is_none() {
        control.start();
    } else {
        ui.log("\"Start the bot when Lantern starts\" is off. Press Start on the web page.");
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            ui.log(format!("Could not start: {err}"));
            std::process::exit(1);
        }
    };
    if let Some(port) = web_port {
        let page = Web::new(ui.clone(), control.clone(), library.clone(), settings_path(), from_env, library_env.clone());
        let log = ui.clone();
        runtime.spawn(async move {
            log.log(format!("Web page on port {port} (inside the container). No sign-in."));
            if let Err(err) = web::serve(page, port).await {
                log.log(err);
            }
        });
    }
    let code = runtime.block_on(supervise(&ui, &control, web_port.is_some()));
    control.handle().stop_and_wait(Duration::from_secs(5));
    ui.log("Lantern server stopped.");
    std::process::exit(code);
}

/// Waits for Ctrl+C or SIGTERM (`docker stop`). Signs in again after Discord
/// drops the session, unless Stop was pressed on the web page. When the token
/// or intents are wrong it waits for a new Start (web page on) or exits with 2.
async fn supervise(ui: &UiState, control: &BotControl, has_page: bool) -> i32 {
    let mut started = Instant::now();
    let mut starts = control.starts();
    let mut seen_up = false;
    let mut retry_at: Option<Instant> = None;
    let stop = shutdown_signal();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => {
                ui.log("Shutting down…");
                return 0;
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
        if control.starts() != starts {
            // Start was pressed: a fresh sign-in gets its own grace period.
            starts = control.starts();
            started = Instant::now();
            seen_up = false;
            retry_at = None;
        }
        let live = ui.live();
        if live.connection != "Offline" {
            seen_up = true;
            retry_at = None;
            continue;
        }
        if !control.wanted() {
            retry_at = None;
            continue;
        }
        if !seen_up && started.elapsed() < Duration::from_secs(10) {
            continue;
        }
        if token_problem(&live.detail) {
            if !has_page {
                ui.log("Stopping: Discord won't accept this bot token or its intents. Fix it and start the container again.");
                return 2;
            }
            ui.log("Discord won't accept this bot token or its intents. Fix it on the web page, then press Start.");
            control.give_up();
            continue;
        }
        match retry_at {
            None => {
                ui.log(format!("Signing in again in {} s", RESTART_AFTER.as_secs()));
                retry_at = Some(Instant::now() + RESTART_AFTER);
            }
            Some(at) if Instant::now() >= at => {
                retry_at = None;
                seen_up = false;
                started = Instant::now();
                control.handle().start();
            }
            Some(_) => {}
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_is_described_never_shown() {
        assert_eq!(token_source(true, "abc.def.ghi"), "from DISCORD_TOKEN");
        assert_eq!(token_source(false, "abc.def.ghi"), "from lantern-settings.json");
        assert_eq!(token_source(true, "  "), "missing");
    }

    #[test]
    fn library_folders_from_the_environment() {
        assert_eq!(library_from_env("/music"), ["/music"]);
        assert_eq!(library_from_env(" /music , /more;/third ,"), ["/music", "/more", "/third"]);
        assert!(library_from_env("").is_empty());
    }

    #[test]
    fn token_and_intent_errors_stop_instead_of_looping() {
        assert!(token_problem("Discord rejected the bot token. Developer Portal…"));
        assert!(token_problem("That doesn't look like a bot token."));
        assert!(token_problem("Discord refused the Message Content intent."));
        assert!(token_problem("Discord disconnected: 401: Unauthorized. Press Start to try again."));
        assert!(!token_problem("Discord disconnected: connection reset. Press Start to try again."));
    }
}
