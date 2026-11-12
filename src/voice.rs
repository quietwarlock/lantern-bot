//! Keeping the voice connection alive (1.8.3).
//!
//! Songbird keeps a server's `Call` registered after its voice connection
//! dies, and its mixer keeps playing tracks into nothing. [`VoiceWatch`]
//! listens for the driver's connect and disconnect events so [`crate::bot`]
//! can pause the song, rejoin the same channel, and carry on.

use crate::bot::Core;
use serenity::all::{ChannelId, GuildId};
use serenity::async_trait;
use songbird::events::context_data::DisconnectReason;
use songbird::{Event, EventContext, EventHandler};
use std::sync::Weak;
use std::time::Duration;

/// Wait after a drop before the first rejoin, so a real kick (which clears
/// the queue) lands first.
pub const SETTLE: Duration = Duration::from_secs(2);
/// Longest wait for one rejoin attempt.
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(30);
/// Periodic checks in a row that must find Lantern missing from voice before it rejoins.
pub const MISSING_CHECKS: u8 = 2;

/// How long to wait after failed rejoin attempt `attempt` (1-based):
/// 5 s, 10 s, 30 s, 1 min, 2 min, then every 5 min. Never spins.
pub fn rejoin_delay(attempt: u32) -> Duration {
    let secs = match attempt {
        0 | 1 => 5,
        2 => 10,
        3 => 30,
        4 => 60,
        5 => 120,
        _ => 300,
    };
    Duration::from_secs(secs)
}

/// A driver disconnect that Lantern didn't ask for. Leaving on purpose
/// (`Requested`, or no reason at all) and a join replaced by a newer one
/// (`AttemptDiscarded`) are not drops.
pub fn is_drop(reason: Option<DisconnectReason>) -> bool {
    !matches!(reason, None | Some(DisconnectReason::Requested) | Some(DisconnectReason::AttemptDiscarded))
}

/// Short words for the log.
pub fn reason_text(reason: Option<DisconnectReason>) -> String {
    match reason {
        Some(DisconnectReason::WsClosed(Some(code))) => format!("Discord closed the voice link: {code:?}"),
        Some(DisconnectReason::WsClosed(None)) => "Discord closed the voice link".into(),
        Some(DisconnectReason::TimedOut) => "timed out".into(),
        Some(DisconnectReason::Io) => "network error".into(),
        Some(other) => format!("{other:?}"),
        None => "no reason given".into(),
    }
}

/// Attached to each server's `Call` once. Holds a weak link so a stopped
/// bot session isn't kept alive by its own event handlers.
pub(crate) struct VoiceWatch {
    pub core: Weak<Core>,
    pub guild: GuildId,
}

#[async_trait]
impl EventHandler for VoiceWatch {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let Some(core) = self.core.upgrade() else { return Some(Event::Cancel) };
        match ctx {
            EventContext::DriverDisconnect(data) if is_drop(data.reason) => {
                let channel = ChannelId::new(data.channel_id.0.get());
                core.voice_dropped(self.guild, channel, &reason_text(data.reason)).await;
            }
            EventContext::DriverConnect(_) | EventContext::DriverReconnect(_) => core.voice_back(self.guild).await,
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejoin_backs_off_and_caps_at_five_minutes() {
        let waits: Vec<u64> = (1..=8).map(|a| rejoin_delay(a).as_secs()).collect();
        assert_eq!(waits, [5, 10, 30, 60, 120, 300, 300, 300]);
        assert!(waits.windows(2).all(|w| w[0] <= w[1]), "never gets shorter");
        assert_eq!(rejoin_delay(1000).as_secs(), 300);
    }

    #[test]
    fn leaving_on_purpose_is_not_a_drop() {
        assert!(!is_drop(None));
        assert!(!is_drop(Some(DisconnectReason::Requested)));
        assert!(!is_drop(Some(DisconnectReason::AttemptDiscarded)));
        assert!(is_drop(Some(DisconnectReason::WsClosed(None))));
        assert!(is_drop(Some(DisconnectReason::TimedOut)));
        assert!(is_drop(Some(DisconnectReason::Io)));
        assert!(is_drop(Some(DisconnectReason::Internal)));
    }
}
