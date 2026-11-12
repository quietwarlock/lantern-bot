//! First-run setup wizard rules (1.7). The window walks a new user through
//! creating a Discord app, pasting the token, inviting the bot and setting an
//! owner. Everything here is pure so it can be tested without a window.

/// Permissions in the invite link (see the README table).
pub const INVITE_PERMISSIONS: u64 = 3_238_976;
pub const PORTAL_URL: &str = "https://discord.com/developers/applications";

/// The wizard's pages, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    CreateApp,
    Token,
    Invite,
    Owner,
    Finish,
}

impl Step {
    pub const ALL: [Step; 5] = [Step::CreateApp, Step::Token, Step::Invite, Step::Owner, Step::Finish];

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
    pub fn next(self) -> Step {
        Self::ALL[(self.index() + 1).min(Self::ALL.len() - 1)]
    }
    pub fn back(self) -> Step {
        Self::ALL[self.index().saturating_sub(1)]
    }
    pub fn title(self) -> &'static str {
        match self {
            Step::CreateApp => "1. Create a Discord bot",
            Step::Token => "2. Paste the bot token",
            Step::Invite => "3. Invite the bot to your server",
            Step::Owner => "4. Your Discord user ID (optional)",
            Step::Finish => "5. Start Lantern",
        }
    }
}

/// The wizard opens by itself when there's no token yet.
pub fn needs_setup(token: &str) -> bool {
    token.trim().is_empty()
}

fn b64_value(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a') as u32 + 26),
        b'0'..=b'9' => Some((c - b'0') as u32 + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

/// Decodes unpadded (URL-safe or standard) base64.
fn b64_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.bytes() {
        acc = (acc << 6) | b64_value(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The application (bot user) ID is the first part of a bot token.
pub fn app_id_from_token(token: &str) -> Option<u64> {
    let first = token.trim().trim_start_matches("Bot ").split('.').next()?;
    let text = String::from_utf8(b64_decode(first)?).ok()?;
    is_snowflake(&text).then(|| text.parse().ok()).flatten()
}

/// A Discord ID: 17–20 digits.
pub fn is_snowflake(text: &str) -> bool {
    (17..=20).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit())
}

/// Checks the shape of a token without contacting Discord. The message never
/// contains the token.
pub fn check_token(token: &str) -> Result<(), &'static str> {
    let token = token.trim().trim_matches('"').trim_start_matches("Bot ").trim();
    if token.is_empty() {
        return Err("Paste the token from the Bot page (Reset Token).");
    }
    if token.split('.').count() != 3 || token.len() < 50 {
        return Err("That doesn't look like a bot token. It has three parts separated by dots. Don't paste the Application ID or Client Secret.");
    }
    if app_id_from_token(token).is_none() {
        return Err("That doesn't look like a bot token. Copy it again with Reset Token.");
    }
    Ok(())
}

/// Empty is fine (no owner). Otherwise it must be a user ID.
pub fn check_owner(owner: &str) -> Result<(), &'static str> {
    let owner = owner.trim();
    if owner.is_empty() || is_snowflake(owner) {
        Ok(())
    } else {
        Err("A user ID is 17 to 20 digits. In Discord: Settings > Advanced > Developer Mode, then right-click your name > Copy User ID.")
    }
}

pub fn invite_url(app_id: u64) -> String {
    format!("https://discord.com/oauth2/authorize?client_id={app_id}&scope=bot%20applications.commands&permissions={INVITE_PERMISSIONS}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // A made-up token: base64("123456789012345678") . junk . junk
    const FAKE: &str = "MTIzNDU2Nzg5MDEyMzQ1Njc4.GabcdE.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN";

    #[test]
    fn app_id_comes_from_the_token() {
        assert_eq!(app_id_from_token(FAKE), Some(123456789012345678));
        assert_eq!(app_id_from_token("nonsense"), None);
        assert!(invite_url(123456789012345678).contains("client_id=123456789012345678&scope=bot%20applications.commands&permissions=3238976"));
    }

    #[test]
    fn token_and_owner_checks() {
        assert!(check_token(FAKE).is_ok());
        assert!(check_token(&format!("Bot {FAKE}")).is_ok());
        assert!(check_token("").is_err());
        assert!(check_token("123456789012345678").is_err());
        let err = check_token("abc.def.ghi").unwrap_err();
        assert!(!err.contains("abc"));
        assert!(check_owner("").is_ok());
        assert!(check_owner("123456789012345678").is_ok());
        assert!(check_owner("@me").is_err());
    }

    #[test]
    fn steps_walk_in_order() {
        assert!(needs_setup("  "));
        assert!(!needs_setup(FAKE));
        assert_eq!(Step::CreateApp.next(), Step::Token);
        assert_eq!(Step::Finish.next(), Step::Finish);
        assert_eq!(Step::CreateApp.back(), Step::CreateApp);
        assert_eq!(Step::Owner.back(), Step::Invite);
    }
}
