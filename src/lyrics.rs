//! Lyrics from lrclib.net, a free lyrics database with an open API (no key).

use serde::Deserialize;
use std::time::Duration;

const API: &str = "https://lrclib.net/api/search";
/// Discord embeds hold 4096 characters; leave room for the footer.
pub const MAX_CHARS: usize = 3900;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    #[serde(default)]
    pub track_name: String,
    #[serde(default)]
    pub artist_name: String,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub instrumental: bool,
    #[serde(default)]
    pub plain_lyrics: Option<String>,
    /// LRC text: `[mm:ss.xx] line` (1.7 synced lyrics).
    #[serde(default)]
    pub synced_lyrics: Option<String>,
}

/// One timed line.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub at_ms: u64,
    pub text: String,
}

/// Reads LRC: each line may carry one or more `[mm:ss.xx]` stamps. Lines
/// without a stamp (tags like `[ar:...]`) are skipped. Sorted by time.
pub fn parse_synced(lrc: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    for raw in lrc.lines() {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while let Some(inner) = rest.strip_prefix('[') {
            let Some(end) = inner.find(']') else { break };
            match stamp_ms(&inner[..end]) {
                Some(ms) => stamps.push(ms),
                None => break,
            }
            rest = inner[end + 1..].trim_start();
        }
        for ms in stamps {
            lines.push(Line { at_ms: ms, text: rest.trim().to_string() });
        }
    }
    lines.sort_by_key(|l| l.at_ms);
    lines
}

fn stamp_ms(text: &str) -> Option<u64> {
    let (min, sec) = text.split_once(':')?;
    let min: u64 = min.trim().parse().ok()?;
    let (whole, frac) = sec.split_once(['.', ':']).unwrap_or((sec, "0"));
    let whole: u64 = whole.trim().parse().ok()?;
    if whole >= 60 || frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let frac_ms = match frac.len() {
        1 => frac.parse::<u64>().ok()? * 100,
        2 => frac.parse::<u64>().ok()? * 10,
        _ => frac[..3].parse::<u64>().ok()?,
    };
    Some(min * 60_000 + whole * 1000 + frac_ms)
}

/// Index of the line being sung at `ms`, if any has started.
pub fn current_line(lines: &[Line], ms: u64) -> Option<usize> {
    lines.iter().rposition(|l| l.at_ms <= ms)
}

/// A few lines around the current one, the current one in bold.
pub fn window(lines: &[Line], ms: u64, before: usize, after: usize) -> String {
    let now = current_line(lines, ms);
    let centre = now.unwrap_or(0);
    let start = centre.saturating_sub(before);
    let end = (centre + after + 1).min(lines.len());
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate().take(end).skip(start) {
        let text = if line.text.is_empty() { "♪".to_string() } else { line.text.replace('*', "\\*") };
        out.push(if Some(i) == now { format!("**▶ {text}**") } else { text });
    }
    out.join("\n")
}

/// Strips what YouTube titles add around a song name: `(Official Video)`,
/// `[Lyrics]`, `ft. someone`, `| Topic`.
pub fn clean_title(title: &str) -> String {
    let mut out = String::new();
    let mut depth = 0i32;
    for ch in title.chars() {
        match ch {
            '(' | '[' | '【' => depth += 1,
            ')' | ']' | '】' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    let lower = out.to_lowercase();
    let mut cut = out.len();
    for marker in [" ft.", " feat.", " ft ", " feat ", " | ", " official"] {
        if let Some(at) = lower.find(marker) {
            cut = cut.min(at);
        }
    }
    out[..cut].split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Picks the result with lyrics whose length is closest to the song's.
pub fn pick(results: &[Found], length: Option<u64>) -> Option<&Found> {
    let usable = results.iter().filter(|r| r.instrumental || r.plain_lyrics.as_deref().is_some_and(|l| !l.trim().is_empty()));
    match length {
        Some(len) => usable.min_by_key(|r| r.duration.map(|d| (d - len as f64).abs() as u64).unwrap_or(u64::MAX)),
        None => usable.into_iter().next(),
    }
}

/// Lyrics trimmed to fit one embed.
pub fn fit(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_CHARS {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(MAX_CHARS).collect();
    if let Some(at) = cut.rfind('\n') {
        cut.truncate(at);
    }
    cut.push_str("\n…");
    cut
}

pub async fn search(query: &str, length: Option<u64>) -> Result<Found, String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("Lantern/", env!("CARGO_PKG_VERSION"), " (self-hosted Discord music bot)"))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(API)
        .query(&[("q", query)])
        .send()
        .await
        .map_err(|e| format!("lrclib.net didn't answer ({e})"))?;
    let response = response.error_for_status().map_err(|e| format!("lrclib.net said no ({e})"))?;
    let results: Vec<Found> = response.json().await.map_err(|e| format!("lrclib.net sent something odd ({e})"))?;
    pick(&results, length).cloned().ok_or_else(|| "No lyrics found.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_noise_is_removed() {
        assert_eq!(clean_title("Rick Astley - Never Gonna Give You Up (Official Music Video)"), "Rick Astley - Never Gonna Give You Up");
        assert_eq!(clean_title("Song [Lyrics] ft. Someone"), "Song");
        assert_eq!(clean_title("Artist - Song | Official Audio"), "Artist - Song");
    }

    fn found(name: &str, secs: f64, words: Option<&str>) -> Found {
        Found { track_name: name.into(), artist_name: "A".into(), duration: Some(secs), instrumental: false, plain_lyrics: words.map(String::from), synced_lyrics: None }
    }

    #[test]
    fn closest_length_with_words_wins() {
        let results = vec![found("short", 100.0, Some("la")), found("none", 212.0, None), found("right", 210.0, Some("la la"))];
        assert_eq!(pick(&results, Some(212)).unwrap().track_name, "right");
        assert_eq!(pick(&results, None).unwrap().track_name, "short");
        assert!(pick(&[found("x", 1.0, Some("  "))], None).is_none());
    }

    #[test]
    fn api_json_parses() {
        let json = r#"[{"id":1,"trackName":"Song","artistName":"Band","albumName":"LP","duration":200.0,"instrumental":false,"plainLyrics":"line one\nline two","syncedLyrics":null}]"#;
        let results: Vec<Found> = serde_json::from_str(json).unwrap();
        assert_eq!(results[0].plain_lyrics.as_deref(), Some("line one\nline two"));
    }

    #[test]
    fn synced_lyrics_parse_and_follow_the_song() {
        let lrc = "[ar:Band]\n[00:01.50] one\n[00:05.00][01:00.00] chorus\n[00:03.2] two\nno stamp\n[00:07.123]";
        let lines = parse_synced(lrc);
        assert_eq!(lines.iter().map(|l| l.at_ms).collect::<Vec<_>>(), [1500, 3200, 5000, 7123, 60000]);
        assert_eq!(current_line(&lines, 0), None);
        assert_eq!(current_line(&lines, 3300), Some(1));
        assert_eq!(current_line(&lines, 99_999), Some(4));
        let text = window(&lines, 5100, 1, 1);
        assert_eq!(text, "two\n**▶ chorus**\n♪");
        assert!(window(&lines, 0, 1, 1).starts_with("one"));
        assert!(parse_synced("[99:xx] bad").is_empty());
        let json = r#"[{"trackName":"S","artistName":"B","syncedLyrics":"[00:01.00] hi"}]"#;
        let found: Vec<Found> = serde_json::from_str(json).unwrap();
        assert_eq!(parse_synced(found[0].synced_lyrics.as_deref().unwrap())[0].text, "hi");
    }

    #[test]
    fn long_lyrics_are_cut_at_a_line() {
        let text = "word words\n".repeat(1000);
        let fitted = fit(&text);
        assert!(fitted.chars().count() <= MAX_CHARS + 2);
        assert!(fitted.ends_with("\n…"));
    }

    #[tokio::test]
    #[ignore = "network"]
    async fn real_lrclib_finds_a_song() {
        let found = search("Rick Astley Never Gonna Give You Up", Some(213)).await.unwrap();
        assert!(found.plain_lyrics.unwrap().to_lowercase().contains("never gonna"));
    }
}
