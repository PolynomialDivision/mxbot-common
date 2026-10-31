//! Invites, joining, and room classification.
//!
//! Who may open a direct chat with a bot: the admins ([`crate::admin::Admins`]), the members of
//! `[security] dm_rooms`, and, for a bot that reads the house directory, the accounts in
//! [`DmAccounts`]: every account the directory recognises as an active person's in the groups
//! whose rooms the bot manages while its read may be used (not a guest's, not a former
//! resident's), and each account such a person just asked to link (a link request) for a slot. Such
//! a chat is joined, then kept only while it is the bot and that account alone, encrypted
//! ([`crate::private::is_private_dm`]); what the bot answers there is its own business (plenum-bot:
//! `!link` only).

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
    time::{Duration, SystemTime},
};

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

/// Rooms allowed at runtime on top of `[security] allowed_rooms`: rooms an
/// admin authorized through a bot command, say. A shared handle: the bot
/// updates it, the invite handler reads it. Cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct DynamicRooms(Arc<RwLock<HashSet<OwnedRoomId>>>);

impl DynamicRooms {
    pub fn new(rooms: impl IntoIterator<Item = OwnedRoomId>) -> Self {
        Self(Arc::new(RwLock::new(rooms.into_iter().collect())))
    }

    /// Replace the whole set.
    pub fn set(&self, rooms: impl IntoIterator<Item = OwnedRoomId>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = rooms.into_iter().collect();
    }

    pub fn insert(&self, room_id: OwnedRoomId) {
        self.0
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(room_id);
    }

    pub fn remove(&self, room_id: &RoomId) {
        self.0
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(room_id);
    }

    pub fn contains(&self, room_id: &RoomId) -> bool {
        self.0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(room_id)
    }
}

/// Accounts that may open a private direct chat with the bot for a while, besides the admins
/// and `dm_rooms`: a shared handle (the bot's directory loop fills it, the invite handler reads
/// it), cheap to clone. Two parts, each expiring by itself:
/// * `known`: the accounts the directory recognises as active people's in the bot's groups
///   ([`known_accounts`]), until the read is too old ([`DmAccounts::follow`]); a later read
///   replaces them;
/// * `announced`: the account of each fresh link request, until a slot after it was made
///   ([`DmAccounts::announce`]); a later call replaces them.
///
/// A bot that will fill it calls [`DmAccounts::wait_for_first_read`] before its first sync:
/// until the first [`DmAccounts::set_known`], a direct-chat invite nobody else allows is left
/// pending instead of being rejected, and [`crate::Bot::process_pending_invites`] after that
/// read takes it up again. So a resident who wrote while the bot restarted is not turned away
/// because the directory had not been read yet.
#[derive(Clone, Debug, Default)]
pub struct DmAccounts(Arc<RwLock<DmAccountsInner>>);

#[derive(Debug, Default)]
struct DmAccountsInner {
    known: HashSet<OwnedUserId>,
    known_until: Option<SystemTime>,
    announced: HashMap<OwnedUserId, SystemTime>,
    /// Set by `wait_for_first_read`, cleared by the first `set_known`.
    waiting: bool,
}

/// What [`DmAccounts`] says about a direct chat from an account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmVerdict {
    /// Known or announced now: join, and keep it if it is private.
    Allowed,
    /// Not known, but the directory has not been read yet: leave the invite pending.
    Wait,
    /// Not known.
    Unknown,
}

impl DmAccounts {
    /// The bot will fill it from the directory: hold direct-chat invites until the first read.
    pub fn wait_for_first_read(&self) {
        self.write().waiting = true;
    }

    /// Whether invites are held for the first read.
    pub fn is_waiting(&self) -> bool {
        self.read().waiting
    }

    /// The accounts of the directory's active people, valid until `until` (the read's time
    /// plus how long it may be used). Replaces the previous ones and ends the wait for the
    /// first read.
    pub fn set_known(&self, accounts: HashSet<OwnedUserId>, until: SystemTime) {
        let mut inner = self.write();
        if inner.known.len() != accounts.len() {
            info!(
                accounts = accounts.len(),
                "Direct chats: accounts the directory knows"
            );
        }
        inner.known = accounts;
        inner.known_until = Some(until);
        inner.waiting = false;
    }

    /// The accounts of fresh link requests, each valid until its own time. Replaces the
    /// previous ones.
    pub fn set_announced(&self, accounts: impl IntoIterator<Item = (OwnedUserId, SystemTime)>) {
        let mut announced: HashMap<OwnedUserId, SystemTime> = HashMap::new();
        for (account, until) in accounts {
            let entry = announced.entry(account).or_insert(until);
            *entry = (*entry).max(until);
        }
        self.write().announced = announced;
    }

    pub fn verdict(&self, account: &UserId) -> DmVerdict {
        self.verdict_at(account, SystemTime::now())
    }

    pub fn verdict_at(&self, account: &UserId, now: SystemTime) -> DmVerdict {
        let inner = self.read();
        let known =
            inner.known_until.is_some_and(|until| now <= until) && inner.known.contains(account);
        let announced = inner
            .announced
            .get(account)
            .is_some_and(|until| now < *until);
        if known || announced {
            DmVerdict::Allowed
        } else if inner.waiting {
            DmVerdict::Wait
        } else {
            DmVerdict::Unknown
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, DmAccountsInner> {
        self.0.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, DmAccountsInner> {
        self.0.write().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(feature = "directory")]
impl DmAccounts {
    /// Follow a directory read: every account it recognises as an active person's in one of
    /// `groups` ([`known_accounts`]), for as long as the read may be used.
    pub fn follow(
        &self,
        snapshot: &crate::directory::Snapshot,
        cfg: &crate::directory::DirectoryConfig,
        groups: &std::collections::BTreeSet<String>,
    ) {
        self.set_known(
            known_accounts(&snapshot.people, groups),
            snapshot.read_at + cfg.stale_after(),
        );
    }

    /// The accounts of the fresh link requests among `requests` ([`announced_accounts`]).
    pub fn announce(
        &self,
        requests: &[crate::directory::LinkRequest],
        house_server: &str,
        now: SystemTime,
    ) {
        self.set_announced(announced_accounts(requests, house_server, now));
    }
}

/// Every account the directory recognises as the person's ([`People::accounts`]: the house
/// account and the links, an account two people claim counts for nobody) of each active person
/// in one of `groups`: the groups whose rooms the bot manages (plenum-bot: its plenums' groups
/// and the groups these require), so only the people the house rooms are for may write to the
/// bot, not a guest or a former resident whose user is still active. Nobody's of deactivated
/// people; an empty `groups`: nobody's.
///
/// [`People::accounts`]: crate::directory::People::accounts
#[cfg(feature = "directory")]
pub fn known_accounts(
    people: &crate::directory::People,
    groups: &std::collections::BTreeSet<String>,
) -> HashSet<OwnedUserId> {
    people
        .active()
        .filter(|p| p.groups.iter().any(|g| groups.contains(g)))
        .flat_map(|p| people.accounts(p))
        .map(UserId::to_owned)
        .collect()
}

/// The account of each fresh link request ([`crate::directory::LinkRequest::is_fresh`]) with
/// how long it counts. An account on the house's server is left out: it needs no link (its
/// person is known through the login, and [`known_accounts`] has it).
#[cfg(feature = "directory")]
pub fn announced_accounts(
    requests: &[crate::directory::LinkRequest],
    house_server: &str,
    now: SystemTime,
) -> Vec<(OwnedUserId, SystemTime)> {
    requests
        .iter()
        .filter(|r| r.is_fresh(now) && r.account.server_name().as_str() != house_server)
        .map(|r| (r.account.clone(), r.until()))
        .collect()
}

/// Who may pull the bot into which rooms.
#[derive(Clone, Debug)]
pub struct InvitePolicy {
    pub inviters: UserAllowList,
    pub rooms: RoomAllowList,
    /// Rooms allowed at runtime, in addition to `rooms`.
    pub dynamic_rooms: DynamicRooms,
    /// Admins may always invite the bot, and their direct chats bypass the
    /// room allow-list (their room IDs cannot be known in advance). The bot's
    /// shared list ([`crate::admin::Admins`]): the configured admins, and those
    /// of the directory's admin groups while their read is fresh.
    pub admins: crate::admin::Admins,
    /// Members of these rooms may open a direct chat with the bot
    /// (`[security] dm_rooms`).
    pub dm_rooms: Arc<Vec<OwnedRoomId>>,
    /// Accounts that may open a direct chat with the bot for a while (from the directory).
    pub dm_accounts: DmAccounts,
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
        } else if !self.rooms.allows(room_id) && !self.dynamic_rooms.contains(room_id) {
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

/// Whether `user` is joined to one of `rooms` right now (asked of the
/// server).
pub async fn member_of_any(client: &Client, rooms: &[OwnedRoomId], user: &UserId) -> bool {
    for room in rooms {
        if matches!(
            crate::private::is_joined_now(client, room, user).await,
            Ok(true)
        ) {
            return true;
        }
    }
    false
}

/// Join a direct chat from `user`, then make sure it is really just the two of them,
/// encrypted; otherwise leave again.
async fn accept_private_dm(client: &Client, room: &Room, user: &UserId) {
    let room_id = room.room_id();
    if join_with_retry(client, room_id, Some(user)).await
        && !crate::private::is_private_dm(client, room_id, user).await
    {
        warn!(room_id = %room_id, inviter = %user, "Direct chat is not private; leaving");
        if let Err(error) = room.leave().await {
            warn!(room_id = %room_id, %error, "Failed to leave a non-private chat");
        }
    }
}

/// The steps for an invite, before anything is asked of the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvitePlan {
    /// The lists allow it ([`InvitePolicy::decide`]): join.
    Join,
    /// A direct chat from an account in [`DmAccounts`]: join, keep it only if private.
    PrivateDm,
    /// A direct chat the lists refuse: a private one if the inviter is in a `dm_rooms` room
    /// now (asked of the server), else `wait` (leave it pending) or reject.
    AskDmRooms {
        wait: bool,
    },
    /// A direct chat held until the directory's first read.
    Wait,
    Reject(InviteDecision),
}

impl InvitePolicy {
    pub fn plan(
        &self,
        inviter: Option<&UserId>,
        room_id: &RoomId,
        is_direct: bool,
        now: SystemTime,
    ) -> InvitePlan {
        let decision = self.decide(inviter, room_id, is_direct);
        if decision == InviteDecision::Accept {
            return InvitePlan::Join;
        }
        let Some(user) = inviter.filter(|_| is_direct) else {
            return InvitePlan::Reject(decision);
        };
        let wait = match self.dm_accounts.verdict_at(user, now) {
            DmVerdict::Allowed => return InvitePlan::PrivateDm,
            DmVerdict::Wait => true,
            DmVerdict::Unknown => false,
        };
        match (self.dm_rooms.is_empty(), wait) {
            (false, wait) => InvitePlan::AskDmRooms { wait },
            (true, true) => InvitePlan::Wait,
            (true, false) => InvitePlan::Reject(decision),
        }
    }
}

async fn handle_invite(
    client: Client,
    room: Room,
    policy: &InvitePolicy,
    inviter: Option<OwnedUserId>,
    is_direct: bool,
) {
    let room_id = room.room_id().to_owned();
    let plan = policy.plan(inviter.as_deref(), &room_id, is_direct, SystemTime::now());
    let rejected = match (plan, inviter.as_deref()) {
        (InvitePlan::Join, _) => {
            info!(room_id = %room_id, inviter = ?inviter, is_direct, "Accepting invite");
            join_with_retry(&client, &room_id, inviter.as_deref()).await;
            return;
        }
        (InvitePlan::PrivateDm, Some(user)) => {
            info!(room_id = %room_id, inviter = %user, "Accepting a direct chat from a known account");
            accept_private_dm(&client, &room, user).await;
            return;
        }
        (InvitePlan::AskDmRooms { wait }, Some(user)) => {
            if member_of_any(&client, &policy.dm_rooms, user).await {
                info!(room_id = %room_id, inviter = %user, "Accepting a direct chat from a member");
                accept_private_dm(&client, &room, user).await;
                return;
            }
            if wait {
                None
            } else {
                Some(policy.decide(Some(user), &room_id, is_direct))
            }
        }
        (InvitePlan::Wait, _) => None,
        (InvitePlan::Reject(decision), _) => Some(decision),
        // not planned without an inviter
        (InvitePlan::PrivateDm | InvitePlan::AskDmRooms { .. }, None) => {
            Some(InviteDecision::RejectInviter)
        }
    };
    let Some(decision) = rejected else {
        info!(room_id = %room_id, inviter = ?inviter,
            "A direct chat left pending until the directory's first read");
        return;
    };
    warn!(room_id = %room_id, inviter = ?inviter, ?decision, "Rejecting invite");
    if let Err(error) = room.leave().await {
        warn!(room_id = %room_id, %error, "Failed to reject invite");
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
/// by `rooms`, never a direct chat (members' private chats included), and
/// admin direct chats only with `include_admin_dms`.
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
        .filter(|room| {
            let direct = room.direct_targets_length() == 1 && room.active_members_count() <= 2;
            !direct || (include_admin_dms && admin_dm_partner(room, admins).is_some())
        })
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
            dynamic_rooms: DynamicRooms::default(),
            admins: HashSet::from([user_id!("@admin:example.org").to_owned()]).into(),
            dm_rooms: Arc::default(),
            dm_accounts: DmAccounts::default(),
        }
    }

    #[test]
    fn rooms_allowed_at_runtime_join_the_static_list() {
        let p = policy(
            UserAllowList::Only(HashSet::new()),
            RoomAllowList::Only(HashSet::new()),
        );
        let admin = Some(user_id!("@admin:example.org"));
        let room = room_id!("!house:example.org");
        assert_eq!(p.decide(admin, room, false), InviteDecision::RejectRoom);
        p.dynamic_rooms.insert(room.to_owned());
        assert_eq!(p.decide(admin, room, false), InviteDecision::Accept);
        // Still only for allowed inviters.
        assert_eq!(
            p.decide(Some(user_id!("@eve:example.org")), room, false),
            InviteDecision::RejectInviter
        );
        p.dynamic_rooms.remove(room);
        assert_eq!(p.decide(admin, room, false), InviteDecision::RejectRoom);
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
    fn an_unset_list_allows_nobody() {
        let room = room_id!("!room:example.org");
        let p = policy(UserAllowList::Unset, RoomAllowList::Unset);
        assert_eq!(
            p.decide(Some(user_id!("@alice:example.org")), room, false),
            InviteDecision::RejectInviter
        );
        assert_eq!(p.decide(None, room, false), InviteDecision::RejectInviter);
        // Admins: their direct chats still work, rooms need the list.
        let admin = Some(user_id!("@admin:example.org"));
        assert_eq!(p.decide(admin, room, true), InviteDecision::Accept);
        assert_eq!(p.decide(admin, room, false), InviteDecision::RejectRoom);
    }

    #[test]
    fn admins_from_the_directory_groups_may_open_a_direct_chat_while_fresh() {
        use std::time::{Duration, SystemTime};
        let p = policy(
            UserAllowList::Only(HashSet::new()),
            RoomAllowList::Only(HashSet::new()),
        );
        let derived = Some(user_id!("@anna:example.org"));
        let dm = room_id!("!dm:example.org");
        assert_eq!(p.decide(derived, dm, true), InviteDecision::RejectInviter);
        // The bot's admin list (shared with its console) follows the directory.
        let console_list = p.admins.clone();
        console_list.set_from_groups(
            HashSet::from([user_id!("@anna:example.org").to_owned()]),
            SystemTime::now() + Duration::from_secs(3600),
        );
        assert_eq!(p.decide(derived, dm, true), InviteDecision::Accept);
        assert_eq!(p.decide(derived, dm, false), InviteDecision::RejectRoom);
        // A read too old: back to the configured admins only.
        console_list.set_from_groups(
            HashSet::from([user_id!("@anna:example.org").to_owned()]),
            SystemTime::now() - Duration::from_secs(1),
        );
        assert_eq!(p.decide(derived, dm, true), InviteDecision::RejectInviter);
        assert_eq!(
            p.decide(Some(user_id!("@admin:example.org")), dm, true),
            InviteDecision::Accept,
            "the configured admins stay"
        );
    }

    #[test]
    fn known_and_announced_accounts_may_open_a_direct_chat_until_they_expire() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let dm = DmAccounts::default();
        let anna = user_id!("@anna:matrix.org");
        let ben = user_id!("@ben:example.org");
        assert_eq!(dm.verdict_at(anna, at), DmVerdict::Unknown);
        dm.set_known(
            HashSet::from([anna.to_owned()]),
            at + Duration::from_secs(3600),
        );
        assert_eq!(dm.verdict_at(anna, at), DmVerdict::Allowed);
        assert_eq!(
            dm.verdict_at(anna, at + Duration::from_secs(3600)),
            DmVerdict::Allowed
        );
        // the read is too old: no longer
        assert_eq!(
            dm.verdict_at(anna, at + Duration::from_secs(3601)),
            DmVerdict::Unknown
        );
        // announced (a link request): for its own time, the known part apart
        dm.set_announced([
            (ben.to_owned(), at + Duration::from_secs(600)),
            (ben.to_owned(), at + Duration::from_secs(1800)),
        ]);
        assert_eq!(dm.verdict_at(ben, at), DmVerdict::Allowed);
        assert_eq!(
            dm.verdict_at(ben, at + Duration::from_secs(1799)),
            DmVerdict::Allowed,
            "the later of two times counts"
        );
        assert_eq!(
            dm.verdict_at(ben, at + Duration::from_secs(1800)),
            DmVerdict::Unknown
        );
        // a later call replaces them
        dm.set_announced([]);
        assert_eq!(dm.verdict_at(ben, at), DmVerdict::Unknown);
        dm.set_known(HashSet::new(), at + Duration::from_secs(3600));
        assert_eq!(dm.verdict_at(anna, at), DmVerdict::Unknown);
    }

    #[test]
    fn direct_chats_wait_for_the_first_read_of_the_directory() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let dm = DmAccounts::default();
        let anna = user_id!("@anna:matrix.org");
        assert!(!dm.is_waiting());
        dm.wait_for_first_read();
        // a clone (the invite handler's) sees the same
        let handler = dm.clone();
        assert_eq!(handler.verdict_at(anna, at), DmVerdict::Wait);
        // an announced account is in before the first read
        dm.set_announced([(anna.to_owned(), at + Duration::from_secs(60))]);
        assert_eq!(handler.verdict_at(anna, at), DmVerdict::Allowed);
        dm.set_announced([]);
        // the first read ends the wait: unknown is unknown from then on
        dm.set_known(HashSet::new(), at + Duration::from_secs(3600));
        assert!(!handler.is_waiting());
        assert_eq!(handler.verdict_at(anna, at), DmVerdict::Unknown);
    }

    #[test]
    fn the_policy_shares_the_direct_chat_accounts_and_its_own_decision_stays() {
        let p = policy(
            UserAllowList::Only(HashSet::new()),
            RoomAllowList::Only(HashSet::new()),
        );
        let anna = user_id!("@anna:matrix.org");
        let dm = room_id!("!dm:matrix.org");
        // `decide` covers the static lists only; the direct-chat accounts are asked after it
        assert_eq!(
            p.decide(Some(anna), dm, true),
            InviteDecision::RejectInviter
        );
        assert_eq!(p.dm_accounts.verdict(anna), DmVerdict::Unknown);
        let filled_by_the_bot = p.dm_accounts.clone();
        filled_by_the_bot.set_announced([(
            anna.to_owned(),
            SystemTime::now() + Duration::from_secs(1800),
        )]);
        assert_eq!(p.dm_accounts.verdict(anna), DmVerdict::Allowed);
        assert_eq!(
            p.dm_accounts.verdict(user_id!("@eve:matrix.org")),
            DmVerdict::Unknown
        );
        // a room (not direct) from the same account is still the lists' business
        assert_eq!(
            p.decide(Some(anna), dm, false),
            InviteDecision::RejectInviter
        );
    }

    #[test]
    fn the_plan_for_a_direct_chat_from_an_announced_account_and_from_an_unknown_one() {
        let now = SystemTime::now();
        let mut p = policy(
            UserAllowList::Only(HashSet::new()),
            RoomAllowList::Only(HashSet::new()),
        );
        let anna = Some(user_id!("@anna:matrix.org"));
        let eve = Some(user_id!("@eve:matrix.org"));
        let dm = room_id!("!dm:matrix.org");
        p.dm_accounts.set_announced([(
            user_id!("@anna:matrix.org").to_owned(),
            now + Duration::from_secs(1800),
        )]);
        assert_eq!(p.plan(anna, dm, true, now), InvitePlan::PrivateDm);
        assert_eq!(
            p.plan(eve, dm, true, now),
            InvitePlan::Reject(InviteDecision::RejectInviter)
        );
        // not a direct chat: the lists only
        assert_eq!(
            p.plan(anna, dm, false, now),
            InvitePlan::Reject(InviteDecision::RejectInviter)
        );
        // expired
        assert_eq!(
            p.plan(anna, dm, true, now + Duration::from_secs(1800)),
            InvitePlan::Reject(InviteDecision::RejectInviter)
        );
        // admins as before
        assert_eq!(
            p.plan(Some(user_id!("@admin:example.org")), dm, true, now),
            InvitePlan::Join
        );
        // with dm_rooms: the server is asked about the unknown one, not about the announced one
        p.dm_rooms = Arc::new(vec![room_id!("!house:example.org").to_owned()]);
        assert_eq!(p.plan(anna, dm, true, now), InvitePlan::PrivateDm);
        assert_eq!(
            p.plan(eve, dm, true, now),
            InvitePlan::AskDmRooms { wait: false }
        );
        // before the directory's first read: held, not rejected
        p.dm_accounts.wait_for_first_read();
        assert_eq!(
            p.plan(eve, dm, true, now),
            InvitePlan::AskDmRooms { wait: true }
        );
        p.dm_rooms = Arc::default();
        assert_eq!(p.plan(eve, dm, true, now), InvitePlan::Wait);
        assert_eq!(
            p.plan(eve, dm, false, now),
            InvitePlan::Reject(InviteDecision::RejectInviter),
            "a room is never held"
        );
        assert_eq!(
            p.plan(None, dm, true, now),
            InvitePlan::Reject(InviteDecision::RejectInviter)
        );
    }

    #[cfg(feature = "directory")]
    #[test]
    fn known_accounts_are_active_peoples_of_the_groups_without_conflicts() {
        use crate::directory::{Choice, Link, People, Person};
        let person = |id: &str, active: bool, links: &[&str], groups: &[&str]| {
            let house = OwnedUserId::try_from(format!("@{id}:house.org")).unwrap();
            let links: Vec<Link> = links
                .iter()
                .map(|l| Link {
                    id: OwnedUserId::try_from(*l).unwrap(),
                    by_admin: false,
                    verified: true,
                })
                .collect();
            Person {
                id: id.into(),
                username: id.into(),
                name: String::new(),
                active,
                accounts: std::iter::once(house.clone())
                    .chain(links.iter().map(|l| l.id.clone()))
                    .collect(),
                groups: groups.iter().map(|g| (*g).to_owned()).collect(),
                house: Some(house),
                links,
                choice: Choice::None,
            }
        };
        let people = People::new(vec![
            person(
                "anna",
                true,
                &["@anna:matrix.org", "@shared:matrix.org"],
                &["residents"],
            ),
            person("ben", true, &["@shared:matrix.org"], &["kitchen-floor-1"]),
            person("gone", false, &["@gone:matrix.org"], &[]),
            // active, but the house rooms are not for them
            person("guest", true, &["@guest:matrix.org"], &["guests"]),
            person("former", true, &[], &["former-residents"]),
        ]);
        let managed = ["residents".to_owned(), "kitchen-floor-1".to_owned()].into();
        let mut known: Vec<String> = known_accounts(&people, &managed)
            .into_iter()
            .map(|a| a.to_string())
            .collect();
        known.sort();
        assert_eq!(
            known,
            ["@anna:house.org", "@anna:matrix.org", "@ben:house.org"]
        );
        assert!(known_accounts(&people, &Default::default()).is_empty());
        // the shared set follows the same rule
        let dm = DmAccounts::default();
        let snapshot = crate::directory::Snapshot {
            people,
            groups: managed.clone(),
            read_at: SystemTime::now(),
        };
        let cfg: crate::directory::DirectoryConfig = toml::from_str(
            "provider = \"authentik\"\napi_url = \"https://login.example.org/api/v3\"",
        )
        .unwrap();
        dm.follow(&snapshot, &cfg, &managed);
        assert_eq!(dm.verdict(user_id!("@ben:house.org")), DmVerdict::Allowed);
        assert_eq!(
            dm.verdict(user_id!("@guest:matrix.org")),
            DmVerdict::Unknown
        );
        assert_eq!(
            dm.verdict(user_id!("@former:house.org")),
            DmVerdict::Unknown
        );
    }

    #[cfg(feature = "directory")]
    #[test]
    fn announced_accounts_are_fresh_requests_off_the_house_server() {
        use crate::directory::LinkRequest;
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let request = |account: &str, ago: i64| LinkRequest {
            person: account.into(),
            username: "x".into(),
            account: OwnedUserId::try_from(account).unwrap(),
            at: chrono::DateTime::from_timestamp(1_800_000_000 - ago, 0).unwrap(),
            verified: Vec::new(),
        };
        let requests = [
            request("@anna:matrix.org", 60),
            request("@old:matrix.org", 1800),
            request("@ahead:matrix.org", -301),
            request("@house:house.org", 0),
        ];
        let announced = announced_accounts(&requests, "house.org", now);
        assert_eq!(
            announced,
            [(
                user_id!("@anna:matrix.org").to_owned(),
                now + Duration::from_secs(1740)
            )]
        );
        let dm = DmAccounts::default();
        dm.announce(&requests, "house.org", now);
        assert_eq!(
            dm.verdict_at(user_id!("@anna:matrix.org"), now),
            DmVerdict::Allowed
        );
        assert_eq!(
            dm.verdict_at(user_id!("@old:matrix.org"), now),
            DmVerdict::Unknown
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
