//! Internet radio search through the free Radio Browser directory (1.10).
//!
//! <https://www.radio-browser.info>: no key. Per their docs a server is
//! picked from the `all.api.radio-browser.info` list, requests carry a
//! `Lantern/<version>` User-Agent, searches use `/json/stations/search`, and
//! `/json/url/<uuid>` is called when a station is played so its click count
//! goes up. Stations play as [`Kind::Radio`] live streams.

use crate::media::{Item, Kind};
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Used when the server list can't be fetched.
pub const FALLBACK_SERVERS: [&str; 3] = ["de1.api.radio-browser.info", "nl1.api.radio-browser.info", "at1.api.radio-browser.info"];
const SERVER_LIST: &str = "https://all.api.radio-browser.info/json/servers";
/// Most results one search returns.
pub const MAX_RESULTS: usize = 50;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Station {
    pub stationuuid: String,
    pub name: String,
    pub url: String,
    pub url_resolved: String,
    pub homepage: String,
    pub favicon: String,
    pub tags: String,
    pub country: String,
    pub countrycode: String,
    pub codec: String,
    pub bitrate: u32,
    pub lastcheckok: u8,
}

impl Station {
    /// The address to play: the resolved one when the directory has it.
    pub fn stream_url(&self) -> Option<&str> {
        [self.url_resolved.trim(), self.url.trim()]
            .into_iter()
            .find(|u| u.starts_with("http://") || u.starts_with("https://"))
    }

    pub fn title(&self) -> String {
        let name = self.name.trim();
        if name.is_empty() { "Radio".into() } else { name.to_string() }
    }

    /// Up to five tags, as the directory lists them.
    pub fn tag_list(&self) -> Vec<String> {
        self.tags.split(',').map(str::trim).filter(|t| !t.is_empty()).take(5).map(String::from).collect()
    }

    /// `Germany · jazz, swing · MP3 128 kbps`, without empty parts.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.country.trim().is_empty() {
            parts.push(self.country.trim().to_string());
        }
        let tags = self.tag_list();
        if !tags.is_empty() {
            parts.push(tags.join(", "));
        }
        let format = match (self.codec.trim(), self.bitrate) {
            ("", 0) => String::new(),
            (codec, 0) => codec.to_string(),
            ("", rate) => format!("{rate} kbps"),
            (codec, rate) => format!("{codec} {rate} kbps"),
        };
        if !format.is_empty() {
            parts.push(format);
        }
        parts.join(" · ")
    }

    /// The queue entry: a live stream with no length.
    pub fn item(&self) -> Option<Item> {
        let url = self.stream_url()?;
        let mut item = Item::new(self.title(), url, Kind::Radio);
        item.length = None;
        Some(item)
    }
}

/// What to search for. Words go to the station name unless they follow
/// `tag:` (or `genre:`) or `country:`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub name: String,
    pub tag: String,
    pub country: String,
}

impl Query {
    pub fn parse(text: &str) -> Self {
        let mut query = Query::default();
        let mut field = 0; // 0 name, 1 tag, 2 country
        for word in text.split_whitespace() {
            let lower = word.to_ascii_lowercase();
            let (switch, rest) = if let Some(rest) = lower.strip_prefix("tag:").or_else(|| lower.strip_prefix("genre:")) {
                (Some(1), &word[word.len() - rest.len()..])
            } else if let Some(rest) = lower.strip_prefix("country:") {
                (Some(2), &word[word.len() - rest.len()..])
            } else if let Some(rest) = lower.strip_prefix("name:") {
                (Some(0), &word[word.len() - rest.len()..])
            } else {
                (None, word)
            };
            if let Some(next) = switch {
                field = next;
            }
            if rest.is_empty() {
                continue;
            }
            let slot = match field {
                1 => &mut query.tag,
                2 => &mut query.country,
                _ => &mut query.name,
            };
            if !slot.is_empty() {
                slot.push(' ');
            }
            slot.push_str(rest);
        }
        query
    }

    pub fn is_empty(&self) -> bool {
        self.name.trim().is_empty() && self.tag.trim().is_empty() && self.country.trim().is_empty()
    }

    /// The `/json/stations/search` query string. A two-letter country is a country code.
    pub fn search_path(&self, limit: usize) -> String {
        let mut params: Vec<(String, String)> = Vec::new();
        if !self.name.trim().is_empty() {
            params.push(("name".into(), self.name.trim().into()));
        }
        if !self.tag.trim().is_empty() {
            params.push(("tag".into(), self.tag.trim().to_lowercase()));
        }
        let country = self.country.trim();
        if country.len() == 2 && country.chars().all(|c| c.is_ascii_alphabetic()) {
            params.push(("countrycode".into(), country.to_ascii_uppercase()));
        } else if !country.is_empty() {
            params.push(("country".into(), country.into()));
        }
        params.push(("limit".into(), limit.clamp(1, MAX_RESULTS).to_string()));
        params.push(("hidebroken".into(), "true".into()));
        params.push(("order".into(), "clickcount".into()));
        params.push(("reverse".into(), "true".into()));
        let text: Vec<String> = params.iter().map(|(k, v)| format!("{k}={}", urlencoding::encode(v))).collect();
        format!("/json/stations/search?{}", text.join("&"))
    }
}

/// Stations from a search answer: only ones with a playable http(s) address, each once.
pub fn parse_stations(json: &str) -> Result<Vec<Station>, String> {
    let all: Vec<serde_json::Value> = serde_json::from_str(json).map_err(|e| format!("Radio Browser sent something unexpected ({e})"))?;
    let mut seen = std::collections::HashSet::new();
    Ok(all
        .into_iter()
        .filter_map(|mut entry| {
            // A null field counts as missing; an entry that still doesn't fit is skipped.
            entry.as_object_mut()?.retain(|_, v| !v.is_null());
            serde_json::from_value::<Station>(entry).ok()
        })
        .filter(|s| s.stream_url().is_some())
        .filter(|s| s.stationuuid.is_empty() || seen.insert(s.stationuuid.clone()))
        .collect())
}

/// A station ID is a UUID: hex digits and dashes.
pub fn valid_uuid(uuid: &str) -> bool {
    uuid.len() == 36 && uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("Lantern/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(6))
            .timeout(Duration::from_secs(12))
            .build()
            .unwrap_or_default()
    })
}

/// The directory servers, from their list (shuffled once), else the fallbacks.
/// The one that answered last goes first.
fn servers() -> &'static Mutex<Vec<String>> {
    static SERVERS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    SERVERS.get_or_init(|| Mutex::new(Vec::new()))
}

#[derive(Deserialize)]
struct ServerEntry {
    #[serde(default)]
    name: String,
}

async fn server_bases() -> Vec<String> {
    {
        let known = servers().lock().unwrap_or_else(|p| p.into_inner());
        if !known.is_empty() {
            return known.clone();
        }
    }
    let mut names: Vec<String> = match client().get(SERVER_LIST).send().await {
        Ok(response) => response.json::<Vec<ServerEntry>>().await.unwrap_or_default().into_iter().map(|s| s.name).collect(),
        Err(_) => Vec::new(),
    };
    names.retain(|n| n.ends_with(".api.radio-browser.info") && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'));
    names.sort();
    names.dedup();
    fastrand::shuffle(&mut names);
    if names.is_empty() {
        names = FALLBACK_SERVERS.iter().map(|s| s.to_string()).collect();
    }
    let bases: Vec<String> = names.into_iter().map(|n| format!("https://{n}")).collect();
    *servers().lock().unwrap_or_else(|p| p.into_inner()) = bases.clone();
    bases
}

fn prefer(base: &str) {
    let mut known = servers().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(at) = known.iter().position(|b| b == base) {
        let first = known.remove(at);
        known.insert(0, first);
    }
}

/// GETs `path` from `base`.
async fn get_from(base: &str, path: &str) -> Result<String, String> {
    let response = client().get(format!("{base}{path}")).send().await.map_err(|e| e.to_string())?;
    let response = response.error_for_status().map_err(|e| e.to_string())?;
    response.text().await.map_err(|e| e.to_string())
}

/// GETs `path` from the first directory server that answers (up to three).
async fn get(path: &str) -> Result<String, String> {
    let mut last = String::from("no Radio Browser server answered");
    for base in server_bases().await.into_iter().take(3) {
        match get_from(&base, path).await {
            Ok(text) => {
                prefer(&base);
                return Ok(text);
            }
            Err(err) => last = err,
        }
    }
    Err(format!("Radio Browser didn't answer ({last})"))
}

pub async fn search(query: &Query, limit: usize) -> Result<Vec<Station>, String> {
    if query.is_empty() {
        return Err("Say what to look for: a station name, tag:genre, or country:name.".into());
    }
    parse_stations(&get(&query.search_path(limit)).await?)
}

/// Typed words (Discord): by name, and when nothing matches, the same words as a tag.
pub async fn search_words(text: &str, limit: usize) -> Result<Vec<Station>, String> {
    let query = Query::parse(text);
    let found = search(&query, limit).await?;
    if found.is_empty() && query.tag.is_empty() && query.country.is_empty() {
        let by_tag = Query { tag: query.name.clone(), ..Default::default() };
        return search(&by_tag, limit).await;
    }
    Ok(found)
}

/// One station by its ID (the web page sends only the ID).
pub async fn by_uuid(uuid: &str) -> Result<Station, String> {
    if !valid_uuid(uuid) {
        return Err("That isn't a station ID.".into());
    }
    parse_stations(&get(&format!("/json/stations/byuuid/{uuid}")).await?)?
        .into_iter()
        .next()
        .ok_or_else(|| "That station isn't in the directory anymore.".into())
}

/// Tells the directory the station was played (their click count). Never fails loudly.
pub fn click(uuid: &str) {
    if !valid_uuid(uuid) {
        return;
    }
    let path = format!("/json/url/{uuid}");
    tokio::spawn(async move {
        let _ = get(&path).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[
      {"stationuuid":"96062a7b-0601-11e8-ae97-52543be04c81","name":" Jazz FM ","url":"http://example.com/jazz.pls","url_resolved":"https://stream.example.com/jazz","favicon":"https://example.com/j.png","tags":"jazz,smooth jazz, ,swing,blues,soul,funk","country":"United Kingdom","countrycode":"GB","codec":"MP3","bitrate":128,"lastcheckok":1,"extra":"ignored"},
      {"stationuuid":"96062a7b-0601-11e8-ae97-52543be04c81","name":"Jazz FM again","url_resolved":"https://stream.example.com/jazz"},
      {"stationuuid":"11111111-2222-3333-4444-555555555555","name":"","url":"https://only-url.example.com/live","url_resolved":"","codec":"AAC","bitrate":0},
      {"stationuuid":"aaaaaaaa-2222-3333-4444-555555555555","name":"No address","url":"rtsp://bad","url_resolved":""},
      {"name":"Missing fields"},
      {"stationuuid":"bbbbbbbb-2222-3333-4444-555555555555","name":"Nulls","url_resolved":"https://nulls.example.com/live","favicon":null,"bitrate":null,"tags":null},
      {"stationuuid":"cccccccc-2222-3333-4444-555555555555","name":"Odd","url_resolved":"https://odd.example.com/live","bitrate":"fast"},
      "not a station"
    ]"#;

    #[test]
    fn stations_are_parsed_and_unplayable_ones_dropped() {
        let stations = parse_stations(SAMPLE).unwrap();
        assert_eq!(stations.len(), 3, "duplicate, rtsp-only, address-less and malformed entries are dropped");
        assert_eq!(stations[2].title(), "Nulls", "null fields count as missing");
        let jazz = &stations[0];
        assert_eq!(jazz.title(), "Jazz FM");
        assert_eq!(jazz.stream_url(), Some("https://stream.example.com/jazz"));
        assert_eq!(jazz.tag_list(), ["jazz", "smooth jazz", "swing", "blues", "soul"]);
        assert_eq!(jazz.summary(), "United Kingdom · jazz, smooth jazz, swing, blues, soul · MP3 128 kbps");
        let bare = &stations[1];
        assert_eq!(bare.title(), "Radio");
        assert_eq!(bare.stream_url(), Some("https://only-url.example.com/live"));
        assert_eq!(bare.summary(), "AAC");
        assert!(parse_stations("{\"not\":\"a list\"}").is_err());
        assert!(parse_stations("[]").unwrap().is_empty());
    }

    #[test]
    fn a_station_plays_as_live_radio() {
        let station = &parse_stations(SAMPLE).unwrap()[0];
        let item = station.item().unwrap();
        assert_eq!(item.kind, Kind::Radio);
        assert_eq!(item.length, None, "no length: no end-of-song watchdog, no longest-song limit");
        assert_eq!(item.source, "https://stream.example.com/jazz");
        assert!(!item.allow_private_host, "a directory entry pointing at the LAN is still refused");
        let limited = crate::limits::check(vec![item], 0, 0, 5, false);
        assert_eq!(limited.kept.len(), 1, "the longest-song limit lets live radio through");
    }

    #[test]
    fn typed_searches_split_into_name_tag_and_country() {
        assert_eq!(Query::parse("jazz fm"), Query { name: "jazz fm".into(), ..Default::default() });
        assert_eq!(Query::parse("tag:classic rock country:Germany"), Query { tag: "classic rock".into(), country: "Germany".into(), ..Default::default() });
        assert_eq!(Query::parse("Genre:Jazz country:US name:smooth"), Query { name: "smooth".into(), tag: "Jazz".into(), country: "US".into() });
        assert_eq!(Query::parse("bbc tag: news"), Query { name: "bbc".into(), tag: "news".into(), ..Default::default() });
        assert!(Query::parse("   ").is_empty());
        assert!(Query::parse("tag:").is_empty());
    }

    #[test]
    fn the_search_address_is_encoded_and_bounded() {
        let path = Query { name: "rock & roll".into(), tag: "Classic Rock".into(), country: "us".into() }.search_path(500);
        assert!(path.starts_with("/json/stations/search?"));
        assert!(path.contains("name=rock%20%26%20roll"));
        assert!(path.contains("tag=classic%20rock"));
        assert!(path.contains("countrycode=US"));
        assert!(path.contains("limit=50"));
        assert!(path.contains("hidebroken=true") && path.contains("order=clickcount") && path.contains("reverse=true"));
        let path = Query { country: "United Kingdom".into(), ..Default::default() }.search_path(0);
        assert!(path.contains("country=United%20Kingdom") && path.contains("limit=1"));
        assert!(!path.contains("name="));
    }

    #[test]
    fn station_ids_are_checked() {
        assert!(valid_uuid("96062a7b-0601-11e8-ae97-52543be04c81"));
        assert!(!valid_uuid("../json/servers"));
        assert!(!valid_uuid("96062a7b-0601-11e8-ae97-52543be04c8/"));
    }

    #[tokio::test]
    async fn a_search_goes_to_the_server_with_lantern_user_agent() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = SAMPLE;
            let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            socket.write_all(reply.as_bytes()).await.unwrap();
            request
        });
        let path = Query::parse("tag:jazz").search_path(10);
        let text = get_from(&base, &path).await.unwrap();
        assert_eq!(parse_stations(&text).unwrap().len(), 3);
        let request = server.await.unwrap();
        assert!(request.starts_with("GET /json/stations/search?tag=jazz"), "{request}");
        assert!(request.to_ascii_lowercase().contains(&format!("user-agent: lantern/{}", crate::VERSION)), "{request}");
    }
}
