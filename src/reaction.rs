//! Reactions as buttons: what a reaction's key means, and seeding the buttons
//! under a message.
//!
//! Clients send the same emoji with or without a variation selector (`✅` and
//! `✅` + U+FE0F), so keys are compared without them. Skin tones and ZWJ
//! sequences stay part of the key: a bot that accepts `🙋🏽` for `🙋` says so
//! itself.

use matrix_sdk::{
    ruma::{
        events::{reaction::ReactionEventContent, relation::Annotation},
        EventId, OwnedTransactionId,
    },
    Room,
};
use tracing::warn;

use crate::send::{send_with_retry, send_with_retry_txn};

/// A reaction key without variation selectors (U+FE00 to U+FE0F).
pub fn key(raw: &str) -> String {
    raw.chars()
        .filter(|c| !('\u{FE00}'..='\u{FE0F}').contains(c))
        .collect()
}

/// Whether a reaction key is one of `keys`, variation selectors aside.
pub fn is_one_of(raw: &str, keys: &[&str]) -> bool {
    let raw = key(raw);
    keys.iter().any(|candidate| key(candidate) == raw)
}

/// React to `event_id` with each of `keys`, so people have buttons to tap.
/// Best effort: a button that cannot be sent (after retries) is logged.
///
/// With `txn`, the buttons have stable transaction IDs (the first `txn`
/// itself, the next ones `txn-1`, `txn-2`, …): seeding again after a restart
/// returns the buttons already there instead of reacting twice.
pub async fn add_buttons<K: AsRef<str>>(
    room: &Room,
    event_id: &EventId,
    keys: &[K],
    txn: Option<&str>,
) {
    for (index, button) in keys.iter().enumerate() {
        let button = button.as_ref();
        let reaction =
            ReactionEventContent::new(Annotation::new(event_id.to_owned(), button.to_owned()));
        let sent = match txn {
            None => send_with_retry(room, reaction).await,
            Some(txn) => {
                let txn: OwnedTransactionId = match index {
                    0 => txn.into(),
                    _ => format!("{txn}-{index}").into(),
                };
                send_with_retry_txn(room, reaction, &txn).await
            }
        };
        if let Err(e) = sent {
            warn!(room_id = %room.room_id(), %event_id, button, "Could not add a reaction button: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variation_selectors_do_not_count() {
        assert_eq!(key("✅\u{fe0f}"), "✅");
        assert_eq!(key("✔\u{fe0e}"), "✔");
        assert_eq!(key("🙋🏽"), "🙋🏽");
        assert!(is_one_of("☑\u{fe0f}", &["✅", "☑"]));
        assert!(is_one_of("↩", &["↩️"]));
        assert!(!is_one_of("✅✅", &["✅"]));
        assert!(!is_one_of("🙋🏽", &["🙋"]));
        assert!(!is_one_of("", &["✅"]));
    }
}
