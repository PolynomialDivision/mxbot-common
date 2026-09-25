//! Invites, joining, and room classification.

use std::{collections::HashSet, sync::Arc, time::Duration};

use matrix_sdk::{
    ruma::{
        events::room::member::StrippedRoomMemberEvent, OwnedRoomId, OwnedServerName, OwnedUserId,
        RoomId, RoomOrAliasId, UserId,
    },
    Client, Room, RoomState,
};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::{
    config::{RoomAllowList, UserAllowList},
    retry::Backoff,
};

const JOIN_MAX_ATTEMPTS: u32 = 8;

/// Who may pull the bot into which rooms.
#[derive(Clone, Debug)]
pub struct InvitePolicy {
    pub inviters: UserAllowList,
    pub rooms: RoomAllowList,
    /// Admins may always invite the bot, and their direct chats bypass the
    /// room allow-list (their room IDs cannot be known in advance).
    pub admins: Arc<HashSet<OwnedUserId>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InviteDecision {
    Accept,
    RejectInviter,
    RejectRoom,
}

impl InvitePolicy {
    pub fn decide(
        &self,
        inviter: Option<&UserId>,
        room_id: &RoomId,
        is_direct: bool,
    ) -> InviteDecision {
        let is_admin = inviter.is_some_and(|user| self.admins.contains(user));
        if is_admin && is_direct {
            return InviteDecision::Accept;
        }
        let inviter_ok = is_admin
            || match inviter {
                Some(user) => self.inviters.allows(user),
                None => self.inviters.is_all(),
            };
        if !inviter_ok {
            InviteDecision::RejectInviter
        } else if !self.rooms.allows(room_id) {
            InviteDecision::RejectRoom
        } else {
            InviteDecision::Accept
        }
    }
}

/// Returns true for Matrix join errors that will not resolve with a retry.
pub fn is_join_terminal(error: &matrix_sdk::Error) -> bool {
    let message = error.to_string();
    message.contains("No known servers")
        || message.contains("M_FORBIDDEN")
        || message.contains("M_UNKNOWN_TOKEN")
        || message.contains("M_GUEST_ACCESS_FORBIDDEN")
}

/// Via servers for joining: the inviter's server always knows the room;
/// the room ID's own server covers non-federated rooms. Without them
/// matrix.org answers 404 "No known servers".
fn via_servers(room_id: &RoomId, inviter: Option<&UserId>) -> Vec<OwnedServerName> {
    let mut via: Vec<OwnedServerName> = inviter
        .map(|user| user.server_name().to_owned())
        .into_iter()
        .collect();
    if let Some(server) = room_id.server_name() {
        let server = server.to_owned();
        if !via.contains(&server) {
            via.push(server);
        }
    }
    via
}

/// Join a room, retrying transient failures with exponential backoff.
pub async fn join_with_retry(client: &Client, room_id: &RoomId, inviter: Option<&UserId>) -> bool {
    let via = via_servers(room_id, inviter);
    let room_or_alias: &RoomOrAliasId = room_id.into();
    let mut backoff = Backoff::new(Duration::from_secs(2), Duration::from_secs(300));
    for attempt in 1..=JOIN_MAX_ATTEMPTS {
        match client.join_room_by_id_or_alias(room_or_alias, &via).await {
            Ok(_) => {
                info!(room_id = %room_id, "Joined room");
                return true;
            }
            Err(ref error) if is_join_terminal(error) => {
                warn!(room_id = %room_id, %error, "Join failed with a non-retryable error");
                return false;
            }
            Err(error) if attempt == JOIN_MAX_ATTEMPTS => {
                warn!(room_id = %room_id, %error, "Join failed after {JOIN_MAX_ATTEMPTS} attempts");
            }
            Err(error) => {
                let delay = backoff.next_delay();
                warn!(
                    room_id = %room_id,
                    %error,
                    "Join attempt {attempt}/{JOIN_MAX_ATTEMPTS} failed; retry in {}s",
                    delay.as_secs()
                );
                sleep(delay).await;
            }
        }
    }
    false
}

async fn handle_invite(
    client: Client,
    room: Room,
    policy: &InvitePolicy,
    inviter: Option<OwnedUserId>,
    is_direct: bool,
) {
    let room_id = room.room_id().to_owned();
    match policy.decide(inviter.as_deref(), &room_id, is_direct) {
        InviteDecision::Accept => {
            info!(room_id = %room_id, inviter = ?inviter, is_direct, "Accepting invite");
            join_with_retry(&client, &room_id, inviter.as_deref()).await;
        }
        decision => {
            warn!(room_id = %room_id, inviter = ?inviter, ?decision, "Rejecting invite");
            if let Err(error) = room.leave().await {
                warn!(room_id = %room_id, %error, "Failed to reject invite");
            }
        }
    }
}

/// Auto-join invites allowed by `policy`; reject the rest.
pub fn install_invite_handler(client: &Client, policy: InvitePolicy) {
    let policy = Arc::new(policy);
    client.add_event_handler(
        move |event: StrippedRoomMemberEvent, room: Room, client: Client| {
            let policy = Arc::clone(&policy);
            async move {
                if client.user_id() != Some(event.state_key.as_ref()) {
                    return;
                }
                if room.state() != RoomState::Invited {
                    return;
                }
                let is_direct = event.content.is_direct.unwrap_or(false);
                tokio::spawn(async move {
                    handle_invite(client, room, &policy, Some(event.sender), is_direct).await;
                });
            }
        },
    );
}

/// Process invites that arrived while the bot was offline.
///
/// `StrippedRoomMemberEvent` only fires for invites received during this
/// session, not for ones already persisted in the store.
pub async fn join_pending_invites(client: &Client, policy: &InvitePolicy) {
    let invited = client.invited_rooms();
    if invited.is_empty() {
        return;
    }
    info!(count = invited.len(), "Processing pending invites");
    for room in invited {
        let inviter = match room.invite_details().await {
            Ok(details) => Some(details.inviter_id),
            Err(error) => {
                warn!(room_id = %room.room_id(), %error, "Could not determine inviter of pending invite");
                None
            }
        };
        let is_direct = room.is_direct().await.unwrap_or(false);
        handle_invite(client.clone(), room, policy, inviter, is_direct).await;
    }
}

/// If `room` is a direct chat between the bot and exactly one of `admins`,
/// returns that admin.
///
/// Uses the bot's own `m.direct` account data (which only the bot can
/// write) and requires that nobody else is joined or invited.
pub fn admin_dm_partner(room: &Room, admins: &HashSet<OwnedUserId>) -> Option<OwnedUserId> {
    if room.state() != RoomState::Joined || room.active_members_count() > 2 {
        return None;
    }
    let targets = room.direct_targets();
    let mut targets = targets.iter();
    let (Some(target), None) = (targets.next(), targets.next()) else {
        return None;
    };
    let user_id = target.as_user_id()?;
    admins.contains(user_id).then(|| user_id.to_owned())
}

/// Joined rooms that should receive the bot's broadcast posts: those allowed
/// by `rooms`, minus admin direct chats unless `include_admin_dms`.
pub fn broadcast_rooms(
    client: &Client,
    rooms: &RoomAllowList,
    admins: &HashSet<OwnedUserId>,
    include_admin_dms: bool,
) -> Vec<Room> {
    client
        .joined_rooms()
        .into_iter()
        .filter(|room| rooms.allows(room.room_id()))
        .filter(|room| include_admin_dms || admin_dm_partner(room, admins).is_none())
        .collect()
}

/// Parse a configured room ID with a helpful error.
pub fn parse_room_id(field: &str, value: &str) -> anyhow::Result<OwnedRoomId> {
    value
        .parse()
        .map_err(|error| anyhow::anyhow!("Invalid room ID in {field}: {value:?} ({error})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{room_id, user_id};

    fn policy(inviters: UserAllowList, rooms: RoomAllowList) -> InvitePolicy {
        InvitePolicy {
            inviters,
            rooms,
            admins: Arc::new(HashSet::from([user_id!("@admin:example.org").to_owned()])),
        }
    }

    #[test]
    fn admin_direct_chats_bypass_both_allow_lists() {
        let p = policy(
            UserAllowList::Only(HashSet::new()),
            RoomAllowList::Only(HashSet::new()),
        );
        let room = room_id!("!dm:example.org");
        assert_eq!(
            p.decide(Some(user_id!("@admin:example.org")), room, true),
            InviteDecision::Accept
        );
        assert_eq!(
            p.decide(Some(user_id!("@admin:example.org")), room, false),
            InviteDecision::RejectRoom
        );
        assert_eq!(
            p.decide(Some(user_id!("@eve:example.org")), room, true),
            InviteDecision::RejectInviter
        );
    }

    #[test]
    fn unknown_inviter_is_only_accepted_when_everyone_may_invite() {
        let room = room_id!("!room:example.org");
        assert_eq!(
            policy(UserAllowList::All, RoomAllowList::All).decide(None, room, false),
            InviteDecision::Accept
        );
        let only = UserAllowList::Only(HashSet::from([user_id!("@alice:example.org").to_owned()]));
        assert_eq!(
            policy(only, RoomAllowList::All).decide(None, room, false),
            InviteDecision::RejectInviter
        );
    }

    #[test]
    fn via_servers_are_deduplicated() {
        let via = via_servers(room_id!("!r:example.org"), Some(user_id!("@a:example.org")));
        assert_eq!(via.len(), 1);
        let via = via_servers(room_id!("!r:other.org"), Some(user_id!("@a:example.org")));
        assert_eq!(via.len(), 2);
    }
}
