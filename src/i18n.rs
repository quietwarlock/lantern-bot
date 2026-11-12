//! Translations (1.7). Lantern's Discord replies live in `lang/en.json`,
//! compiled in as the English default. To translate, copy that file to
//! `lantern-lang.json` next to the exe and change the values; any key left
//! out falls back to English. `{0}`, `{1}` are filled in by [`tf`].

use std::collections::HashMap;
use std::sync::RwLock;

pub const ENGLISH: &str = include_str!("../lang/en.json");
pub const OVERRIDE_FILE: &str = "lantern-lang.json";

fn english() -> &'static HashMap<String, String> {
    static EN: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();
    EN.get_or_init(|| serde_json::from_str(ENGLISH).expect("lang/en.json is valid"))
}

static OVERRIDES: RwLock<Option<HashMap<String, String>>> = RwLock::new(None);

/// Loads `lantern-lang.json` if there is one. Returns a note for the log.
pub fn load(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<HashMap<String, String>>(&text) {
        Ok(map) => {
            let count = map.keys().filter(|k| english().contains_key(*k)).count();
            set_overrides(map);
            Some(format!("Loaded {count} translated replies from {OVERRIDE_FILE}"))
        }
        Err(err) => Some(format!("Couldn't read {OVERRIDE_FILE} ({err}); using English.")),
    }
}

pub fn set_overrides(map: HashMap<String, String>) {
    *OVERRIDES.write().unwrap_or_else(|p| p.into_inner()) = Some(map);
}

/// The text for a key: the translation, else English, else the key itself.
pub fn t(key: &str) -> String {
    if let Some(text) = OVERRIDES.read().unwrap_or_else(|p| p.into_inner()).as_ref().and_then(|m| m.get(key)) {
        if !text.trim().is_empty() {
            return text.clone();
        }
    }
    english().get(key).cloned().unwrap_or_else(|| key.to_string())
}

/// [`t`] with `{0}`, `{1}`, … replaced.
pub fn tf(key: &str, args: &[&str]) -> String {
    let mut text = t(key);
    for (i, arg) in args.iter().enumerate() {
        text = text.replace(&format!("{{{i}}}"), arg);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_is_the_default_and_keys_are_used() {
        assert!(english().len() >= 15);
        assert_eq!(t("join_a_voice_channel_first"), "Join a voice channel first.");
        assert_eq!(t("no_such_key"), "no_such_key");
        let source = [include_str!("commands.rs"), include_str!("limits.rs"), include_str!("bot.rs")].concat();
        for key in english().keys() {
            assert!(source.contains(&format!("\"{key}\"")), "unused key {key}");
        }
    }

    #[test]
    fn overrides_win_and_fall_back() {
        let mut map = HashMap::new();
        map.insert("nothing_found".to_string(), "Nada.".to_string());
        map.insert("nothing_is_playing".to_string(), "  ".to_string());
        map.insert("x_{0}".to_string(), "a {0} b {1}".to_string());
        set_overrides(map);
        assert_eq!(t("nothing_found"), "Nada.");
        assert_eq!(t("nothing_is_playing"), "Nothing is playing.");
        assert_eq!(tf("x_{0}", &["1", "2"]), "a 1 b 2");
        set_overrides(HashMap::new());
        assert_eq!(t("nothing_found"), "Nothing found.");
    }
}
