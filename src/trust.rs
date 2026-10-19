//! `[security] trust`: whose devices count.
//!
//! * `verified` (default): an identity the bot verified stays the only one.
//!   If that person resets their keys, the SDK refuses to encrypt for any
//!   room they are in (`VerifiedUserChangedIdentity`) until they verify
//!   again, which another admin allows with `!reset-trust`.
//! * `cross_signed`: the bot follows each person's current identity. When a
//!   verified identity changes, the bot drops its old verification (logged),
//!   so the devices signed by the new identity count again: sending goes on
//!   and admin commands work from them. A device nobody signed still gets
//!   nothing. Goes with `encryption_strategy = "identity_based"`.
//!
//! What `cross_signed` gives up: someone who takes over an account entirely
//! (password and a key reset) is trusted like its owner.

use std::{
    collections::BTreeSet,
    sync::atomic::{AtomicBool, Ordering},
};

use futures_util::StreamExt;
use matrix_sdk::{ruma::OwnedUserId, Client, RoomMemberships};
use matrix_sdk_crypto::{OlmError, SessionRecipientCollectionError};
use tracing::{info, warn};

use crate::config::Trust;

static CROSS_SIGNED: AtomicBool = AtomicBool::new(false);

/// Set once at startup by [`crate::Bot`].
pub(crate) fn set(trust: Trust) {
    CROSS_SIGNED.store(trust == Trust::CrossSigned, Ordering::Relaxed);
}

/// `trust = "cross_signed"`: a changed identity is accepted.
pub fn follows_new_identities() -> bool {
    CROSS_SIGNED.load(Ordering::Relaxed)
}

/// The people a send failed for because their identity changed since the
/// bot verified them.
pub fn changed_identities(error: &matrix_sdk::Error) -> Option<&[OwnedUserId]> {
    match error {
        matrix_sdk::Error::OlmError(olm) => match olm.as_ref() {
            OlmError::SessionRecipientCollectionError(
                SessionRecipientCollectionError::VerifiedUserChangedIdentity(users),
            ) => Some(users),
            _ => None,
        },
        _ => None,
    }
}

/// Drop the bot's old verification of each of `users` whose identity
/// changed since: their current identity counts from now on. Returns
/// whether any changed.
pub async fn accept_new_identities<'a>(
    client: &Client,
    users: impl IntoIterator<Item = &'a OwnedUserId>,
) -> bool {
    let mut accepted = false;
    for user_id in users {
        let identity = match client.encryption().get_user_identity(user_id).await {
            Ok(Some(identity)) if identity.has_verification_violation() => identity,
            Ok(_) => continue,
            Err(error) => {
                warn!(%user_id, %error, "Could not look up an identity");
                continue;
            }
        };
        match identity.withdraw_verification().await {
            Ok(()) => {
                warn!(
                    %user_id,
                    "Their identity changed since we verified; accepting the new one (trust = cross_signed)"
                );
                accepted = true;
            }
            Err(error) => warn!(%user_id, %error, "Could not accept a new identity"),
        }
    }
    accepted
}

/// Everyone in the bot's rooms whose identity changed while it was away.
pub(crate) async fn accept_changed_in_rooms(client: &Client) {
    let mut users = BTreeSet::new();
    for room in client.joined_rooms() {
        match room.members_no_sync(RoomMemberships::JOIN).await {
            Ok(members) => users.extend(members.iter().map(|m| m.user_id().to_owned())),
            Err(error) => warn!(room_id = %room.room_id(), %error, "Could not list members"),
        }
    }
    accept_new_identities(client, &users).await;
}

/// From now on, accept a changed identity as soon as the bot learns of it.
pub(crate) async fn watch(client: Client) {
    let updates = match client.encryption().user_identities_stream().await {
        Ok(updates) => updates,
        Err(error) => {
            warn!(%error, "Cannot follow identity changes; they are accepted when a send fails");
            return;
        }
    };
    info!("Following identity changes (trust = cross_signed)");
    tokio::spawn(async move {
        let mut updates = Box::pin(updates);
        while let Some(update) = updates.next().await {
            let changed: Vec<OwnedUserId> = update
                .new
                .into_iter()
                .chain(update.changed)
                .filter(|(_, identity)| identity.has_verification_violation())
                .map(|(user_id, _)| user_id)
                .collect();
            if !changed.is_empty() {
                accept_new_identities(&client, &changed).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::user_id;

    #[test]
    fn a_changed_identity_is_read_from_a_failed_send() {
        let alice = user_id!("@alice:example.org").to_owned();
        let error = matrix_sdk::Error::from(OlmError::SessionRecipientCollectionError(
            SessionRecipientCollectionError::VerifiedUserChangedIdentity(vec![alice.clone()]),
        ));
        assert_eq!(changed_identities(&error), Some(&[alice][..]));
        let other = matrix_sdk::Error::from(OlmError::SessionRecipientCollectionError(
            SessionRecipientCollectionError::CrossSigningNotSetup,
        ));
        assert_eq!(changed_identities(&other), None);
    }
}
