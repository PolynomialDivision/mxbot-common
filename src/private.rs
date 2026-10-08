//! Private 1:1 rooms between the bot and one user, and membership checks
//! that ask the homeserver instead of trusting the local store.
//!
//! Authorization never relies on names or on `m.direct` hints (anyone can
//! mark any room as direct). Everything here fails closed: an error, an
//! extra member, a pending invite or a missing encryption event all mean
//! "not private" / "not a member".

use anyhow::{Context, Result};
use matrix_sdk::{
    ruma::{
        api::client::{
            discovery::get_capabilities::v3::{RoomVersionStability, RoomVersionsCapability},
            room::create_room::{self, RoomPowerLevelsContentOverride},
            state::{get_state_event_for_key, get_state_events},
        },
        assign,
        events::{room::encryption::RoomEncryptionEventContent, InitialStateEvent, StateEventType},
        int,
        serde::Raw,
        RoomId, RoomVersionId, UserId,
    },
    Client, Room,
};
use serde_json::Value;

/// The room version for a new room: the first of `preferred` the server
/// supports as *stable*, else `None` (= the server's default). Room
/// version IDs are opaque strings, so "newest" is never computed — the
/// caller lists the versions it has intentionally tested, best first.
pub fn choose_room_version(
    capability: &RoomVersionsCapability,
    preferred: &[RoomVersionId],
) -> Option<RoomVersionId> {
    preferred
        .iter()
        .find(|version| {
            matches!(
                capability.available.get(*version),
                Some(RoomVersionStability::Stable)
            )
        })
        .cloned()
}

/// The request [`create_private_dm`] sends: an encrypted direct chat
/// inviting only `user`, where only the bot may change state, invite,
/// kick or ban. The bot stays the room's creator (with the highest power
/// level in every room version); `user` keeps the default level 0, which
/// still allows sending messages and reactions and redacting their own.
pub fn private_dm_request(
    user: &UserId,
    version: Option<RoomVersionId>,
) -> create_room::v3::Request {
    let initial_state = vec![InitialStateEvent::with_empty_state_key(
        RoomEncryptionEventContent::with_recommended_defaults(),
    )
    .to_raw_any()];
    // No `users` map: it would replace the server's (which seats the
    // creator at 100 in room versions before 12, and must not list the
    // creator from 12 on).
    let power_levels = assign!(RoomPowerLevelsContentOverride::default(), {
        users_default: Some(int!(0)),
        events_default: Some(int!(0)),
        state_default: Some(int!(100)),
        invite: Some(int!(100)),
        kick: Some(int!(100)),
        ban: Some(int!(100)),
        redact: Some(int!(100)),
    });
    assign!(create_room::v3::Request::new(), {
        invite: vec![user.to_owned()],
        is_direct: true,
        preset: Some(create_room::v3::RoomPreset::PrivateChat),
        initial_state,
        room_version: version,
        power_level_content_override: Raw::new(&power_levels).ok(),
    })
}

/// Create an encrypted private chat with `user` (see [`private_dm_request`])
/// in the first of `preferred` room versions the server supports as stable,
/// else in its default version.
pub async fn create_private_dm(
    client: &Client,
    user: &UserId,
    preferred: &[RoomVersionId],
) -> Result<Room> {
    let version = match client.homeserver_capabilities().room_versions().await {
        Ok(capability) => choose_room_version(&capability, preferred),
        Err(error) => {
            tracing::warn!(%error, "Could not read the server's room versions; using its default");
            None
        }
    };
    tracing::info!(version = ?version.as_ref().map(|v| v.as_str()), "Creating a private chat");
    client
        .create_room(private_dm_request(user, version))
        .await
        .context("Creating a private chat")
}

/// Whether the room state `events` (as served by `/state`) describe a
/// private chat of exactly `bot` and `user`, both joined, nobody else
/// joined, invited or knocking, and end-to-end encrypted.
pub fn is_private_state(events: &[Value], bot: &str, user: &str) -> bool {
    let active: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "m.room.member")
        .filter(|e| {
            matches!(
                e["content"]["membership"].as_str(),
                Some("join" | "invite" | "knock")
            )
        })
        .collect();
    bot != user
        && active.len() == 2
        && [bot, user].iter().all(|id| {
            active
                .iter()
                .any(|e| e["state_key"] == *id && e["content"]["membership"] == "join")
        })
        && events.iter().any(|e| {
            e["type"] == "m.room.encryption" && e["content"]["algorithm"] == "m.megolm.v1.aes-sha2"
        })
}

/// [`is_private_state`] for `room_id`, from the server's current state.
/// `false` on any error.
pub async fn is_private_dm(client: &Client, room_id: &RoomId, user: &UserId) -> bool {
    let Some(bot) = client.user_id() else {
        return false;
    };
    let Ok(response) = client
        .send(get_state_events::v3::Request::new(room_id.to_owned()))
        .await
    else {
        return false;
    };
    let events: Vec<Value> = response
        .room_state
        .iter()
        .filter_map(|e| e.deserialize_as::<Value>().ok())
        .collect();
    is_private_state(&events, bot.as_str(), user.as_str())
}

/// Whether `user` is joined to `room_id` right now, asked of the server
/// (the bot must be in the room to see it). `Ok(false)` when they have no
/// membership there at all; an error for anything the server wouldn't
/// answer — callers treat that as "no".
pub async fn is_joined_now(client: &Client, room_id: &RoomId, user: &UserId) -> Result<bool> {
    let request = get_state_event_for_key::v3::Request::new(
        room_id.to_owned(),
        StateEventType::RoomMember,
        user.as_str().to_owned(),
    );
    match client.send(request).await {
        Ok(response) => {
            let content: Value = serde_json::from_str(response.event_or_content.get())
                .context("Reading a membership event")?;
            Ok(content["membership"] == "join")
        }
        Err(error)
            if error
                .as_client_api_error()
                .is_some_and(|e| e.status_code.as_u16() == 404) =>
        {
            Ok(false)
        }
        Err(error) => Err(error).context("Asking the server for a membership"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::user_id;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn member(id: &str, membership: &str) -> Value {
        json!({"type":"m.room.member", "state_key":id, "content":{"membership":membership}})
    }

    #[test]
    fn only_two_joined_members_and_encryption_are_private() {
        let mut events = vec![
            member("@bot:x", "join"),
            member("@a:x", "join"),
            json!({"type":"m.room.encryption","content":{"algorithm":"m.megolm.v1.aes-sha2"}}),
        ];
        assert!(is_private_state(&events, "@bot:x", "@a:x"));
        assert!(!is_private_state(&events, "@bot:x", "@other:x"));
        assert!(!is_private_state(&events, "@bot:x", "@bot:x"));
        for membership in ["invite", "join", "knock"] {
            events.push(member("@third:x", membership));
            assert!(!is_private_state(&events, "@bot:x", "@a:x"));
            events.pop();
        }
        // Someone who left doesn't count.
        events.push(member("@third:x", "leave"));
        assert!(is_private_state(&events, "@bot:x", "@a:x"));
        events.pop();
        // The user only invited, not joined yet.
        events[1] = member("@a:x", "invite");
        assert!(!is_private_state(&events, "@bot:x", "@a:x"));
        events[1] = member("@a:x", "join");
        // No encryption.
        events.pop();
        assert!(!is_private_state(&events, "@bot:x", "@a:x"));
    }

    fn capability(available: &[(&str, RoomVersionStability)]) -> RoomVersionsCapability {
        RoomVersionsCapability::new(
            RoomVersionId::V10,
            available
                .iter()
                .map(|(v, s)| (RoomVersionId::try_from(*v).unwrap(), s.clone()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    #[test]
    fn room_version_is_the_first_preferred_stable_one_never_a_numeric_max() {
        use RoomVersionStability::*;
        let preferred = [RoomVersionId::V12, RoomVersionId::V11];
        let cap = capability(&[("10", Stable), ("11", Stable), ("12", Stable)]);
        assert_eq!(
            choose_room_version(&cap, &preferred),
            Some(RoomVersionId::V12)
        );
        // 12 only unstable: the next preferred one.
        let cap = capability(&[("10", Stable), ("11", Stable), ("12", Unstable)]);
        assert_eq!(
            choose_room_version(&cap, &preferred),
            Some(RoomVersionId::V11)
        );
        // A "bigger" unknown version is never picked by itself.
        let cap = capability(&[("10", Stable), ("99", Stable)]);
        assert_eq!(choose_room_version(&cap, &preferred), None);
    }

    #[test]
    fn the_private_dm_is_encrypted_direct_and_only_the_bot_rules_it() {
        let request = private_dm_request(user_id!("@a:x"), Some(RoomVersionId::V12));
        assert_eq!(request.invite, [user_id!("@a:x").to_owned()]);
        assert!(request.is_direct);
        assert_eq!(request.room_version, Some(RoomVersionId::V12));
        assert_eq!(
            request.preset,
            Some(create_room::v3::RoomPreset::PrivateChat)
        );
        let state: Vec<Value> = request
            .initial_state
            .iter()
            .map(|e| e.deserialize_as::<Value>().unwrap())
            .collect();
        assert!(state.iter().any(|e| e["type"] == "m.room.encryption"
            && e["content"]["algorithm"] == "m.megolm.v1.aes-sha2"));
        let levels: Value = request
            .power_level_content_override
            .unwrap()
            .deserialize_as()
            .unwrap();
        assert_eq!(levels["invite"], 100);
        assert_eq!(levels["state_default"], 100);
        assert_eq!(levels["users_default"], 0);
        assert!(levels.get("users").is_none());
    }
}
