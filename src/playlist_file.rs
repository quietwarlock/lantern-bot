//! Playlist files: `.m3u`, `.m3u8`, `.pls` (Winamp, VLC) and VLC's `.xspf`.
//!
//! [`parse`] reads one into entries: a path on this PC or a link. Relative
//! paths are taken from the playlist's own folder, and `file:///` links become
//! paths. [`write`] does the reverse in the same four formats, so a list can go
//! back into Winamp or VLC.

use crate::media::{Item, Kind};
use std::path::{Path, PathBuf};

/// Longest playlist file read, so a wrong pick can't fill memory.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Most entries taken from one playlist.
pub const MAX_ENTRIES: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    M3u,
    M3u8,
    Pls,
    Xspf,
}

impl Format {
    /// From a file name's extension.
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Self::from_ext(&ext)
    }

    pub fn from_ext(ext: &str) -> Option<Self> {
        match ext.trim_start_matches('.').to_ascii_lowercase().as_str() {
            "m3u" => Some(Self::M3u),
            "m3u8" => Some(Self::M3u8),
            "pls" => Some(Self::Pls),
            "xspf" => Some(Self::Xspf),
            _ => None,
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Self::M3u => "m3u",
            Self::M3u8 => "m3u8",
            Self::Pls => "pls",
            Self::Xspf => "xspf",
        }
    }
}

pub const EXTENSIONS: [&str; 4] = ["m3u", "m3u8", "pls", "xspf"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Path(PathBuf),
    Url(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub target: Target,
    pub title: Option<String>,
    /// Seconds, when the playlist says.
    pub length: Option<u64>,
}

/// Reads playlist text. `base` is the playlist's folder, for relative paths.
pub fn parse(text: &str, format: Format, base: Option<&Path>) -> Vec<Entry> {
    let text = text.trim_start_matches('\u{feff}');
    let mut out = match format {
        Format::M3u | Format::M3u8 => parse_m3u(text, base),
        Format::Pls => parse_pls(text, base),
        Format::Xspf => parse_xspf(text, base),
    };
    out.truncate(MAX_ENTRIES);
    out
}

/// Text with no extension to go by: guesses from the content.
pub fn sniff(text: &str) -> Format {
    let head = text.trim_start_matches('\u{feff}').trim_start();
    if head.starts_with("<?xml") || head.starts_with("<playlist") {
        Format::Xspf
    } else if head.to_ascii_lowercase().starts_with("[playlist]") {
        Format::Pls
    } else {
        Format::M3u8
    }
}

fn target(raw: &str, base: Option<&Path>) -> Option<Target> {
    let raw = raw.trim().trim_matches('"');
    if raw.is_empty() {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    if lower.starts_with("file:") {
        return file_url_path(raw).map(Target::Path);
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Some(Target::Url(raw.to_string()));
    }
    // Other schemes (rtsp:, mms:, spotify:) aren't playable here.
    if let Some((scheme, _)) = raw.split_once("://") {
        if scheme.len() > 1 && scheme.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') {
            return None;
        }
    }
    let path = PathBuf::from(raw.replace('/', std::path::MAIN_SEPARATOR_STR));
    if is_absolute(raw) {
        return Some(Target::Path(path));
    }
    Some(Target::Path(match base {
        Some(base) => base.join(path),
        None => path,
    }))
}

/// `C:\x`, `\\server\share`, `/x`: absolute on Windows or Unix.
fn is_absolute(raw: &str) -> bool {
    let b = raw.as_bytes();
    raw.starts_with('\\') || raw.starts_with('/') || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

/// `file:///C:/Music/a%20b.mp3` → `C:\Music\a b.mp3`; `file://server/share/x` → `\\server\share\x`.
fn file_url_path(url: &str) -> Option<PathBuf> {
    let rest = &url[5..];
    let decoded = urlencoding::decode(rest).map(|c| c.into_owned()).unwrap_or_else(|_| rest.to_string());
    let path = if let Some(after) = decoded.strip_prefix("///") {
        let b = after.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            after.to_string()
        } else {
            format!("/{after}")
        }
    } else if let Some(after) = decoded.strip_prefix("//") {
        if after.to_ascii_lowercase().starts_with("localhost/") {
            let after = &after[10..];
            let b = after.as_bytes();
            if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' { after.to_string() } else { format!("/{after}") }
        } else {
            format!("//{after}")
        }
    } else {
        decoded
    };
    if path.is_empty() {
        return None;
    }
    let path = if cfg!(windows) { path.replace('/', "\\") } else { path };
    Some(PathBuf::from(path))
}

fn parse_m3u(text: &str, base: Option<&Path>) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut pending: (Option<String>, Option<u64>) = (None, None);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(info) = line.strip_prefix("#EXTINF:") {
            let (len, title) = info.split_once(',').unwrap_or((info, ""));
            // `#EXTINF:123 tvg-id="x",Title` carries attributes after the number.
            let len = len.split_whitespace().next().unwrap_or("").parse::<i64>().ok().filter(|n| *n > 0).map(|n| n as u64);
            let title = title.trim();
            pending = ((!title.is_empty()).then(|| title.to_string()), len);
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if let Some(target) = target(line, base) {
            out.push(Entry { target, title: pending.0.take(), length: pending.1.take() });
        }
        pending = (None, None);
    }
    out
}

fn parse_pls(text: &str, base: Option<&Path>) -> Vec<Entry> {
    use std::collections::BTreeMap;
    let mut files: BTreeMap<u32, String> = BTreeMap::new();
    let mut titles: BTreeMap<u32, String> = BTreeMap::new();
    let mut lengths: BTreeMap<u32, u64> = BTreeMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once('=') else { continue };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        let number = |prefix: &str| key.strip_prefix(prefix).and_then(|n| n.parse::<u32>().ok());
        if let Some(n) = number("file") {
            files.insert(n, value.to_string());
        } else if let Some(n) = number("title") {
            titles.insert(n, value.to_string());
        } else if let Some(n) = number("length") {
            if let Ok(len) = value.parse::<i64>() {
                if len > 0 {
                    lengths.insert(n, len as u64);
                }
            }
        }
    }
    files
        .into_iter()
        .filter_map(|(n, file)| {
            Some(Entry {
                target: target(&file, base)?,
                title: titles.get(&n).filter(|t| !t.is_empty()).cloned(),
                length: lengths.get(&n).copied(),
            })
        })
        .collect()
}

fn xml_unescape(text: &str) -> String {
    let text = text.trim();
    let text = text.strip_prefix("<![CDATA[").and_then(|t| t.strip_suffix("]]>")).unwrap_or(text);
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..end];
        let ch = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if name.starts_with("#x") || name.starts_with("#X") => u32::from_str_radix(&name[2..], 16).ok().and_then(char::from_u32),
            _ if name.starts_with('#') => name[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Text inside the first `<tag>…</tag>` (tag names compared without case).
fn inner<'a>(block: &'a str, tag: &str) -> Option<&'a str> {
    let lower = block.to_ascii_lowercase();
    let open = lower.find(&format!("<{tag}"))?;
    let start = open + lower[open..].find('>')? + 1;
    if lower[open..start].ends_with("/>") {
        return None;
    }
    let end = start + lower[start..].find(&format!("</{tag}>"))?;
    Some(&block[start..end])
}

fn parse_xspf(text: &str, base: Option<&Path>) -> Vec<Entry> {
    let mut out = Vec::new();
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(open) = lower[from..].find("<track").map(|i| from + i) {
        // Not <trackList>.
        let after = lower.as_bytes().get(open + 6).copied().unwrap_or(b' ');
        if after.is_ascii_alphanumeric() {
            from = open + 6;
            continue;
        }
        let Some(close) = lower[open..].find("</track>").map(|i| open + i) else { break };
        let block = &text[open..close];
        from = close + 8;
        let Some(location) = inner(block, "location") else { continue };
        let Some(target) = target(&xml_unescape(location), base) else { continue };
        let title = inner(block, "title").map(xml_unescape).filter(|t| !t.is_empty());
        let creator = inner(block, "creator").map(xml_unescape).filter(|t| !t.is_empty());
        let title = match (creator, title) {
            (Some(c), Some(t)) => Some(format!("{c} - {t}")),
            (None, t) => t,
            (Some(_), None) => None,
        };
        let length = inner(block, "duration").and_then(|d| d.trim().parse::<u64>().ok()).filter(|ms| *ms > 0).map(|ms| ms.div_ceil(1000));
        out.push(Entry { target, title, length });
    }
    out
}

/// One line per track for a file the owner exports from the window: local
/// files as their path, everything else as its link. Items with neither
/// (nothing to point at) are left out.
fn location(item: &Item, include_paths: bool) -> Option<String> {
    match item.kind {
        Kind::File if include_paths => Some(item.source.clone()),
        Kind::File => None,
        _ => item.link().map(str::to_string),
    }
}

fn file_uri(path: &str) -> String {
    let forward = path.replace('\\', "/");
    let encode = |s: &str| s.split('/').map(|part| urlencoding::encode(part).into_owned()).collect::<Vec<_>>().join("/");
    if let Some(unc) = forward.strip_prefix("//") {
        format!("file://{}", encode(unc))
    } else if forward.starts_with('/') {
        format!("file://{}", encode(&forward))
    } else {
        // C:/x → file:///C:/x (the colon is kept).
        let (drive, rest) = forward.split_at(forward.find('/').unwrap_or(forward.len()));
        format!("file:///{drive}{}", encode(rest))
    }
}

/// Writes items in `format`. Returns the text and how many items were left out.
/// `include_paths` is false for anything that leaves this PC (Discord).
pub fn write(items: &[Item], format: Format, include_paths: bool) -> (String, usize) {
    let kept: Vec<(&Item, String)> = items.iter().filter_map(|i| location(i, include_paths).map(|l| (i, l))).collect();
    let left_out = items.len() - kept.len();
    let mut out = String::new();
    let one_line = |s: &str| s.replace(['\r', '\n'], " ");
    match format {
        Format::M3u | Format::M3u8 => {
            out.push_str("#EXTM3U\r\n");
            for (item, loc) in &kept {
                let len = item.length.map_or(-1, |l| l as i64);
                out.push_str(&format!("#EXTINF:{len},{}\r\n{loc}\r\n", one_line(&item.title)));
            }
        }
        Format::Pls => {
            out.push_str("[playlist]\r\n");
            for (n, (item, loc)) in kept.iter().enumerate() {
                let n = n + 1;
                let len = item.length.map_or(-1, |l| l as i64);
                out.push_str(&format!("File{n}={loc}\r\nTitle{n}={}\r\nLength{n}={len}\r\n", one_line(&item.title)));
            }
            out.push_str(&format!("NumberOfEntries={}\r\nVersion=2\r\n", kept.len()));
        }
        Format::Xspf => {
            out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<playlist version=\"1\" xmlns=\"http://xspf.org/ns/0/\">\n  <trackList>\n");
            for (item, loc) in &kept {
                let loc = if item.kind == Kind::File { file_uri(loc) } else { loc.clone() };
                out.push_str("    <track>\n");
                out.push_str(&format!("      <location>{}</location>\n", xml_escape(&loc)));
                out.push_str(&format!("      <title>{}</title>\n", xml_escape(&item.title)));
                if let Some(len) = item.length {
                    out.push_str(&format!("      <duration>{}</duration>\n", len * 1000));
                }
                out.push_str("    </track>\n");
            }
            out.push_str("  </trackList>\n</playlist>\n");
        }
    }
    (out, left_out)
}

/// Turns path entries into queue items. Missing files are counted, not queued.
/// Link entries are returned separately; they are looked up like `play <link>`.
pub fn split_entries(entries: Vec<Entry>, path_ok: impl Fn(&Path) -> Option<Item>) -> (Vec<Pending>, usize) {
    let mut out = Vec::new();
    let mut missing = 0;
    for entry in entries {
        match entry.target {
            Target::Path(path) => match path_ok(&path) {
                Some(mut item) => {
                    if let Some(title) = entry.title {
                        if item.title == crate::media::local_title(&path) {
                            item.title = title;
                        }
                    }
                    if item.length.is_none() {
                        item.length = entry.length;
                    }
                    out.push(Pending::Ready(item));
                }
                None => missing += 1,
            },
            Target::Url(url) => out.push(Pending::Link(url, entry.title)),
        }
    }
    (out, missing)
}

/// One playlist line on its way to the queue.
#[derive(Clone, Debug, PartialEq)]
pub enum Pending {
    Ready(Item),
    /// A link still to look up, with the playlist's title for it.
    Link(String, Option<String>),
}

/// A local file from the playlist, when it is still there and is audio.
pub fn local_file(path: &Path) -> Option<Item> {
    if path.is_file() && crate::media::is_audio_path(path) {
        Some(crate::media::local_item(path))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Target {
        Target::Path(PathBuf::from(s.replace('/', std::path::MAIN_SEPARATOR_STR)))
    }

    #[test]
    fn m3u_with_extinf_relative_paths_and_links() {
        let text = "\u{feff}#EXTM3U\r\n#EXTINF:215,Artist - Song\r\nAlbum/01 Song.mp3\r\n\r\n# comment\r\nhttps://youtu.be/abc\r\nC:\\Music\\x.flac\r\nrtsp://nope\r\n";
        let got = parse(text, Format::M3u, Some(Path::new("/lists")));
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].target, Target::Path(Path::new("/lists").join("Album/01 Song.mp3".replace('/', std::path::MAIN_SEPARATOR_STR))));
        assert_eq!(got[0].title.as_deref(), Some("Artist - Song"));
        assert_eq!(got[0].length, Some(215));
        assert_eq!(got[1].target, Target::Url("https://youtu.be/abc".into()));
        assert_eq!(got[1].title, None, "EXTINF belongs only to the next line");
        assert_eq!(got[2].target, p("C:\\Music\\x.flac"));
    }

    #[test]
    fn winamp_and_vlc_pls() {
        let text = "[playlist]\nFile1=http://radio.example/stream\nTitle1=Radio\nLength1=-1\nfile2=D:\\Music\\b.mp3\nTitle2=B\nLength2=200\nNumberOfEntries=2\nVersion=2\n";
        let got = parse(text, Format::Pls, None);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].target, Target::Url("http://radio.example/stream".into()));
        assert_eq!(got[0].length, None);
        assert_eq!(got[1].title.as_deref(), Some("B"));
        assert_eq!(got[1].length, Some(200));
        assert_eq!(sniff(text), Format::Pls);
    }

    #[test]
    fn vlc_xspf_with_file_uris_and_entities() {
        let text = r#"<?xml version="1.0" encoding="UTF-8"?>
<playlist xmlns="http://xspf.org/ns/0/" version="1"><title>Playlist</title><trackList>
<track><location>file:///D:/Music/Rock%20%26%20Roll/a.mp3</location><title>A &amp; B</title><creator>Band</creator><duration>200500</duration><extension application="http://www.videolan.org/vlc/playlist/0"><vlc:id>0</vlc:id></extension></track>
<track><location>https://www.youtube.com/watch?v=x&amp;t=1</location></track>
<track><title>no location</title></track>
</trackList></playlist>"#;
        let got = parse(text, Format::Xspf, None);
        assert_eq!(got.len(), 2);
        let want = if cfg!(windows) { "D:\\Music\\Rock & Roll\\a.mp3" } else { "D:/Music/Rock & Roll/a.mp3" };
        assert_eq!(got[0].target, Target::Path(PathBuf::from(want)));
        assert_eq!(got[0].title.as_deref(), Some("Band - A & B"));
        assert_eq!(got[0].length, Some(201));
        assert_eq!(got[1].target, Target::Url("https://www.youtube.com/watch?v=x&t=1".into()));
        assert_eq!(sniff(text), Format::Xspf);
    }

    #[test]
    fn unc_file_uri() {
        let got = file_url_path("file://nas/music/a%20b.flac").unwrap();
        let want = if cfg!(windows) { "\\\\nas\\music\\a b.flac" } else { "//nas/music/a b.flac" };
        assert_eq!(got, PathBuf::from(want));
    }

    fn item(title: &str, source: &str, kind: Kind, length: Option<u64>) -> Item {
        Item { title: title.into(), source: source.into(), query: None, kind, length, allow_private_host: false, requested_by: None }
    }

    #[test]
    fn export_round_trips_in_every_format() {
        let items = vec![
            item("Song & Co", "https://www.youtube.com/watch?v=abc", Kind::YouTube, Some(100)),
            item("Local", "/music/My Album/01.mp3", Kind::File, Some(61)),
        ];
        for format in [Format::M3u, Format::M3u8, Format::Pls, Format::Xspf] {
            let (text, left) = write(&items, format, true);
            assert_eq!(left, 0);
            let back = parse(&text, format, None);
            assert_eq!(back.len(), 2, "{format:?}: {text}");
            assert_eq!(back[0].target, Target::Url("https://www.youtube.com/watch?v=abc".into()), "{format:?}");
            assert_eq!(back[0].title.as_deref(), Some("Song & Co"), "{format:?}");
            assert_eq!(back[0].length, Some(100), "{format:?}");
            if !cfg!(windows) {
                assert_eq!(back[1].target, Target::Path(PathBuf::from("/music/My Album/01.mp3")), "{format:?}");
            }
            assert_eq!(sniff(&text) == Format::Xspf, format == Format::Xspf);
        }
    }

    #[test]
    fn export_for_discord_leaves_paths_out() {
        let items = vec![item("Local", "D:\\Secret\\a.mp3", Kind::File, None), item("Web", "https://x.example/a.mp3", Kind::Direct, None)];
        let (text, left) = write(&items, Format::M3u8, false);
        assert_eq!(left, 1);
        assert!(!text.contains("Secret"));
        assert!(text.contains("https://x.example/a.mp3"));
    }

    #[test]
    fn missing_files_are_counted() {
        let entries = vec![
            Entry { target: Target::Path("/nope/a.mp3".into()), title: None, length: None },
            Entry { target: Target::Url("https://a.example/x".into()), title: Some("X".into()), length: None },
            Entry { target: Target::Path("/nope/b.mp3".into()), title: None, length: None },
        ];
        let (pending, missing) = split_entries(entries, local_file);
        assert_eq!(missing, 2);
        assert_eq!(pending, vec![Pending::Link("https://a.example/x".into(), Some("X".into()))]);
    }

    #[test]
    fn present_files_get_the_playlist_title_and_length() {
        let dir = std::env::temp_dir().join(format!("lantern-pl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("track01.mp3");
        std::fs::write(&file, b"not really audio").unwrap();
        let text = format!("#EXTINF:42,Nice Title\n{}\n", file.display());
        let (pending, missing) = split_entries(parse(&text, Format::M3u, None), local_file);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(missing, 0);
        match &pending[0] {
            Pending::Ready(item) => {
                assert_eq!(item.kind, Kind::File);
                assert_eq!(item.title, "Nice Title");
                assert_eq!(item.length, Some(42));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn extensions_are_known() {
        for ext in EXTENSIONS {
            assert_eq!(Format::from_ext(ext).unwrap().ext(), ext);
        }
        assert_eq!(Format::from_path(Path::new("a.M3U8")), Some(Format::M3u8));
        assert_eq!(Format::from_path(Path::new("a.txt")), None);
    }
}
