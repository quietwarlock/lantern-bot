//! "Copy diagnostics" (1.7): a report for public bug reports with the token,
//! Discord IDs, file paths and network addresses removed.

use crate::settings::Settings;
use regex::Regex;
use std::sync::OnceLock;

/// Log lines included in the report.
pub const LOG_LINES: usize = 200;

fn patterns() -> &'static [(Regex, &'static str)] {
    static P: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    P.get_or_init(|| {
        [
            // Bot tokens: three base64 parts.
            (r"[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{20,}", "<token>"),
            // Windows UNC and drive paths, then Unix home paths.
            (r#"\\\\[^\s"'<>|]+"#, "<path>"),
            (r#"(?i)\b[a-z]:[\\/][^\s"'<>|]*"#, "<path>"),
            (r#"(?:/home|/Users|/root)/[^\s"'<>|]*"#, "<path>"),
            (r"(?i)file://[^\s]*", "<path>"),
            // IPv4 addresses.
            (r"\b(?:\d{1,3}\.){3}\d{1,3}\b", "<ip>"),
            // Discord snowflakes (server, channel, user, role IDs).
            (r"\b\d{17,20}\b", "<id>"),
        ]
        .into_iter()
        .map(|(p, r)| (Regex::new(p).expect("valid pattern"), r))
        .collect()
    })
}

/// Removes anything that could identify the user. `secrets` are exact
/// strings (the token, the owner ID) removed first, whatever their shape.
pub fn redact(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_string();
    for secret in secrets.iter().map(|s| s.trim()).filter(|s| s.len() >= 4) {
        out = out.replace(secret, "<redacted>");
    }
    for (re, with) in patterns() {
        out = re.replace_all(&out, *with).into_owned();
    }
    out
}

/// The settings with private fields blanked, as pretty JSON.
pub fn safe_settings(settings: &Settings) -> String {
    let mut copy = settings.clone();
    copy.token = if copy.token.trim().is_empty() { String::new() } else { "<set>".into() };
    copy.owner = if copy.owner.trim().is_empty() { String::new() } else { "<set>".into() };
    copy.library_folders = copy.library_folders.iter().map(|_| "<folder>".to_string()).collect();
    serde_json::to_string_pretty(&copy).unwrap_or_default()
}

/// The whole report.
pub fn report(settings: &Settings, logs: &[String], connection: &str) -> String {
    let start = logs.len().saturating_sub(LOG_LINES);
    let log = logs[start..].join("\n");
    let text = format!(
        "Lantern {} diagnostics\nOS: {} {}\nStatus: {}\nLibrary folders: {}\n\nSettings:\n{}\n\nLast {} log lines:\n{}\n",
        crate::VERSION,
        std::env::consts::OS,
        std::env::consts::ARCH,
        connection,
        settings.library_folders.len(),
        safe_settings(settings),
        logs.len() - start,
        log
    );
    redact(&text, &[settings.token.as_str(), settings.owner.as_str()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_things_are_removed() {
        let token = format!("{}.{}.{}", "A".repeat(24), "B".repeat(6), "C".repeat(27));
        let text = format!(
            "token {token} guild 123456789012345678 file D:\\Music\\me\\a.mp3 share \\\\nas\\music\\x.flac home /home/someone/a ip 10.0.0.7 owner 4242424242"
        );
        let out = redact(&text, &["4242424242"]);
        for bad in [token.as_str(), "123456789012345678", "D:\\Music", "nas", "someone", "10.0.0.7", "4242424242"] {
            assert!(!out.contains(bad), "{bad} left in {out}");
        }
        assert!(out.contains("<token>") && out.contains("<id>") && out.contains("<path>") && out.contains("<ip>"));
    }

    #[test]
    fn report_hides_settings_fields() {
        let settings = Settings {
            token: format!("{}.{}.{}", "x".repeat(24), "y".repeat(6), "z".repeat(27)),
            owner: "123456789012345678".into(),
            library_folders: vec![r"\\nas\music".into(), r"C:\Users\someone\Music".into()],
            ..Default::default()
        };
        let logs: Vec<String> = (0..300).map(|i| format!("line {i} in channel 987654321098765432")).collect();
        let out = report(&settings, &logs, "Online");
        assert!(!out.contains(&settings.token) && !out.contains("123456789012345678") && !out.contains("987654321098765432"));
        assert!(!out.contains("someone") && !out.contains("nas"));
        assert!(out.contains("line 299") && !out.contains("line 99 "));
        assert!(out.contains("Library folders: 2"));
    }
}
