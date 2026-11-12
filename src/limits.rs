//! Per-user queue limit and longest song (1.7). Both are off (0) by default.
//! Controllers (owner and DJs) are never limited.

use crate::media::Item;

#[derive(Debug, PartialEq)]
pub struct Checked {
    pub kept: Vec<Item>,
    /// Songs left out because they were too long.
    pub too_long: usize,
    /// Songs left out because the person already has their share queued.
    pub over_limit: usize,
}

/// `already` is how many songs this person has waiting in the queue.
/// `max_per_user` 0 = no limit; `max_minutes` 0 = any length. Songs of
/// unknown length (radio, some links) are allowed.
pub fn check(items: Vec<Item>, already: usize, max_per_user: usize, max_minutes: u32, exempt: bool) -> Checked {
    if exempt {
        return Checked { kept: items, too_long: 0, over_limit: 0 };
    }
    let mut kept = Vec::new();
    let (mut too_long, mut over_limit) = (0, 0);
    for item in items {
        if max_minutes > 0 && item.length.is_some_and(|secs| secs > u64::from(max_minutes) * 60) {
            too_long += 1;
        } else if max_per_user > 0 && already + kept.len() >= max_per_user {
            over_limit += 1;
        } else {
            kept.push(item);
        }
    }
    Checked { kept, too_long, over_limit }
}

/// The reply when something was left out, or `None`.
pub fn note(checked: &Checked, max_per_user: usize, max_minutes: u32) -> Option<String> {
    let mut parts = Vec::new();
    if checked.too_long > 0 {
        parts.push(crate::i18n::tf("limit_too_long", &[&checked.too_long.to_string(), &max_minutes.to_string()]));
    }
    if checked.over_limit > 0 {
        parts.push(crate::i18n::tf("limit_per_user", &[&checked.over_limit.to_string(), &max_per_user.to_string()]));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::Kind;

    fn song(n: u64, length: Option<u64>) -> Item {
        Item { title: format!("S{n}"), source: format!("https://youtu.be/{n}"), query: None, kind: Kind::YouTube, length, allow_private_host: false, requested_by: None }
    }

    #[test]
    fn off_by_default_keeps_everything() {
        let items = vec![song(1, Some(99_999)), song(2, None)];
        let c = check(items.clone(), 500, 0, 0, false);
        assert_eq!(c.kept, items);
        assert!(note(&c, 0, 0).is_none());
    }

    #[test]
    fn long_songs_and_extra_songs_are_left_out() {
        let items = vec![song(1, Some(600)), song(2, Some(30 * 60)), song(3, None), song(4, Some(60))];
        let c = check(items, 1, 3, 10, false);
        assert_eq!(c.kept.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), ["S1", "S3"]);
        assert_eq!((c.too_long, c.over_limit), (1, 1));
        let text = note(&c, 3, 10).unwrap();
        assert!(text.contains("10") && text.contains('3'), "{text}");
        assert_eq!(check(vec![song(9, Some(9999))], 99, 1, 1, true).kept.len(), 1);
    }
}
