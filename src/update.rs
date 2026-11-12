//! Update check (1.7). At startup Lantern asks GitHub for the latest release
//! of its repository and, if it's newer, says so in the window. It never
//! downloads or installs anything. Turn it off in Settings.

use serde::Deserialize;
use std::time::Duration;

/// `owner/name` taken from Cargo.toml's `repository`.
pub fn repo() -> Option<&'static str> {
    let url = option_env!("CARGO_PKG_REPOSITORY")?;
    let path = url.trim_end_matches('/').trim_end_matches(".git").strip_prefix("https://github.com/")?;
    (path.split('/').count() == 2 && !path.is_empty()).then_some(path)
}

/// `v1.7.0`, `1.7`, `1.7.0-beta` → (1, 7, 0). Pre-release text is ignored.
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let core = text.trim().trim_start_matches(['v', 'V']).split(['-', '+', ' ']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>());
    let major = parts.next()?.ok()?;
    let minor = parts.next().unwrap_or(Ok(0)).ok()?;
    let patch = parts.next().unwrap_or(Ok(0)).ok()?;
    Some((major, minor, patch))
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// The newer version and its page, or `None` (up to date, or no answer).
pub async fn check() -> Result<Option<(String, String)>, String> {
    let Some(repo) = repo() else { return Ok(None) };
    let client = reqwest::Client::builder()
        .user_agent(concat!("Lantern/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(format!("https://api.github.com/repos/{repo}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let release: Release = response.error_for_status().map_err(|e| e.to_string())?.json().await.map_err(|e| e.to_string())?;
    Ok((!release.draft && !release.prerelease && is_newer(&release.tag_name, crate::VERSION))
        .then(|| (release.tag_name.trim_start_matches('v').to_string(), release.html_url)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        assert_eq!(parse_version("v1.7.0"), Some((1, 7, 0)));
        assert_eq!(parse_version("1.8"), Some((1, 8, 0)));
        assert_eq!(parse_version("2.0.1-beta"), Some((2, 0, 1)));
        assert_eq!(parse_version("latest"), None);
        assert!(is_newer("v1.10.0", "1.9.9"));
        assert!(!is_newer("v1.7.0", "1.7.0"));
        assert!(!is_newer("v1.6.1", "1.7.0"));
        assert!(!is_newer("nonsense", "1.7.0"));
    }

    #[test]
    fn repo_comes_from_cargo() {
        if let Some(repo) = repo() {
            assert_eq!(repo.split('/').count(), 2);
        }
    }

    #[test]
    fn release_json_parses() {
        let r: Release = serde_json::from_str(r#"{"tag_name":"v9.0.0","html_url":"https://github.com/o/r/releases/tag/v9.0.0","draft":false,"prerelease":false,"assets":[]}"#).unwrap();
        assert!(is_newer(&r.tag_name, crate::VERSION));
    }
}
