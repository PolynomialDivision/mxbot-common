//! Rooms whose members follow the directory: the planning of one round, the guards around it
//! and what a bot remembers between rounds. Pure: the bot reads the rooms and the directory
//! ([`crate::directory::Directory::snapshot`]) and carries the plan out (plenum-bot; house-infra
//! docs/HOUSE-DIRECTORY.md, "The room reconciler").
//!
//! From one [`Snapshot`], for each managed room ([`plan`]):
//! * **wanted**: the active people in one of the room's groups who are in its required group
//!   too ([`Rule`]);
//! * **invite**: the chosen account ([`Contact::Chosen`]) of each wanted person, unless it is in
//!   the room (joined or invited), the bot invited it before (who leaves on their own is not
//!   invited again while the account stays theirs), or its invitation failed less than a day
//!   ago. Nobody is invited who has not chosen: they are [`Plan::unreachable`], and the bot
//!   asks them to choose ([`Notices`]);
//! * **tolerate**: every recognised account of a wanted person (an older chosen account, the
//!   house account that joined on its own) is never removed;
//! * **remove**: who is in the room and not tolerated, unless it is the bot, a kept account or
//!   a room admin (power level 100);
//! * **frozen**: an account two active people claim, one of them wanted in the room, is
//!   neither invited nor removed. When none of its claimants is wanted there it goes like any
//!   account of someone not in the room's groups (a former resident does not stay through a
//!   link someone else claims too). The admins hear of every conflict once
//!   ([`Notices::untold_conflicts`]).
//!
//! Guards: a stale snapshot or a house group with nobody in it changes nothing ([`halt`]; a
//! failed read gives no snapshot at all); a room whose group the directory does not know is
//! left alone ([`Skip`]); more removals in one room than [`removal_limit`] are held until an
//! admin confirms that very plan once, by its [`Hold::digest`], or until it shrinks
//! ([`gate`], [`Remembered::confirm`]). Removing is a kick: the bot never bans, deactivates
//! accounts or changes power levels.
//!
//! Notices go out with transaction IDs from a key ([`crate::send::txn_for`]); the homeserver
//! remembers those for good, so every key names its episode ([`Hold::episode`], the day a
//! condition was found), never only who it is about.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use chrono::{DateTime, Duration, Utc};
use matrix_sdk::ruma::{OwnedUserId, UserId};
use serde::{Deserialize, Serialize};

use crate::directory::{Contact, People, Person, Snapshot};

/// A short fingerprint of a set of strings (accounts, departures): the first 8 hex digits of
/// the SHA-256 over them, sorted and deduplicated, one per line. What an admin's confirmation
/// names, so it goes only for the plan they saw.
pub fn digest<I, S>(items: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    use sha2::{Digest, Sha256};
    let sorted: BTreeSet<String> = items.into_iter().map(|s| s.as_ref().to_owned()).collect();
    let mut hasher = Sha256::new();
    for item in &sorted {
        hasher.update(item.as_bytes());
        hasher.update(b"\n");
    }
    hasher.finalize()[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// How long a failed invitation (an account that does not exist yet) waits.
pub const RETRY_FAILED: Duration = Duration::hours(24);

/// How long a person who has not chosen an account waits for the next request to choose.
pub const ASK_EVERY: Duration = Duration::days(14);

/// How often a person is asked to choose at most.
pub const ASK_AT_MOST: u32 = 3;

/// Someone in a room, joined or invited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub id: OwnedUserId,
    /// Power level 100 or more: a room admin, never removed.
    pub admin: bool,
}

/// Who a room is for: the members of `group` who are in `require` too (when set). A room with
/// several rules is for everyone one of them names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub group: String,
    pub require: Option<String>,
}

/// What the bot remembers per room.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct Remembered {
    /// Accounts the bot invited (or found in the room): not invited again after they leave
    /// on their own.
    #[serde(default)]
    pub invited: BTreeSet<String>,
    /// Invitations that failed, tried again after [`RETRY_FAILED`].
    #[serde(default)]
    pub failed: BTreeMap<String, DateTime<Utc>>,
    /// Removals held for being too many at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<Hold>,
}

/// A room's removals held by [`gate`].
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct Hold {
    /// The accounts the held plan removes.
    pub accounts: BTreeSet<String>,
    /// The admins were told about this plan.
    #[serde(default)]
    pub told: bool,
    /// An admin confirmed this plan: the next round carries it out once, if it is the same.
    #[serde(default)]
    pub confirmed: bool,
    /// When this plan was first held (none in a state file from before it was kept).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
}

impl Hold {
    /// The plan's fingerprint ([`digest`] of its accounts): `!members confirm <room> <digest>`.
    pub fn digest(&self) -> String {
        digest(&self.accounts)
    }

    /// This hold for a notice's key: its digest and when it began, so the same plan held again
    /// later is a new message, not one the homeserver takes for a resend.
    pub fn episode(&self) -> String {
        let since = self.since.map(|t| t.to_rfc3339()).unwrap_or_default();
        format!("{}:{since}", self.digest())
    }
}

/// The answer to an admin's confirmation ([`Remembered::confirm`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Confirm {
    /// Nothing is held.
    Nothing,
    /// The held plan is the one they named: it goes once.
    Confirmed,
    /// Another plan is held (it changed, or a typo): nothing is confirmed; this one's digest and
    /// accounts, to look at and confirm.
    Other {
        digest: String,
        accounts: Vec<String>,
    },
}

impl Remembered {
    /// Before a round: forget the accounts no wanted person has any more, so that who comes
    /// back is invited again.
    pub fn forget_others(&mut self, tolerated: &BTreeSet<OwnedUserId>) {
        let keep = |id: &String| tolerated.iter().any(|t| t.as_str() == id);
        self.invited.retain(keep);
        self.failed.retain(|id, _| keep(id));
    }

    pub fn record_invited(&mut self, account: &UserId) {
        self.failed.remove(account.as_str());
        self.invited.insert(account.to_string());
    }

    pub fn record_failed(&mut self, account: &UserId, now: DateTime<Utc>) {
        self.failed.insert(account.to_string(), now);
    }

    /// After a round: a wanted person's account in the room counts as invited (leaving later
    /// is their choice).
    pub fn record_present(&mut self, current: &[Member], tolerated: &BTreeSet<OwnedUserId>) {
        for m in current.iter().filter(|m| tolerated.contains(&m.id)) {
            self.invited.insert(m.id.to_string());
        }
    }

    /// An admin's go for the held removals whose [`Hold::digest`] is `digest` (as shown in the
    /// notice and in `!members`): any other plan is not confirmed.
    pub fn confirm(&mut self, digest: &str) -> Confirm {
        match &mut self.hold {
            None => Confirm::Nothing,
            Some(hold) if hold.digest().eq_ignore_ascii_case(digest.trim()) => {
                hold.confirmed = true;
                Confirm::Confirmed
            }
            Some(hold) => Confirm::Other {
                digest: hold.digest(),
                accounts: hold.accounts.iter().cloned().collect(),
            },
        }
    }
}

/// Why a room is left as it is this round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Skip {
    /// The directory does not know one of its groups (a typo, a renamed group, a broken read).
    UnknownGroup(String),
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Skip::UnknownGroup(g) => write!(f, "the directory does not know the group {g}"),
        }
    }
}

/// An account to remove.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removal {
    pub account: OwnedUserId,
    /// The id of the deactivated person whose account it is, when nobody active owns it.
    pub deactivated: Option<String>,
}

/// One room's round.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub invite: Vec<OwnedUserId>,
    /// Before [`gate`]: every removal the room needs.
    pub remove: Vec<Removal>,
    /// In the room and in conflict with a wanted person among its claimants: neither invited
    /// nor removed.
    pub frozen: Vec<OwnedUserId>,
    /// Ids of the wanted people without a usable chosen account, sorted.
    pub unreachable: Vec<String>,
    /// Every recognised account of a wanted person.
    pub tolerated: BTreeSet<OwnedUserId>,
    /// Who is in the room but the bot and the kept accounts: what [`removal_limit`] is a
    /// share of.
    pub humans: usize,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.invite.is_empty() && self.remove.is_empty()
    }
}

/// Whether the directory knows `group`: it exists, or someone is in it (the group list may
/// have been read a moment before that person was added).
fn known(snapshot: &Snapshot, group: &str) -> bool {
    snapshot.has_group(group) || snapshot.people.members(group).next().is_some()
}

/// The active people a room with `rules` is for, by id.
pub fn wanted<'a>(people: &'a People, rules: &'a [Rule]) -> BTreeMap<&'a str, &'a Person> {
    rules
        .iter()
        .flat_map(|rule| {
            people
                .members(&rule.group)
                .filter(move |p| rule.require.as_deref().is_none_or(|r| p.groups.contains(r)))
        })
        .map(|p| (p.id.as_str(), p))
        .collect()
}

/// The round of one room with `rules`: `current` is who is in it now (joined or invited),
/// `keep` the accounts never removed, `me` the bot.
pub fn plan(
    snapshot: &Snapshot,
    rules: &[Rule],
    current: &[Member],
    keep: &HashSet<OwnedUserId>,
    me: &UserId,
    remembered: &Remembered,
    now: DateTime<Utc>,
) -> Result<Plan, Skip> {
    if let Some(missing) = rules
        .iter()
        .flat_map(|r| std::iter::once(&r.group).chain(r.require.as_ref()))
        .find(|g| !known(snapshot, g))
    {
        return Err(Skip::UnknownGroup(missing.clone()));
    }
    let people = &snapshot.people;
    let wanted = wanted(people, rules);
    let tolerated: BTreeSet<OwnedUserId> = wanted
        .values()
        .flat_map(|p| people.accounts(p))
        .map(UserId::to_owned)
        .collect();
    let here: HashSet<&UserId> = current.iter().map(|m| m.id.as_ref()).collect();
    let mut invite = BTreeSet::new();
    let mut unreachable = Vec::new();
    for person in wanted.values() {
        match people.contact(person) {
            Contact::Chosen(account) => {
                let due = account != me
                    && !here.contains(&*account)
                    && !remembered.invited.contains(account.as_str())
                    && remembered
                        .failed
                        .get(account.as_str())
                        .is_none_or(|at| now - *at >= RETRY_FAILED);
                if due {
                    invite.insert(account);
                }
            }
            Contact::Unchosen | Contact::Invalid(_) => unreachable.push(person.id.clone()),
        }
    }
    let ours = |m: &&Member| m.id != me && !keep.contains(&m.id);
    let mut frozen = Vec::new();
    let mut remove = Vec::new();
    for m in current.iter().filter(ours) {
        // claimed by two or more active people: frozen while one of them is wanted here
        let claimants = || people.active().filter(|p| p.accounts.contains(&m.id));
        if people.is_conflict(&m.id) && claimants().any(|p| wanted.contains_key(p.id.as_str())) {
            frozen.push(m.id.clone());
        } else if !m.admin && !tolerated.contains(&m.id) {
            let deactivated = claimants()
                .next()
                .is_none()
                .then(|| {
                    people
                        .all()
                        .iter()
                        .find(|p| !p.active && p.accounts.contains(&m.id))
                        .map(|p| p.id.clone())
                })
                .flatten();
            remove.push(Removal {
                account: m.id.clone(),
                deactivated,
            });
        }
    }
    remove.sort_by(|a, b| a.account.cmp(&b.account));
    frozen.sort();
    Ok(Plan {
        invite: invite.into_iter().collect(),
        remove,
        frozen,
        unreachable,
        tolerated,
        humans: current.iter().filter(ours).count(),
    })
}

/// Why no room changes this round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Halt {
    /// The last good read is older than the directory's `stale_after`.
    Stale { minutes: u64 },
    /// The house's own group exists with nobody in it: a broken directory, not an empty house.
    EmptyHouseGroup(String),
}

impl std::fmt::Display for Halt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Halt::Stale { minutes } => {
                write!(f, "the directory's last good read is {minutes} min old")
            }
            Halt::EmptyHouseGroup(g) => write!(f, "the house's group {g} has nobody in it"),
        }
    }
}

/// Whether the whole round stops: the snapshot is older than `stale_after`
/// ([`crate::directory::DirectoryConfig::stale_after`]), or one of `house_groups` (who lives
/// here) exists with nobody in it. A house group the directory does not know stops nothing
/// here: the rooms that need it are skipped one by one ([`Skip`]).
pub fn halt<'a>(
    snapshot: &Snapshot,
    house_groups: impl IntoIterator<Item = &'a str>,
    stale_after: std::time::Duration,
    now: std::time::SystemTime,
) -> Option<Halt> {
    let age = snapshot.age(now);
    if age > stale_after {
        return Some(Halt::Stale {
            minutes: age.as_secs() / 60,
        });
    }
    house_groups
        .into_iter()
        .find(|g| snapshot.has_group(g) && snapshot.people.members(g).next().is_none())
        .map(|g| Halt::EmptyHouseGroup(g.to_owned()))
}

/// The most removals one room takes in one round: 20 % of its people ([`Plan::humans`]),
/// at least 2.
pub fn removal_limit(humans: usize) -> usize {
    (humans / 5).max(2)
}

/// What becomes of a room's removals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    /// Carry them out (few enough, or the same plan an admin confirmed).
    Go,
    /// Held: nobody is removed this round (invitations still go out). `tell`: the admins have
    /// not heard of this plan yet; the bot tells them and then sets [`Hold::told`].
    Held { limit: usize, tell: bool },
}

/// The mass-removal guard. More removals than [`removal_limit`] are held; the hold is
/// remembered with its accounts and since when (`now`), so the admins hear of each distinct
/// plan once. An admin's [`Remembered::confirm`] lets the same plan go once; a changed plan is
/// a new hold, and a plan within the limit ends the hold.
pub fn gate(
    remembered: &mut Remembered,
    removals: &[Removal],
    humans: usize,
    now: DateTime<Utc>,
) -> Gate {
    let limit = removal_limit(humans);
    if removals.len() <= limit {
        remembered.hold = None;
        return Gate::Go;
    }
    let accounts: BTreeSet<String> = removals.iter().map(|r| r.account.to_string()).collect();
    match &mut remembered.hold {
        Some(hold) if hold.accounts == accounts && hold.confirmed => {
            remembered.hold = None;
            Gate::Go
        }
        Some(hold) if hold.accounts == accounts => {
            hold.since.get_or_insert(now);
            Gate::Held {
                limit,
                tell: !hold.told,
            }
        }
        _ => {
            remembered.hold = Some(Hold {
                accounts,
                told: false,
                confirmed: false,
                since: Some(now),
            });
            Gate::Held { limit, tell: true }
        }
    }
}

/// Where a mention or a message to `person` goes: their chosen account, else their house
/// account (the first account no one else claims). A mention is no invitation: it may name
/// an account that has not chosen to be the contact.
pub fn address(people: &People, person: &Person) -> Option<OwnedUserId> {
    match people.contact(person) {
        Contact::Chosen(id) => Some(id),
        _ => people.accounts(person).next().map(UserId::to_owned),
    }
}

/// What the bot told whom, per person id (the directory's `uuid`, which never changes).
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct Notices {
    /// Asked to choose their account.
    #[serde(default)]
    pub asked: BTreeMap<String, Asked>,
    /// The admins were told that the person's choice cannot be used.
    #[serde(default)]
    pub invalid: BTreeSet<String>,
    /// The admins were told that the person is deactivated with a Matrix account still in use.
    #[serde(default)]
    pub deactivated: BTreeSet<String>,
    /// Accounts two or more active people claim, as the admins were told ("account names"):
    /// each told once while it lasts ([`Notices::untold_conflicts`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub conflicts: BTreeSet<String>,
    /// Who could not be reached, one line each, as the admins last heard of it
    /// ([`Notices::unreachable_is_news`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub unreachable: BTreeSet<String>,
}

/// A conflict as [`Notices::conflicts`] keeps it: the account and who claims it.
fn conflict_entry(account: &UserId, names: &[&str]) -> String {
    format!("{account} {}", names.join(","))
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Asked {
    pub times: u32,
    pub last: DateTime<Utc>,
}

impl Notices {
    /// At most [`ASK_AT_MOST`] times, [`ASK_EVERY`] apart.
    pub fn may_ask(&self, person: &str, now: DateTime<Utc>) -> bool {
        self.asked
            .get(person)
            .is_none_or(|a| a.times < ASK_AT_MOST && now - a.last >= ASK_EVERY)
    }

    pub fn record_asked(&mut self, person: &str, now: DateTime<Utc>) {
        let asked = self.asked.entry(person.to_owned()).or_insert(Asked {
            times: 0,
            last: now,
        });
        asked.times += 1;
        asked.last = now;
    }

    /// The conflicts the admins have not heard of yet (account, the claimants' usernames).
    pub fn untold_conflicts<'a>(&self, people: &'a People) -> Vec<(OwnedUserId, Vec<&'a str>)> {
        people
            .conflicts()
            .into_iter()
            .filter(|(account, names)| !self.conflicts.contains(&conflict_entry(account, names)))
            .collect()
    }

    pub fn record_conflict(&mut self, account: &UserId, names: &[&str]) {
        self.conflicts.insert(conflict_entry(account, names));
    }

    /// Whether the admins should hear of `current` (who cannot be reached, one line each): it
    /// has a line they have not heard of. A list that only shrank is no news; the bot records
    /// it all the same ([`Self::record_unreachable`]), so who comes back is news again.
    pub fn unreachable_is_news(&self, current: &BTreeSet<String>) -> bool {
        current.iter().any(|line| !self.unreachable.contains(line))
    }

    pub fn record_unreachable(&mut self, current: BTreeSet<String>) {
        self.unreachable = current;
    }

    /// Forget what no longer holds: a choice that became usable (a new bad one is told
    /// again), a deactivation undone (a new one is told again), a conflict resolved, and
    /// everyone the directory no longer has. How often someone was asked is kept while they
    /// are in the directory.
    pub fn tidy(&mut self, people: &People) {
        let invalid = |id: &String| {
            people
                .by_id(id)
                .is_some_and(|p| p.active && matches!(people.contact(p), Contact::Invalid(_)))
        };
        self.invalid.retain(invalid);
        self.deactivated
            .retain(|id| people.by_id(id).is_some_and(|p| !p.active));
        self.asked.retain(|id, _| people.by_id(id).is_some());
        let conflicts: BTreeSet<String> = people
            .conflicts()
            .iter()
            .map(|(account, names)| conflict_entry(account, names))
            .collect();
        self.conflicts.retain(|c| conflicts.contains(c));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::{Choice, Link};
    use std::time::{Duration as StdDuration, SystemTime};

    fn id(s: &str) -> OwnedUserId {
        UserId::parse(s).unwrap()
    }

    /// A person with a house account `@<name>:h`, `groups`, verified `links` and a choice.
    fn person(name: &str, groups: &[&str], links: &[&str], choice: Option<&str>) -> Person {
        let house = id(&format!("@{name}:h"));
        let links: Vec<Link> = links
            .iter()
            .map(|l| Link {
                id: id(l),
                by_admin: false,
                verified: true,
            })
            .collect();
        Person {
            id: format!("u-{name}"),
            username: name.into(),
            name: String::new(),
            active: true,
            accounts: std::iter::once(house.clone())
                .chain(links.iter().map(|l| l.id.clone()))
                .collect(),
            groups: groups.iter().map(|g| (*g).to_owned()).collect(),
            house: Some(house),
            links,
            choice: choice.map_or(Choice::None, |c| Choice::Account(id(c))),
        }
    }

    fn chosen(name: &str, groups: &[&str]) -> Person {
        person(name, groups, &[], Some(&format!("@{name}:h")))
    }

    const GROUPS: [&str; 5] = [
        "residents",
        "former-residents",
        "kitchen-floor-1",
        "kitchen-floor-3",
        "finance",
    ];

    fn at() -> SystemTime {
        SystemTime::UNIX_EPOCH + StdDuration::from_secs(1_800_000_000)
    }

    fn now() -> DateTime<Utc> {
        DateTime::<Utc>::from(at())
    }

    fn snap(people: Vec<Person>) -> Snapshot {
        Snapshot {
            people: People::new(people),
            groups: GROUPS.iter().map(|g| (*g).to_owned()).collect(),
            read_at: at(),
        }
    }

    fn rule(group: &str, require: Option<&str>) -> Rule {
        Rule {
            group: group.into(),
            require: require.map(str::to_owned),
        }
    }

    fn room(ids: &[&str]) -> Vec<Member> {
        ids.iter()
            .map(|s| Member {
                id: id(s),
                admin: false,
            })
            .collect()
    }

    fn me() -> OwnedUserId {
        id("@bot:h")
    }

    fn plan_of(s: &Snapshot, rules: &[Rule], current: &[Member], r: &Remembered) -> Plan {
        plan(s, rules, current, &HashSet::new(), &me(), r, now()).unwrap()
    }

    fn removed(p: &Plan) -> Vec<String> {
        p.remove.iter().map(|r| r.account.to_string()).collect()
    }

    fn ids(list: &[&str]) -> Vec<OwnedUserId> {
        list.iter().map(|s| id(s)).collect()
    }

    #[test]
    fn person_a_without_a_choice_is_not_invited_but_asked_and_kept() {
        let s = snap(vec![person("a", &["residents"], &[], None)]);
        let rules = [rule("residents", None)];
        let p = plan_of(&s, &rules, &[], &Remembered::default());
        assert!(p.invite.is_empty());
        assert_eq!(p.unreachable, ["u-a"]);
        // already in the room with the house account: stays
        let p = plan_of(&s, &rules, &room(&["@a:h"]), &Remembered::default());
        assert!(p.remove.is_empty() && p.invite.is_empty());
        // asked at most three times, two weeks apart
        let mut n = Notices::default();
        assert!(n.may_ask("u-a", now()));
        n.record_asked("u-a", now());
        assert!(!n.may_ask("u-a", now() + Duration::days(13)));
        n.record_asked("u-a", now() + Duration::days(14));
        n.record_asked("u-a", now() + Duration::days(28));
        assert!(!n.may_ask("u-a", now() + Duration::days(100)));
        n.tidy(&s.people);
        assert_eq!(n.asked["u-a"].times, 3);
    }

    #[test]
    fn person_b_gets_only_the_chosen_link_and_the_house_account_is_tolerated() {
        let b = person(
            "b",
            &["residents"],
            &["@b:matrix.org"],
            Some("@b:matrix.org"),
        );
        let s = snap(vec![b]);
        let rules = [rule("residents", None)];
        let p = plan_of(&s, &rules, &[], &Remembered::default());
        assert_eq!(p.invite, ids(&["@b:matrix.org"]));
        // the house account joined on its own: tolerated, and the link still invited
        let p = plan_of(&s, &rules, &room(&["@b:h"]), &Remembered::default());
        assert!(p.remove.is_empty());
        assert_eq!(p.invite, ids(&["@b:matrix.org"]));
    }

    #[test]
    fn person_c_with_several_accounts_gets_one_invitation_and_keeps_the_others() {
        let c = person(
            "c",
            &["residents"],
            &["@c:matrix.org", "@c:example.org"],
            Some("@c:example.org"),
        );
        let s = snap(vec![c]);
        let rules = [rule("residents", None)];
        let p = plan_of(
            &s,
            &rules,
            &room(&["@c:h", "@c:matrix.org"]),
            &Remembered::default(),
        );
        assert_eq!(p.invite, ids(&["@c:example.org"]));
        assert!(p.remove.is_empty());
        assert_eq!(p.tolerated.len(), 3);
    }

    #[test]
    fn person_d_moves_from_kitchen_1_to_kitchen_3() {
        let s = snap(vec![chosen("d", &["residents", "kitchen-floor-3"])]);
        let k1 = [rule("kitchen-floor-1", Some("residents"))];
        let k3 = [rule("kitchen-floor-3", Some("residents"))];
        let mut r1 = Remembered::default();
        r1.invited.insert("@d:h".into());
        let p = plan_of(&s, &k1, &room(&["@d:h", "@x:h", "@y:h"]), &r1);
        assert_eq!(removed(&p), ["@d:h", "@x:h", "@y:h"]);
        assert_eq!(p.remove[0].deactivated, None);
        // forgotten in kitchen 1: coming back invites again
        r1.forget_others(&p.tolerated);
        assert!(r1.invited.is_empty());
        let p = plan_of(&s, &k3, &[], &Remembered::default());
        assert_eq!(p.invite, ids(&["@d:h"]));
    }

    #[test]
    fn person_e_moving_out_leaves_the_rooms_that_need_residents_only() {
        let e = chosen("e", &["former-residents", "kitchen-floor-1"]);
        let s = snap(vec![e, chosen("f", &["residents", "kitchen-floor-1"])]);
        let here = room(&["@e:h", "@f:h"]);
        for rules in [
            vec![rule("residents", None)],
            vec![rule("kitchen-floor-1", Some("residents"))],
        ] {
            let p = plan_of(&s, &rules, &here, &Remembered::default());
            assert_eq!(removed(&p), ["@e:h"], "{rules:?}");
        }
        // a room for the kitchen without the residents rule keeps them
        let p = plan_of(
            &s,
            &[rule("kitchen-floor-1", None)],
            &here,
            &Remembered::default(),
        );
        assert!(p.remove.is_empty());
    }

    #[test]
    fn person_f_deactivated_is_removed_and_named() {
        let mut f = chosen("f", &["residents"]);
        f.active = false;
        f.groups.clear();
        let s = snap(vec![f, chosen("g", &["residents"])]);
        let p = plan_of(
            &s,
            &[rule("residents", None)],
            &room(&["@f:h", "@g:h"]),
            &Remembered::default(),
        );
        assert_eq!(
            p.remove,
            [Removal {
                account: id("@f:h"),
                deactivated: Some("u-f".into())
            }]
        );
        let mut n = Notices {
            deactivated: ["u-f".to_owned()].into(),
            ..Notices::default()
        };
        n.tidy(&s.people);
        assert!(n.deactivated.contains("u-f"));
        // active again: forgotten, a second deactivation is told again
        n.tidy(&snap(vec![chosen("f", &["residents"])]).people);
        assert!(n.deactivated.is_empty());
    }

    #[test]
    fn person_h_changes_the_chosen_account_without_loops() {
        let rules = [rule("residents", None)];
        let before = person("h", &["residents"], &["@h:matrix.org"], Some("@h:h"));
        let after = person(
            "h",
            &["residents"],
            &["@h:matrix.org"],
            Some("@h:matrix.org"),
        );
        let mut r = Remembered::default();
        let mut current = room(&["@h:h"]);
        let p = plan_of(&snap(vec![before]), &rules, &current, &r);
        assert!(p.is_empty());
        r.record_present(&current, &p.tolerated);
        let s = snap(vec![after]);
        let p = plan_of(&s, &rules, &current, &r);
        assert_eq!(p.invite, ids(&["@h:matrix.org"]));
        assert!(p.remove.is_empty());
        r.record_invited(&id("@h:matrix.org"));
        // they decline the invitation and leave with the old one: nobody is invited again
        current.clear();
        r.forget_others(&p.tolerated);
        assert!(plan_of(&s, &rules, &current, &r).is_empty());
    }

    #[test]
    fn an_account_two_people_claim_is_frozen() {
        let mut a = person("a", &["residents"], &["@shared:matrix.org"], None);
        a.choice = Choice::Account(id("@shared:matrix.org"));
        let b = person("b", &[], &["@shared:matrix.org"], None);
        let s = snap(vec![a, b]);
        let rules = [rule("residents", None)];
        let p = plan_of(
            &s,
            &rules,
            &room(&["@shared:matrix.org"]),
            &Remembered::default(),
        );
        assert!(p.remove.is_empty() && p.invite.is_empty());
        assert_eq!(p.frozen, ids(&["@shared:matrix.org"]));
        assert_eq!(p.unreachable, ["u-a"]);
        // not in the room: not invited either
        assert!(plan_of(&s, &rules, &[], &Remembered::default()).is_empty());
    }

    #[test]
    fn a_conflict_nobody_wanted_claims_is_removed_like_any_other() {
        // e moved out (former-residents); f still lives here but not in the finance group:
        // the link both claim keeps nobody in the finance room
        let e = person("e", &["former-residents"], &["@shared:matrix.org"], None);
        let f = person("f", &["residents"], &["@shared:matrix.org"], None);
        let g = chosen("g", &["residents", "finance"]);
        let s = snap(vec![e, f, g]);
        let here = room(&["@shared:matrix.org", "@g:h"]);
        let p = plan_of(&s, &[rule("finance", None)], &here, &Remembered::default());
        assert!(p.frozen.is_empty());
        assert_eq!(
            p.remove,
            [Removal {
                account: id("@shared:matrix.org"),
                deactivated: None
            }]
        );
        // the residents' room has f, one of its claimants: frozen there
        let p = plan_of(
            &s,
            &[rule("residents", None)],
            &here,
            &Remembered::default(),
        );
        assert_eq!(p.frozen, ids(&["@shared:matrix.org"]));
        assert!(p.remove.is_empty());
        // the admins hear of it once, again when the claimants change, not after it is gone
        let mut n = Notices::default();
        let untold = n.untold_conflicts(&s.people);
        assert_eq!(untold, [(id("@shared:matrix.org"), vec!["e", "f"])]);
        n.record_conflict(&untold[0].0, &untold[0].1);
        assert!(n.untold_conflicts(&s.people).is_empty());
        n.tidy(&s.people);
        assert_eq!(n.conflicts.len(), 1);
        n.tidy(&snap(vec![chosen("g", &["residents"])]).people);
        assert!(n.conflicts.is_empty());
    }

    #[test]
    fn the_unreachable_list_is_news_when_someone_is_added() {
        let mut n = Notices::default();
        let list = |lines: &[&str]| -> BTreeSet<String> {
            lines.iter().map(|l| (*l).to_owned()).collect()
        };
        assert!(!n.unreachable_is_news(&list(&[])));
        assert!(n.unreachable_is_news(&list(&["a", "b"])));
        n.record_unreachable(list(&["a", "b"]));
        assert!(!n.unreachable_is_news(&list(&["a", "b"])));
        // b chose: no news, but remembered, so b coming back is news again
        assert!(!n.unreachable_is_news(&list(&["a"])));
        n.record_unreachable(list(&["a"]));
        assert!(n.unreachable_is_news(&list(&["a", "b"])));
        assert!(n.unreachable_is_news(&list(&["a", "c"])));
    }

    #[test]
    fn the_digest_is_short_stable_and_order_free() {
        let d = digest(["@b:h", "@a:h"]);
        assert_eq!(d.len(), 8);
        assert!(d.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(d, digest(["@a:h", "@b:h", "@a:h"]));
        assert_ne!(d, digest(["@a:h"]));
    }

    #[test]
    fn the_bot_kept_accounts_and_room_admins_stay() {
        let s = snap(vec![chosen("a", &["residents"])]);
        let mut current = room(&["@bot:h", "@cal-bot:h", "@eve:h", "@a:h"]);
        current.push(Member {
            id: id("@nick:h"),
            admin: true,
        });
        let keep = HashSet::from([id("@cal-bot:h")]);
        let p = plan(
            &s,
            &[rule("residents", None)],
            &current,
            &keep,
            &me(),
            &Remembered::default(),
            now(),
        )
        .unwrap();
        assert_eq!(removed(&p), ["@eve:h"]);
        assert_eq!(p.humans, 3);
    }

    #[test]
    fn invitations_are_not_repeated() {
        let s = snap(vec![
            chosen("a", &["residents"]),
            chosen("b", &["residents"]),
        ]);
        let rules = [rule("residents", None)];
        let mut r = Remembered::default();
        r.invited.insert("@a:h".into());
        r.record_failed(&id("@b:h"), now() - Duration::hours(2));
        assert!(plan_of(&s, &rules, &[], &r).invite.is_empty());
        // a pending invitation is someone in the room
        assert!(
            plan_of(&s, &rules, &room(&["@b:h"]), &Remembered::default())
                .invite
                .iter()
                .all(|i| i.as_str() != "@b:h")
        );
        r.failed.insert("@b:h".into(), now() - Duration::hours(25));
        assert_eq!(plan_of(&s, &rules, &[], &r).invite, ids(&["@b:h"]));
        // the bot is never invited
        let s = snap(vec![chosen("bot", &["residents"])]);
        assert!(plan_of(&s, &rules, &[], &Remembered::default()).is_empty());
    }

    #[test]
    fn an_unknown_group_leaves_the_room_alone_an_empty_one_empties_it() {
        let s = snap(vec![chosen("a", &["residents"])]);
        let here = room(&["@a:h", "@b:h"]);
        let skip = plan(
            &s,
            &[rule("kitchen-floor-9", None)],
            &here,
            &HashSet::new(),
            &me(),
            &Remembered::default(),
            now(),
        );
        assert_eq!(skip, Err(Skip::UnknownGroup("kitchen-floor-9".into())));
        let skip = plan(
            &s,
            &[rule("finance", Some("gone"))],
            &here,
            &HashSet::new(),
            &me(),
            &Remembered::default(),
            now(),
        );
        assert_eq!(skip, Err(Skip::UnknownGroup("gone".into())));
        let p = plan_of(&s, &[rule("finance", None)], &here, &Remembered::default());
        assert_eq!(removed(&p), ["@a:h", "@b:h"]);
    }

    #[test]
    fn a_stale_snapshot_or_an_empty_house_group_halts_everything() {
        let s = snap(vec![chosen("a", &["kitchen-floor-1"])]);
        let hour = StdDuration::from_secs(3600);
        assert_eq!(
            halt(&s, ["residents"], hour, at()),
            Some(Halt::EmptyHouseGroup("residents".into()))
        );
        let s = snap(vec![chosen("a", &["residents"])]);
        assert_eq!(halt(&s, ["residents"], hour, at() + hour), None);
        assert_eq!(
            halt(&s, ["residents"], hour, at() + hour * 2),
            Some(Halt::Stale { minutes: 120 })
        );
        // a house group the directory does not know: the rooms are skipped one by one
        assert_eq!(halt(&s, ["nobody-knows"], hour, at()), None);
    }

    #[test]
    fn many_removals_are_held_until_confirmed_or_fewer() {
        let removals = |n: usize| -> Vec<Removal> {
            (0..n)
                .map(|i| Removal {
                    account: id(&format!("@p{i}:h")),
                    deactivated: None,
                })
                .collect()
        };
        assert_eq!(removal_limit(3), 2);
        assert_eq!(removal_limit(30), 6);
        let mut r = Remembered::default();
        let t = now();
        assert_eq!(gate(&mut r, &removals(2), 4, t), Gate::Go);
        assert_eq!(
            gate(&mut r, &removals(3), 4, t),
            Gate::Held {
                limit: 2,
                tell: true
            }
        );
        r.hold.as_mut().unwrap().told = true;
        let first = r.hold.as_ref().unwrap().episode();
        // the same plan: told once, the same episode
        assert_eq!(
            gate(&mut r, &removals(3), 4, t + Duration::hours(1)),
            Gate::Held {
                limit: 2,
                tell: false
            }
        );
        assert_eq!(r.hold.as_ref().unwrap().episode(), first);
        // another plan: a new hold, told again
        assert_eq!(
            gate(&mut r, &removals(4), 4, t),
            Gate::Held {
                limit: 2,
                tell: true
            }
        );
        r.hold.as_mut().unwrap().told = true;
        let four = r.hold.as_ref().unwrap().digest();
        // confirmed by its digest: goes once, then the hold is gone
        assert_eq!(r.confirm(&four.to_uppercase()), Confirm::Confirmed);
        assert_eq!(gate(&mut r, &removals(4), 4, t), Gate::Go);
        assert!(r.hold.is_none());
        assert_eq!(r.confirm(&four), Confirm::Nothing);
        // a confirmation is for that plan only
        gate(&mut r, &removals(3), 4, t);
        assert!(matches!(r.confirm(&four), Confirm::Other { .. }));
        assert!(!r.hold.as_ref().unwrap().confirmed);
        let three = r.hold.as_ref().unwrap().digest();
        assert_eq!(r.confirm(&three), Confirm::Confirmed);
        assert!(matches!(
            gate(&mut r, &removals(5), 4, t),
            Gate::Held { .. }
        ));
        assert!(!r.hold.as_ref().unwrap().confirmed);
        // the plan shrinks: the hold ends by itself
        assert_eq!(gate(&mut r, &removals(1), 4, t), Gate::Go);
        assert!(r.hold.is_none());
        // the same plan held again a day later: a new episode (a new notice key)
        gate(&mut r, &removals(3), 4, t + Duration::days(1));
        assert_ne!(r.hold.as_ref().unwrap().episode(), first);
        assert_eq!(r.hold.as_ref().unwrap().digest(), three);
    }

    #[test]
    fn a_confirmation_naming_another_plan_shows_the_held_one() {
        let mut r = Remembered::default();
        let removals: Vec<Removal> = ["@x:h", "@y:h", "@z:h"]
            .iter()
            .map(|a| Removal {
                account: id(a),
                deactivated: None,
            })
            .collect();
        gate(&mut r, &removals, 3, now());
        let held = r.hold.as_ref().unwrap().digest();
        assert_eq!(
            r.confirm("deadbeef"),
            Confirm::Other {
                digest: held,
                accounts: vec!["@x:h".into(), "@y:h".into(), "@z:h".into()],
            }
        );
        assert!(!r.hold.as_ref().unwrap().confirmed);
    }

    #[test]
    fn the_address_is_the_choice_else_the_house_account() {
        let s = snap(vec![
            person("a", &[], &[], None),
            person("b", &[], &["@b:matrix.org"], Some("@b:matrix.org")),
        ]);
        let of = |uuid: &str| address(&s.people, s.people.by_id(uuid).unwrap());
        assert_eq!(of("u-a"), Some(id("@a:h")));
        assert_eq!(of("u-b"), Some(id("@b:matrix.org")));
    }

    #[test]
    fn an_older_state_file_still_loads() {
        let r: Remembered = serde_json::from_str(
            r#"{"invited": ["@a:h"], "failed": {"@b:h": "2026-10-22T10:00:00Z"}}"#,
        )
        .unwrap();
        assert!(r.invited.contains("@a:h") && r.hold.is_none());
        assert_eq!(
            r.failed["@b:h"],
            "2026-10-22T10:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        // and an older bot reads the new one (no hold written while there is none)
        assert!(!serde_json::to_string(&r).unwrap().contains("hold"));
        let n: Notices = serde_json::from_str("{}").unwrap();
        assert_eq!(n, Notices::default());
        assert_eq!(
            serde_json::to_string(&n)
                .unwrap()
                .matches("conflicts")
                .count(),
            0
        );
        // a hold from before `since`: loads, its episode is its digest alone until held again
        let r: Remembered = serde_json::from_str(
            r#"{"hold": {"accounts": ["@a:h", "@b:h", "@c:h"], "told": true}}"#,
        )
        .unwrap();
        let hold = r.hold.unwrap();
        assert!(hold.since.is_none() && hold.told);
        assert_eq!(hold.episode(), format!("{}:", hold.digest()));
    }

    #[test]
    fn an_invalid_choice_is_told_until_it_is_fixed() {
        let mut d = person("d", &["residents"], &[], None);
        d.choice = Choice::Account(id("@d:matrix.org")); // not verified
        let mut n = Notices {
            invalid: ["u-d".to_owned(), "u-gone".to_owned()].into(),
            ..Notices::default()
        };
        n.tidy(&snap(vec![d.clone()]).people);
        assert_eq!(n.invalid, ["u-d".to_owned()].into());
        d.choice = Choice::Account(id("@d:h"));
        n.tidy(&snap(vec![d]).people);
        assert!(n.invalid.is_empty());
    }
}
