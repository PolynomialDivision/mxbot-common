//! Telling new events from replayed ones.
//!
//! A bot started with an empty store (a new device, a restored or repaired store) gets each
//! room's recent timeline in its first sync, and its handlers would run old commands again,
//! `!admin` commands included. [`crate::sync::initial_sync`] notes when it starts from an empty
//! store; from then on [`is_replayed`] is true for events sent before that moment. A normal
//! restart keeps the store's sync position, gets only what is new, and replays nothing: there
//! [`is_replayed`] is always false, so commands sent while the bot was down still run.
//!
//! The admin console ([`crate::admin::Admin::handle`]) skips replayed commands itself, which
//! covers every bot command behind it; bots that read other messages (no `!`) can ask too.

use std::{sync::OnceLock, time::SystemTime};

use matrix_sdk::ruma::MilliSecondsSinceUnixEpoch;

/// Events sent before this moment are replays (set once, by the first sync of an empty store).
static REPLAY_CUTOFF: OnceLock<SystemTime> = OnceLock::new();

/// Called by the initial sync when the store had no sync position.
pub(crate) fn starting_from_empty_store(now: SystemTime) {
    let _ = REPLAY_CUTOFF.set(now);
}

/// Whether an event sent at `sent` is old history the first sync of an empty store brought
/// along, not something that happened since the bot runs.
pub fn is_replayed(sent: MilliSecondsSinceUnixEpoch) -> bool {
    REPLAY_CUTOFF
        .get()
        .is_some_and(|cutoff| replayed_before(*cutoff, sent))
}

fn replayed_before(cutoff: SystemTime, sent: MilliSecondsSinceUnixEpoch) -> bool {
    sent.to_system_time().is_some_and(|sent| sent < cutoff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::UInt;
    use std::time::{Duration, UNIX_EPOCH};

    fn at(secs: u64) -> MilliSecondsSinceUnixEpoch {
        MilliSecondsSinceUnixEpoch(UInt::try_from(secs * 1000).unwrap())
    }

    #[test]
    fn only_events_from_before_an_empty_start_are_replays() {
        let cutoff = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert!(replayed_before(cutoff, at(1_799_999_000)));
        assert!(!replayed_before(cutoff, at(1_800_000_000)));
        assert!(!replayed_before(cutoff, at(1_800_000_060)));
    }

    #[test]
    fn nothing_is_a_replay_after_a_normal_start() {
        // No empty store in this process (the cutoff is never set in the unit tests).
        assert!(!is_replayed(at(0)));
    }
}
