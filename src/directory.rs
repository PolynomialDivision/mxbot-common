//! The house's directory (its identity provider, `[directory]`): who lives here, their
//! Matrix accounts and their groups.
//!
//! A person is an Authentik user, keyed by its `uuid` (it never changes; the username may).
//! Their accounts:
//! * the house account, @<login>:<server> (or the localpart attribute): tuwunel makes it at
//!   the first login, so it is always theirs, but it may not exist yet;
//! * links an admin entered (the `matrix_ids` attribute): recognised as the person, never an
//!   address the bots pick by themselves;
//! * links the person proved (`matrix_verified`, written by the provider's flow
//!   `house-matrix-accounts` after a link code, [`crate::link`]).
//!
//! A link on the house's own server is refused (a house account is bound by the login, a link
//! cannot claim one), and so is an account two active people claim: it counts for nobody
//! ([`People::conflicts`]). Where the bots invite and write to someone is their chosen account
//! (`matrix_preferred`, [`People::contact`]): the house account or a proven link. Nobody has
//! one until they choose; the house account is never chosen for them. Rights (admins) never
//! follow a link: bots take them from the house account and their static lists only.
//!
//! Lists are given as a list of full Matrix IDs, or one string separated by commas or spaces.
//! Only active people are members of anything, never service accounts. The API token comes
//! from the environment (`token_env`, `DIRECTORY_TOKEN` by default), never from the config;
//! it needs to view users and groups.
//!
//! A read is all or nothing: an answer without anyone, without the users' groups or ids, or
//! one that changed between its pages is an error, and a bot changes nothing on an error.
//! [`Snapshot::is_stale`] tells a bot when its last good read is too old to act on.
//!
//! Link requests ([`LinkRequest`], `matrix_link_request`): the provider's flow writes one on a
//! logged-in person who named an account on another server without a code; the directory bot
//! then invites that account to a direct chat and sends the code there ([`crate::link`]).
//! [`Directory::link_requests`] reads only them, often and cheaply, and only of the people in
//! the groups a bot names (the ones whose rooms it manages: guests and former residents ask
//! for nothing); it is no part of a [`Snapshot`] and nothing about rooms or rights follows
//! from it.
//!
//! Errors: [`is_outage`] tells a provider that is down (unreachable, timing out, 5xx, 429)
//! from one that refuses the bot or answers what it cannot read; only the first is another
//! service's outage (`Alive::unavailable`), the rest a broken config or bot.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use matrix_sdk::ruma::{OwnedUserId, UserId};
use serde::Deserialize;
use tracing::warn;

/// The `[directory]` table.
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct DirectoryConfig {
    pub provider: DirectoryProvider,
    /// The REST API base, e.g. "https://login.example.org/api/v3".
    pub api_url: String,
    /// The house's homeserver, for the house accounts. Default: the bot's own server.
    #[serde(default)]
    pub server_name: Option<String>,
    /// User attribute that overrides the login as the house account's localpart.
    #[serde(default)]
    pub localpart_attribute: Option<String>,
    /// User attribute with the accounts an admin linked.
    #[serde(default = "default_matrix_ids_attribute")]
    pub matrix_ids_attribute: String,
    /// User attribute with the accounts the person proved.
    #[serde(default = "default_verified_attribute")]
    pub verified_attribute: String,
    /// User attribute with the person's chosen account.
    #[serde(default = "default_preferred_attribute")]
    pub preferred_attribute: String,
    /// User attribute with the person's open link request (`{"id": "@x:server", "at": <unix
    /// seconds>}`), written by the provider's flow, removed when the link succeeds.
    #[serde(default = "default_link_request_attribute")]
    pub link_request_attribute: String,
    /// Environment variable with the read-only API token.
    #[serde(default = "default_token_env")]
    pub token_env: String,
    /// How often a bot reads it again.
    #[serde(default = "default_refresh_minutes")]
    pub refresh_minutes: u64,
    /// How old the last good read may be before a bot stops acting on it (no invitations,
    /// no removals, no rights from groups).
    #[serde(default = "default_stale_after_minutes")]
    pub stale_after_minutes: u64,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DirectoryProvider {
    Authentik,
}

fn default_matrix_ids_attribute() -> String {
    "matrix_ids".to_owned()
}

fn default_verified_attribute() -> String {
    "matrix_verified".to_owned()
}

fn default_preferred_attribute() -> String {
    "matrix_preferred".to_owned()
}

fn default_link_request_attribute() -> String {
    "matrix_link_request".to_owned()
}

fn default_token_env() -> String {
    "DIRECTORY_TOKEN".to_owned()
}

fn default_refresh_minutes() -> u64 {
    15
}

fn default_stale_after_minutes() -> u64 {
    60
}

impl DirectoryConfig {
    pub fn refresh(&self) -> Duration {
        Duration::from_secs(self.refresh_minutes.max(1) * 60)
    }

    /// At least two refreshes, so one failed read does not make the data stale.
    pub fn stale_after(&self) -> Duration {
        Duration::from_secs(self.stale_after_minutes * 60).max(self.refresh() * 2)
    }
}

/// One person in the directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    /// The provider's id of the person (Authentik's `uuid`): the key for bot state.
    pub id: String,
    pub username: String,
    /// The name shown for them (Authentik's `name`; the apps that log in through it, such
    /// as the wiki, show the same name), trimmed; empty when the directory has none.
    pub name: String,
    /// Deactivated people are in [`People::all`] only (a bot can tell "deactivated" from
    /// "gone"); they are in no group and own no account.
    pub active: bool,
    /// The house account first, then the links in their order (admin-entered, then proved).
    pub accounts: Vec<OwnedUserId>,
    pub groups: BTreeSet<String>,
    /// The house account, when the login makes a valid one.
    pub house: Option<OwnedUserId>,
    pub links: Vec<Link>,
    /// What the person chose, unchecked ([`People::contact`] checks it).
    pub choice: Choice,
}

/// An account linked to a person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub id: OwnedUserId,
    /// An admin entered it (`matrix_ids`).
    pub by_admin: bool,
    /// The person proved it is theirs (`matrix_verified`).
    pub verified: bool,
}

/// The chosen-account attribute as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    None,
    Account(OwnedUserId),
    /// Not a Matrix ID.
    Malformed,
}

/// Where the bots invite and write to a person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Contact {
    /// Their chosen account: the house account or a proven link.
    Chosen(OwnedUserId),
    /// They have not chosen: nobody new is invited, they are asked to choose.
    Unchosen,
    /// What they chose cannot be used: like [`Contact::Unchosen`], and the admins are told.
    Invalid(Invalid),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    /// The attribute is not a Matrix ID.
    Malformed,
    /// Neither their house account nor a link they proved.
    NotTheirs,
    /// Another active person claims it too.
    Conflict,
}

impl Contact {
    pub fn account(&self) -> Option<&UserId> {
        match self {
            Contact::Chosen(id) => Some(id),
            _ => None,
        }
    }
}

/// Everyone in one read, with who owns which account.
#[derive(Clone, Debug, Default)]
pub struct People {
    all: Vec<Person>,
    /// Account → the active people claiming it (more than one: a conflict).
    claims: HashMap<OwnedUserId, Vec<usize>>,
}

impl People {
    pub fn new(all: Vec<Person>) -> Self {
        let mut claims: HashMap<OwnedUserId, Vec<usize>> = HashMap::new();
        for (i, person) in all.iter().enumerate().filter(|(_, p)| p.active) {
            for account in &person.accounts {
                claims.entry(account.clone()).or_default().push(i);
            }
        }
        Self { all, claims }
    }

    /// Everyone, deactivated people too.
    pub fn all(&self) -> &[Person] {
        &self.all
    }

    pub fn active(&self) -> impl Iterator<Item = &Person> {
        self.all.iter().filter(|p| p.active)
    }

    pub fn by_id(&self, id: &str) -> Option<&Person> {
        self.all.iter().find(|p| p.id == id)
    }

    /// The one active person whose account this is; nobody's when two claim it.
    pub fn owner_of(&self, account: &UserId) -> Option<&Person> {
        match self.claims.get(account).map(Vec::as_slice) {
            Some([one]) => Some(&self.all[*one]),
            _ => None,
        }
    }

    /// The active people claiming `account` (none, one, or a conflict's several).
    pub fn claimants<'a>(&'a self, account: &UserId) -> impl Iterator<Item = &'a Person> + 'a {
        self.claims
            .get(account)
            .into_iter()
            .flatten()
            .map(|i| &self.all[*i])
    }

    pub fn is_conflict(&self, account: &UserId) -> bool {
        self.claims.get(account).is_some_and(|c| c.len() > 1)
    }

    /// Accounts two or more active people claim, with their usernames, sorted.
    pub fn conflicts(&self) -> BTreeMap<OwnedUserId, Vec<&str>> {
        self.claims
            .iter()
            .filter(|(_, c)| c.len() > 1)
            .map(|(a, c)| {
                let mut names: Vec<&str> =
                    c.iter().map(|i| self.all[*i].username.as_str()).collect();
                names.sort_unstable();
                (a.clone(), names)
            })
            .collect()
    }

    /// The person's accounts that count: not claimed by anyone else (none when deactivated).
    pub fn accounts<'a>(&'a self, person: &'a Person) -> impl Iterator<Item = &'a UserId> {
        person
            .accounts
            .iter()
            .filter(move |a| person.active && !self.is_conflict(a))
            .map(|a| a.as_ref())
    }

    /// Where to invite and write to `person`.
    pub fn contact(&self, person: &Person) -> Contact {
        let id = match &person.choice {
            Choice::None => return Contact::Unchosen,
            Choice::Malformed => return Contact::Invalid(Invalid::Malformed),
            Choice::Account(id) => id,
        };
        let theirs = person.house.as_ref() == Some(id)
            || person.links.iter().any(|l| &l.id == id && l.verified);
        if !theirs {
            Contact::Invalid(Invalid::NotTheirs)
        } else if self.is_conflict(id) {
            Contact::Invalid(Invalid::Conflict)
        } else {
            Contact::Chosen(id.clone())
        }
    }

    /// The active members of `group`.
    pub fn members<'a>(&'a self, group: &'a str) -> impl Iterator<Item = &'a Person> {
        self.active().filter(move |p| p.groups.contains(group))
    }
}

/// One read of the directory: everyone, and which groups exist.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub people: People,
    /// Which of the groups asked for exist, whether anyone is in them or not.
    pub groups: BTreeSet<String>,
    pub read_at: SystemTime,
}

impl Snapshot {
    /// Whether the group exists (an unknown group is a typo or a broken directory; a bot
    /// leaves what follows it alone).
    pub fn has_group(&self, group: &str) -> bool {
        self.groups.contains(group)
    }

    pub fn age(&self, now: SystemTime) -> Duration {
        now.duration_since(self.read_at).unwrap_or_default()
    }

    /// Too old to act on ([`DirectoryConfig::stale_after`]).
    pub fn is_stale(&self, cfg: &DirectoryConfig, now: SystemTime) -> bool {
        self.age(now) > cfg.stale_after()
    }
}

/// How far in the future a link request's time may be (the provider's clock ahead of ours).
pub const LINK_REQUEST_AHEAD: Duration = Duration::from_secs(5 * 60);

/// An open link request: a logged-in person named `account` in the provider's flow
/// `house-matrix-accounts` without a code, so the directory bot invites that account to a
/// direct chat and sends it the code ([`crate::link::code`] for `username` and `account`).
/// One per person; a new one replaces the old, a successful link removes it. Read on its own
/// ([`Directory::link_requests`]), not with a [`Snapshot`]: it says who asked, never who owns
/// what.
///
/// It carries no shown name on purpose: the bot writes to an account anyone may name, and a
/// name the person edits themselves would put their text (links, lines) into that message.
/// Only the login, which the person cannot change, says who asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkRequest {
    /// The provider's id of the person who asked ([`Person::id`]).
    pub person: String,
    /// Their login: the code is for it, and it is all the requested account is told of them.
    pub username: String,
    /// The account to link (any server; the bot checks which it serves).
    pub account: OwnedUserId,
    /// When the person asked (the provider's clock).
    pub at: DateTime<Utc>,
    /// The accounts the person proved already (`verified_attribute`, on other servers).
    pub verified: Vec<OwnedUserId>,
}

impl LinkRequest {
    /// Asked less than a slot ([`crate::link::SLOT_SECONDS`]) ago and not more than
    /// [`LINK_REQUEST_AHEAD`] in the future: an older one is stale (the person has given up or
    /// asks again), and a time further ahead is a broken value.
    pub fn is_fresh(&self, now: SystemTime) -> bool {
        let at = SystemTime::from(self.at);
        match now.duration_since(at) {
            Ok(age) => age < Duration::from_secs(crate::link::SLOT_SECONDS),
            Err(ahead) => ahead.duration() <= LINK_REQUEST_AHEAD,
        }
    }

    /// Until when it counts: a slot after it was made.
    pub fn until(&self) -> SystemTime {
        SystemTime::from(self.at) + Duration::from_secs(crate::link::SLOT_SECONDS)
    }

    /// The key that tells one request from the next (a new one replaces the old: another
    /// account, or the same one asked again later).
    pub fn key(&self) -> String {
        format!("{} {} {}", self.person, self.account, self.at.timestamp())
    }
}

/// The value of the link-request attribute: `{"id": "@x:server", "at": <unix seconds>}`.
/// Anything else (no object, no valid Matrix ID, no time or a time that is no whole number of
/// seconds a date can hold) is no request: `None`, never an error.
fn link_request_value(value: &serde_json::Value) -> Option<(OwnedUserId, DateTime<Utc>)> {
    let object = value.as_object()?;
    let account = UserId::parse(object.get("id")?.as_str()?.trim()).ok()?;
    let at = match object.get("at")? {
        serde_json::Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))?,
        _ => return None,
    };
    Some((account, DateTime::from_timestamp(at, 0)?))
}

/// The link requests of active non-service users in one of `groups` (the groups whose rooms
/// the bot manages; none: nobody's). Never fails: a user without a request, with one that
/// cannot be read, or outside those groups (a guest, a former resident, a user whose groups
/// the answer lacks) is left out.
fn link_requests_of(
    users: &[User],
    cfg: &DirectoryConfig,
    server_name: &str,
    groups: &BTreeSet<String>,
) -> Vec<LinkRequest> {
    users
        .iter()
        .filter(|u| u.is_active && !u.kind.contains("service_account"))
        .filter(|u| !u.uuid.trim().is_empty())
        .filter(|u| {
            u.groups_obj
                .iter()
                .flatten()
                .any(|g| groups.contains(&g.name))
        })
        .filter_map(|u| {
            let (account, at) = link_request_value(u.attributes.get(&cfg.link_request_attribute)?)?;
            let house = house_account(u, cfg, server_name);
            let verified = links_of(u, cfg, server_name, house.as_deref())
                .into_iter()
                .filter(|l| l.verified)
                .map(|l| l.id)
                .collect();
            Some(LinkRequest {
                person: u.uuid.trim().to_owned(),
                username: u.username.clone(),
                account,
                at,
                verified,
            })
        })
        .collect()
}

/// Whether [`Directory::link_requests`] asks Authentik for only the users with a request
/// (`attributes={"<attribute>__isnull": false}`, a filter Authentik 2026.8 has). Switched off,
/// or for an attribute name a filter cannot carry, it reads every active user; the answer is
/// read the same either way, so a provider that ignores the filter changes nothing.
const LINK_REQUEST_FILTER: bool = true;

/// The users' query for [`Directory::link_requests`]: active users with their groups (only
/// people in the bot's groups may ask), and with `filter` only those with the attribute set.
fn link_request_query(attribute: &str, filter: bool) -> String {
    let plain = "core/users/?is_active=true&include_groups=true".to_owned();
    // A Django lookup: a name with `__` or other characters would be read as more than a key.
    let usable = !attribute.is_empty()
        && !attribute.contains("__")
        && attribute
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !filter || !usable {
        return plain;
    }
    let json = format!("{{\"{attribute}__isnull\":false}}");
    format!("{plain}&attributes={}", percent_encode(&json))
}

/// Percent-encoding of a query value (all but the unreserved characters).
fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The members' accounts of some groups.
pub type Groups = BTreeMap<String, BTreeSet<OwnedUserId>>;

/// The accounts of each of `wanted` that anyone is in; a group nobody is in (or that does
/// not exist) is missing from the answer. [`groups_with_empty`] tells the two apart. An
/// account two of `people` claim is in no group.
pub fn groups(people: &[Person], wanted: &BTreeSet<String>) -> Groups {
    let index = People::new(people.to_vec());
    let mut out = Groups::new();
    for person in index.active() {
        for group in person.groups.iter().filter(|g| wanted.contains(*g)) {
            out.entry(group.clone())
                .or_default()
                .extend(index.accounts(person).map(UserId::to_owned));
        }
    }
    out
}

/// [`groups`], plus an empty entry for each of `wanted` that `existing` names (from
/// [`Directory::existing_groups`]) and nobody is in: a group that exists but has nobody is
/// in the answer with no accounts, only an unknown group is missing from it.
pub fn groups_with_empty(
    people: &[Person],
    wanted: &BTreeSet<String>,
    existing: &BTreeSet<String>,
) -> Groups {
    let mut out = groups(people, wanted);
    for group in wanted.iter().filter(|g| existing.contains(*g)) {
        out.entry(group.clone()).or_default();
    }
    out
}

/// A reader of the directory.
pub struct Directory {
    cfg: DirectoryConfig,
    server_name: String,
    token: String,
    http: reqwest::Client,
    /// The provider refused the link-request filter once (400): every later read of the
    /// requests goes without it for the rest of the process, and the warning is logged once.
    filter_refused: AtomicBool,
}

impl Directory {
    /// The reader for `cfg`, with `own` (the bot's account) giving the default server. Fails
    /// without a token in `token_env`.
    pub fn new(cfg: &DirectoryConfig, own: &UserId) -> Result<Self> {
        let token = std::env::var(&cfg.token_env)
            .ok()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .with_context(|| format!("no directory token in {}", cfg.token_env))?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("the directory's HTTP client")?;
        Ok(Self {
            server_name: cfg
                .server_name
                .clone()
                .unwrap_or_else(|| own.server_name().to_string()),
            cfg: cfg.clone(),
            token,
            http,
            filter_refused: AtomicBool::new(false),
        })
    }

    pub fn config(&self) -> &DirectoryConfig {
        &self.cfg
    }

    /// The house's server (of the house accounts).
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Every active person. An answer without anyone is an error: a directory that names
    /// nobody is broken, and a bot acting on it would remove everyone.
    pub async fn people(&self) -> Result<Vec<Person>> {
        Ok(self.everyone().await?.active().cloned().collect())
    }

    /// Everyone and which of `wanted` exist, in one go.
    pub async fn snapshot(&self, wanted: &BTreeSet<String>) -> Result<Snapshot> {
        let people = self.everyone().await?;
        let groups = self.existing_groups(wanted).await?;
        Ok(Snapshot {
            people,
            groups,
            read_at: SystemTime::now(),
        })
    }

    async fn everyone(&self) -> Result<People> {
        let DirectoryProvider::Authentik = self.cfg.provider;
        let users: Vec<User> = self
            .all_pages("core/users/?include_groups=true", "users")
            .await?;
        // A delete and a create between two pages keep the count but shift the pages: someone
        // is read twice and someone else skipped (who would look removed).
        let mut seen = HashSet::new();
        if let Some(twice) = users
            .iter()
            .map(|u| u.uuid.trim())
            .find(|uuid| !uuid.is_empty() && !seen.insert(*uuid))
        {
            bail!("the directory's users changed while they were read ({twice} read twice)");
        }
        let people = people_of(&users, &self.cfg, &self.server_name)?;
        if !people.iter().any(|p| p.active) {
            bail!("the directory names nobody");
        }
        Ok(People::new(people))
    }

    /// The open link requests ([`LinkRequest`]), fresh or not, of the active people in one of
    /// `groups` (the groups whose rooms the bot manages; an empty set: nobody's), for a bot
    /// that sends link codes; cheap enough to ask every few seconds. Not all or nothing like a
    /// snapshot: it decides nothing about rooms or rights, and a request that cannot be read
    /// is left out. An answer the provider refuses or that cannot be read is an error.
    ///
    /// Authentik is asked for the users with the attribute only ([`LINK_REQUEST_FILTER`]); when
    /// it refuses that filter (400), every active user is read instead, from then on for the
    /// life of the process (one warning, not one per read), and the requests are picked out
    /// here either way.
    pub async fn link_requests(&self, groups: &BTreeSet<String>) -> Result<Vec<LinkRequest>> {
        let DirectoryProvider::Authentik = self.cfg.provider;
        let attribute = &self.cfg.link_request_attribute;
        let filter = LINK_REQUEST_FILTER && !self.filter_refused.load(Ordering::Relaxed);
        let filtered = link_request_query(attribute, filter);
        let users: Vec<User> = match self.all_pages(&filtered, "users").await {
            Err(e) if filtered.contains("&attributes=") && is_bad_request(&e) => {
                if !self.filter_refused.swap(true, Ordering::Relaxed) {
                    warn!("Directory: the link-request filter was refused ({e:#}); reading every active user from now on");
                }
                self.all_pages(&link_request_query(attribute, false), "users")
                    .await?
            }
            read => read?,
        };
        Ok(link_requests_of(
            &users,
            &self.cfg,
            &self.server_name,
            groups,
        ))
    }

    /// Which of `wanted` exist in the directory, whether anyone is in them or not (a group
    /// that is missing from [`groups`] exists with nobody in it when it is in this answer).
    /// An answer without any group is an error: every house has groups, and a token that may
    /// not see them gets an empty list from Authentik, not a refusal.
    pub async fn existing_groups(&self, wanted: &BTreeSet<String>) -> Result<BTreeSet<String>> {
        let DirectoryProvider::Authentik = self.cfg.provider;
        if wanted.is_empty() {
            return Ok(BTreeSet::new());
        }
        let all: Vec<Group> = self
            .all_pages("core/groups/?include_users=false", "groups")
            .await?;
        if all.is_empty() {
            bail!("the directory names no group");
        }
        Ok(all
            .into_iter()
            .map(|g| g.name)
            .filter(|g| wanted.contains(g))
            .collect())
    }

    /// Every page of a list (`path` with its query, below the API base). Each page must give
    /// the total (`pagination.count`; Authentik always does), the pages must agree on it and
    /// add up to it: a list that changed while it was read (someone added or deleted) may have
    /// skipped someone, who would then look removed.
    async fn all_pages<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        what: &str,
    ) -> Result<Vec<T>> {
        let base = self.cfg.api_url.trim_end_matches('/');
        let mut out: Vec<T> = Vec::new();
        let mut page = 1u64;
        let mut total: Option<u64> = None;
        // At most 50 pages of 200: far more than a house.
        for _ in 0..50 {
            let url = format!("{base}/{path}&page_size=200&page={page}");
            let body = self
                .http
                .get(&url)
                .bearer_auth(&self.token)
                .send()
                .await
                .context("reading the directory")?
                .error_for_status()
                .context("reading the directory")?
                .bytes()
                .await
                .context("reading the directory")?;
            let answer: Page<T> =
                serde_json::from_slice(&body).context("the directory's answer")?;
            out.extend(answer.results);
            let Some(pagination) = answer.pagination else {
                bail!("the directory's answer has no pagination for its {what}");
            };
            let Some(count) = pagination.count else {
                bail!("the directory's answer has no count of its {what}");
            };
            if total.is_some_and(|t| t != count) {
                bail!("the directory's {what} changed while they were read");
            }
            total = Some(count);
            match pagination.next.as_u64() {
                Some(next) if next > page => page = next,
                _ => {
                    if total.is_some_and(|t| t != out.len() as u64) {
                        bail!("the directory's {what} changed while they were read");
                    }
                    return Ok(out);
                }
            }
        }
        bail!("the directory has more than 50 pages of {what}")
    }
}

/// Whether a directory error is an outage of the provider, not a fault of the bot or its
/// config: it could not be reached (connect, DNS), timed out, broke off while answering, or
/// answered 5xx (down, overloaded, a proxy without its backend) or 429 (too many requests).
/// Everything else is no outage: a refusal (401/403: a revoked or wrong token; 400, 404: a
/// wrong URL or API), an answer that cannot be read (a changed schema), and what a read finds
/// wrong itself (nobody in it, no groups, a list that changed while it was read). A bot reports
/// an outage as `Alive::unavailable` (never unhealthy) and the rest as `Alive::failed`, which
/// turns unhealthy after its time: those need someone to fix the bot or its config.
pub fn is_outage(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|e| e.downcast_ref::<reqwest::Error>())
        .any(|e| match e.status() {
            Some(status) => {
                status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            }
            None => e.is_connect() || e.is_timeout() || e.is_request() || e.is_body(),
        })
}

/// The provider answered 400: it does not take the query (an unknown filter, say).
fn is_bad_request(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|e| e.downcast_ref::<reqwest::Error>())
        .any(|e| e.status() == Some(reqwest::StatusCode::BAD_REQUEST))
}

#[derive(Deserialize, Debug)]
struct User {
    #[serde(default)]
    uuid: String,
    username: String,
    #[serde(default)]
    name: String,
    #[serde(default = "yes")]
    is_active: bool,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    attributes: serde_json::Map<String, serde_json::Value>,
    /// Missing when the token or the API version does not give the groups: an error, not
    /// "in no group" (that would remove everyone from everything).
    groups_obj: Option<Vec<Group>>,
}

#[derive(Deserialize, Debug)]
struct Group {
    name: String,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct Page<T> {
    results: Vec<T>,
    #[serde(default)]
    pagination: Option<Pagination>,
}

#[derive(Deserialize)]
struct Pagination {
    /// The next page's number, 0 on the last one.
    #[serde(default)]
    next: serde_json::Value,
    /// How many there are on all pages together (required).
    #[serde(default)]
    count: Option<u64>,
}

fn people_of(users: &[User], cfg: &DirectoryConfig, server_name: &str) -> Result<Vec<Person>> {
    let mut out = Vec::new();
    for u in users.iter().filter(|u| !u.kind.contains("service_account")) {
        let Some(groups) = &u.groups_obj else {
            bail!("the directory's answer has no groups for its users");
        };
        if u.uuid.trim().is_empty() {
            bail!("the directory's answer has no id for its users");
        }
        let house = house_account(u, cfg, server_name);
        let links = links_of(u, cfg, server_name, house.as_deref());
        let accounts = house
            .iter()
            .cloned()
            .chain(links.iter().map(|l| l.id.clone()))
            .collect();
        out.push(Person {
            id: u.uuid.trim().to_owned(),
            username: u.username.clone(),
            name: u.name.trim().to_owned(),
            active: u.is_active,
            accounts,
            groups: if u.is_active {
                groups.iter().map(|g| g.name.clone()).collect()
            } else {
                BTreeSet::new()
            },
            house,
            links,
            choice: choice_of(u, cfg),
        });
    }
    Ok(out)
}

fn house_account(user: &User, cfg: &DirectoryConfig, server_name: &str) -> Option<OwnedUserId> {
    let localpart = cfg
        .localpart_attribute
        .as_ref()
        .and_then(|a| user.attributes.get(a))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .unwrap_or(&user.username)
        .to_lowercase();
    match UserId::parse(format!("@{localpart}:{server_name}")) {
        Ok(id) => Some(id),
        Err(_) => {
            warn!(user = %user.username, "Directory: no valid house account");
            None
        }
    }
}

/// The strings of a list attribute: a list, or one string separated by commas or spaces.
fn listed<'a>(user: &'a User, attribute: &str) -> Vec<&'a str> {
    let items: Vec<&str> = match user.attributes.get(attribute) {
        Some(serde_json::Value::Array(items)) => items.iter().filter_map(|v| v.as_str()).collect(),
        Some(serde_json::Value::String(s)) => s.split([',', ' ', '\n', '\t']).collect(),
        _ => Vec::new(),
    };
    items
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

fn links_of(
    user: &User,
    cfg: &DirectoryConfig,
    server_name: &str,
    house: Option<&UserId>,
) -> Vec<Link> {
    let mut out: Vec<Link> = Vec::new();
    let sources = [
        (cfg.matrix_ids_attribute.as_str(), true),
        (cfg.verified_attribute.as_str(), false),
    ];
    for (attribute, by_admin) in sources {
        for item in listed(user, attribute) {
            let Ok(id) = UserId::parse(item) else {
                warn!(user = %user.username, %attribute, "Directory: not a Matrix ID, left out");
                continue;
            };
            if Some(id.as_ref()) == house {
                continue;
            }
            if id.server_name().as_str() == server_name {
                warn!(user = %user.username, %attribute, account = %id,
                    "Directory: a link on the house's server, left out (house accounts come from the login)");
                continue;
            }
            match out.iter_mut().find(|l| l.id == id) {
                Some(link) if by_admin => link.by_admin = true,
                Some(link) => link.verified = true,
                None => out.push(Link {
                    id,
                    by_admin,
                    verified: !by_admin,
                }),
            }
        }
    }
    out
}

fn choice_of(user: &User, cfg: &DirectoryConfig) -> Choice {
    match user.attributes.get(&cfg.preferred_attribute) {
        None | Some(serde_json::Value::Null) => Choice::None,
        Some(serde_json::Value::String(s)) if s.trim().is_empty() => Choice::None,
        Some(serde_json::Value::String(s)) => match UserId::parse(s.trim()) {
            Ok(id) => Choice::Account(id),
            Err(_) => Choice::Malformed,
        },
        Some(_) => Choice::Malformed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DirectoryConfig {
        toml::from_str(
            "provider = 'authentik'\napi_url = 'https://login.example.org/api/v3'\n\
             localpart_attribute = 'matrix_localpart'",
        )
        .unwrap()
    }

    fn people() -> Vec<Person> {
        let users: Vec<User> = serde_json::from_value(serde_json::json!([
            {"uuid": "u-anna", "username": "anna", "name": " Anna Schmidt ", "groups_obj": [{"name": "residents"}, {"name": "kitchen-floor-1"}],
             "attributes": {"matrix_ids": ["@anna:matrix.org", "not an id", "@anna:house.org"]}},
            {"uuid": "u-ben", "username": "Ben", "groups_obj": [{"name": "kitchen-floor-1"}, {"name": "finance"}],
             "attributes": {"matrix_localpart": "benny", "matrix_ids": "@ben:matrix.org, @ben:example.org"}},
            {"uuid": "u-old", "username": "old", "is_active": false, "groups_obj": [{"name": "kitchen-floor-1"}]},
            {"uuid": "u-svc", "username": "svc", "type": "service_account", "groups_obj": [{"name": "kitchen-floor-1"}]},
            {"uuid": "u-cara", "username": "cara", "groups_obj": [{"name": "kitchen-floor-3"}]}
        ]))
        .unwrap();
        people_of(&users, &cfg(), "house.org")
            .unwrap()
            .into_iter()
            .filter(|p| p.active)
            .collect()
    }

    fn ids(list: &[&str]) -> Vec<OwnedUserId> {
        list.iter().map(|s| UserId::parse(*s).unwrap()).collect()
    }

    #[test]
    fn defaults() {
        let c = cfg();
        assert_eq!(c.token_env, "DIRECTORY_TOKEN");
        assert_eq!(c.matrix_ids_attribute, "matrix_ids");
        assert_eq!(c.refresh(), Duration::from_secs(900));
        assert_eq!(c.verified_attribute, "matrix_verified");
        assert_eq!(c.preferred_attribute, "matrix_preferred");
        // a config from before link requests: the default attribute
        assert_eq!(c.link_request_attribute, "matrix_link_request");
        assert_eq!(c.stale_after(), Duration::from_secs(3600));
        assert!(c.server_name.is_none());
    }

    #[test]
    fn active_people_with_their_accounts() {
        let p = people();
        assert_eq!(
            p.iter().map(|p| p.username.as_str()).collect::<Vec<_>>(),
            ["anna", "Ben", "cara"]
        );
        // the shown name, trimmed; none in the directory: empty
        assert_eq!(
            (p[0].name.as_str(), p[1].name.as_str()),
            ("Anna Schmidt", "")
        );
        // the house account first, no duplicates, invalid IDs left out
        assert_eq!(p[0].accounts, ids(&["@anna:house.org", "@anna:matrix.org"]));
        assert_eq!(
            p[1].accounts,
            ids(&["@benny:house.org", "@ben:matrix.org", "@ben:example.org"])
        );
    }

    #[test]
    fn each_wanted_group_with_its_members_accounts() {
        let wanted: BTreeSet<String> = ["kitchen-floor-1", "finance", "kitchen-floor-4"]
            .map(String::from)
            .into();
        let g = groups(&people(), &wanted);
        assert_eq!(
            g["kitchen-floor-1"],
            ids(&[
                "@anna:house.org",
                "@anna:matrix.org",
                "@benny:house.org",
                "@ben:matrix.org",
                "@ben:example.org"
            ])
            .into_iter()
            .collect()
        );
        assert_eq!(g["finance"].len(), 3);
        assert!(!g.contains_key("kitchen-floor-4"));
        assert!(!g.contains_key("kitchen-floor-3"));
    }

    #[test]
    fn an_existing_group_without_anyone_is_empty_an_unknown_one_missing() {
        let wanted: BTreeSet<String> = ["kitchen-floor-1", "finance", "kitchen-floor-4", "gone"]
            .map(String::from)
            .into();
        let existing: BTreeSet<String> = ["kitchen-floor-1", "finance", "kitchen-floor-4"]
            .map(String::from)
            .into();
        let g = groups_with_empty(&people(), &wanted, &existing);
        assert_eq!(g["kitchen-floor-1"].len(), 5);
        assert_eq!(g["finance"].len(), 3);
        assert!(g["kitchen-floor-4"].is_empty());
        assert!(!g.contains_key("gone"));
        // a group not wanted stays out, even if it exists
        assert!(!g.contains_key("kitchen-floor-3"));
        // a group someone is in counts even when the group list missed it (read in between)
        let g = groups_with_empty(&people(), &wanted, &BTreeSet::new());
        assert_eq!(g, groups(&people(), &wanted));
    }

    fn reader(url: String, token_env: &str) -> Directory {
        let mut c = cfg();
        c.api_url = url;
        c.token_env = token_env.into();
        Directory::new(&c, &UserId::parse("@bot:house.org").unwrap()).unwrap()
    }

    #[tokio::test]
    async fn existing_groups_reads_every_page_and_no_group_at_all_is_an_error() {
        use wiremock::matchers::{header, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        std::env::set_var("MXBOT_TEST_GROUPS_TOKEN", "secret");
        let wanted: BTreeSet<String> = ["finance", "kitchen-floor-4", "gone"]
            .map(String::from)
            .into();
        let server = MockServer::start().await;
        for (page, next, names) in [
            ("1", 2, ["residents", "finance"]),
            ("2", 0, ["kitchen-floor-4", "it-admins"]),
        ] {
            Mock::given(method("GET"))
                .and(path("/api/v3/core/groups/"))
                .and(query_param("page", page))
                .and(query_param("page_size", "200"))
                .and(query_param("include_users", "false"))
                .and(header("authorization", "Bearer secret"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "pagination": {"next": next, "count": 4},
                    "results": names.map(|n| serde_json::json!({"pk": n, "name": n, "users": []})),
                })))
                .expect(1)
                .mount(&server)
                .await;
        }
        let dir = reader(
            format!("{}/api/v3", server.uri()),
            "MXBOT_TEST_GROUPS_TOKEN",
        );
        let found = dir.existing_groups(&wanted).await.unwrap();
        assert_eq!(
            found,
            ["finance", "kitchen-floor-4"].map(String::from).into()
        );
        // nothing wanted: nothing asked
        assert!(dir
            .existing_groups(&BTreeSet::new())
            .await
            .unwrap()
            .is_empty());

        // a token that may not see groups gets an empty list: an error, not "none exist"
        let empty = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"pagination": {"next": 0, "count": 0}, "results": []}),
            ))
            .mount(&empty)
            .await;
        let dir = reader(empty.uri(), "MXBOT_TEST_GROUPS_TOKEN");
        assert!(dir.existing_groups(&wanted).await.is_err());
        let refused = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&refused)
            .await;
        let dir = reader(refused.uri(), "MXBOT_TEST_GROUPS_TOKEN");
        assert!(dir.existing_groups(&wanted).await.is_err());
        let garbled = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
            .mount(&garbled)
            .await;
        let dir = reader(garbled.uri(), "MXBOT_TEST_GROUPS_TOKEN");
        assert!(dir.existing_groups(&wanted).await.is_err());
    }

    #[tokio::test]
    async fn reads_every_page_with_the_token_and_an_empty_answer_is_an_error() {
        use wiremock::matchers::{header, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        for (page, next, name) in [("1", 2, "one"), ("2", 0, "two")] {
            Mock::given(method("GET"))
                .and(path("/api/v3/core/users/"))
                .and(query_param("page", page))
                .and(query_param("include_groups", "true"))
                .and(header("authorization", "Bearer secret"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "pagination": {"next": next, "count": 2},
                    "results": [{"uuid": name, "username": name, "type": "internal", "attributes": {},
                                 "groups_obj": [{"name": "residents"}]}],
                })))
                .expect(1)
                .mount(&server)
                .await;
        }
        std::env::set_var("MXBOT_TEST_DIRECTORY_TOKEN", " secret\n");
        let reader = |url: String| {
            let mut c = cfg();
            c.api_url = url;
            c.token_env = "MXBOT_TEST_DIRECTORY_TOKEN".into();
            Directory::new(&c, &UserId::parse("@bot:house.org").unwrap()).unwrap()
        };
        let people = reader(format!("{}/api/v3/", server.uri()))
            .people()
            .await
            .unwrap();
        let names: Vec<&str> = people.iter().map(|p| p.username.as_str()).collect();
        assert_eq!(names, ["one", "two"]);
        // the bot's own server for the house accounts
        assert_eq!(people[0].accounts, ids(&["@one:house.org"]));

        let empty = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"pagination": {"next": 0, "count": 0}, "results": []}),
            ))
            .mount(&empty)
            .await;
        assert!(reader(empty.uri()).people().await.is_err());
        let refused = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&refused)
            .await;
        assert!(reader(refused.uri()).people().await.is_err());

        let mut c = cfg();
        c.token_env = "MXBOT_TEST_NO_SUCH_TOKEN".into();
        assert!(Directory::new(&c, &UserId::parse("@bot:house.org").unwrap()).is_err());
    }

    fn index(users: serde_json::Value) -> People {
        let users: Vec<User> = serde_json::from_value(users).unwrap();
        People::new(people_of(&users, &cfg(), "house.org").unwrap())
    }

    fn id(s: &str) -> OwnedUserId {
        UserId::parse(s).unwrap()
    }

    #[test]
    fn links_admin_entered_and_proved_and_never_on_the_house_server() {
        let p = index(serde_json::json!([
            {"uuid": "1", "username": "anna", "groups_obj": [],
             "attributes": {"matrix_ids": ["@anna:matrix.org", "@bob:house.org", "@anna:house.org"],
                            "matrix_verified": "@anna:matrix.org @anna:example.org"}}
        ]));
        let anna = &p.all()[0];
        assert_eq!(anna.house, Some(id("@anna:house.org")));
        assert_eq!(
            anna.links,
            [
                Link {
                    id: id("@anna:matrix.org"),
                    by_admin: true,
                    verified: true
                },
                Link {
                    id: id("@anna:example.org"),
                    by_admin: false,
                    verified: true
                },
            ]
        );
        // someone else's house account cannot be linked
        assert!(!anna.accounts.contains(&id("@bob:house.org")));
        assert_eq!(p.owner_of(&id("@anna:example.org")).unwrap().id, "1");
    }

    #[test]
    fn the_contact_is_only_what_the_person_chose_from_their_own_accounts() {
        let p = index(serde_json::json!([
            // Person A: only the house account, nothing chosen
            {"uuid": "a", "username": "a", "groups_obj": [], "attributes": {}},
            // Person B: chose a proved link
            {"uuid": "b", "username": "b", "groups_obj": [],
             "attributes": {"matrix_verified": ["@b:matrix.org"], "matrix_preferred": "@b:matrix.org"}},
            // chose the house account
            {"uuid": "c", "username": "c", "groups_obj": [],
             "attributes": {"matrix_preferred": " @c:house.org "}},
            // chose a link only an admin entered: not proved
            {"uuid": "d", "username": "d", "groups_obj": [],
             "attributes": {"matrix_ids": ["@d:matrix.org"], "matrix_preferred": "@d:matrix.org"}},
            // chose someone else's account
            {"uuid": "e", "username": "e", "groups_obj": [],
             "attributes": {"matrix_preferred": "@a:house.org"}},
            {"uuid": "f", "username": "f", "groups_obj": [],
             "attributes": {"matrix_preferred": "not an id"}},
            {"uuid": "g", "username": "g", "groups_obj": [],
             "attributes": {"matrix_preferred": ["@g:house.org"]}},
            {"uuid": "h", "username": "h", "groups_obj": [], "attributes": {"matrix_preferred": ""}}
        ]));
        let contact = |uuid: &str| p.contact(p.by_id(uuid).unwrap());
        assert_eq!(contact("a"), Contact::Unchosen);
        assert_eq!(contact("b"), Contact::Chosen(id("@b:matrix.org")));
        assert_eq!(contact("c"), Contact::Chosen(id("@c:house.org")));
        assert_eq!(contact("d"), Contact::Invalid(Invalid::NotTheirs));
        assert_eq!(contact("e"), Contact::Invalid(Invalid::NotTheirs));
        assert_eq!(contact("f"), Contact::Invalid(Invalid::Malformed));
        assert_eq!(contact("g"), Contact::Invalid(Invalid::Malformed));
        assert_eq!(contact("h"), Contact::Unchosen);
        assert_eq!(contact("b").account(), Some(id("@b:matrix.org").as_ref()));
        assert_eq!(contact("a").account(), None);
    }

    #[test]
    fn an_account_two_active_people_claim_counts_for_nobody() {
        let p = index(serde_json::json!([
            {"uuid": "1", "username": "anna", "groups_obj": [{"name": "kitchen-floor-1"}],
             "attributes": {"matrix_verified": ["@shared:matrix.org"], "matrix_preferred": "@shared:matrix.org"}},
            {"uuid": "2", "username": "ben", "groups_obj": [{"name": "kitchen-floor-1"}],
             "attributes": {"matrix_ids": ["@shared:matrix.org"]}},
            // renamed: keeps the old localpart, which the new "cara" would get too
            {"uuid": "3", "username": "cara-old", "groups_obj": [],
             "attributes": {"matrix_localpart": "cara"}},
            {"uuid": "4", "username": "cara", "groups_obj": [{"name": "it-admins"}], "attributes": {}},
            // a deactivated person's claim is no conflict
            {"uuid": "5", "username": "gone", "is_active": false, "groups_obj": [{"name": "kitchen-floor-1"}],
             "attributes": {"matrix_ids": ["@ben-too:matrix.org"]}},
            {"uuid": "6", "username": "dora", "groups_obj": [],
             "attributes": {"matrix_ids": ["@ben-too:matrix.org"]}}
        ]));
        assert!(p.owner_of(&id("@shared:matrix.org")).is_none());
        assert!(p.owner_of(&id("@cara:house.org")).is_none());
        assert_eq!(
            p.conflicts().into_iter().collect::<Vec<_>>(),
            [
                (id("@cara:house.org"), vec!["cara", "cara-old"]),
                (id("@shared:matrix.org"), vec!["anna", "ben"]),
            ]
        );
        let anna = p.by_id("1").unwrap();
        assert_eq!(p.contact(anna), Contact::Invalid(Invalid::Conflict));
        assert_eq!(
            p.accounts(anna).collect::<Vec<_>>(),
            [&*id("@anna:house.org")]
        );
        assert_eq!(p.owner_of(&id("@ben-too:matrix.org")).unwrap().id, "6");
        // the shared account is in no group
        let wanted: BTreeSet<String> = ["kitchen-floor-1".to_owned()].into();
        let g = groups(p.all(), &wanted);
        assert_eq!(
            g["kitchen-floor-1"],
            [id("@anna:house.org"), id("@ben:house.org")].into()
        );
    }

    #[test]
    fn a_deactivated_person_is_known_but_in_no_group_and_owns_nothing() {
        let p = index(serde_json::json!([
            {"uuid": "1", "username": "anna", "groups_obj": [{"name": "residents"}], "attributes": {}},
            {"uuid": "2", "username": "gone", "is_active": false,
             "groups_obj": [{"name": "residents"}], "attributes": {}}
        ]));
        let gone = p.by_id("2").unwrap();
        assert!(!gone.active && gone.groups.is_empty());
        assert!(p.owner_of(&id("@gone:house.org")).is_none());
        assert_eq!(p.accounts(gone).count(), 0);
        assert_eq!(
            p.members("residents")
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            ["1"]
        );
        assert_eq!(p.active().count(), 1);
        assert_eq!(p.all().len(), 2);
    }

    #[test]
    fn users_without_groups_or_ids_make_the_answer_unusable() {
        let no_groups: Vec<User> =
            serde_json::from_value(serde_json::json!([{"uuid": "1", "username": "a"}])).unwrap();
        assert!(people_of(&no_groups, &cfg(), "house.org").is_err());
        let no_id: Vec<User> =
            serde_json::from_value(serde_json::json!([{"username": "a", "groups_obj": []}]))
                .unwrap();
        assert!(people_of(&no_id, &cfg(), "house.org").is_err());
        // service accounts are left out before the check
        let svc: Vec<User> = serde_json::from_value(serde_json::json!([
            {"username": "svc", "type": "service_account"},
            {"uuid": "1", "username": "a", "groups_obj": []}
        ]))
        .unwrap();
        assert_eq!(people_of(&svc, &cfg(), "house.org").unwrap().len(), 1);
    }

    #[test]
    fn a_snapshot_is_stale_after_its_time_and_at_least_two_refreshes() {
        let mut c = cfg();
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let snap = Snapshot {
            people: People::default(),
            groups: ["residents".to_owned()].into(),
            read_at: at,
        };
        assert!(snap.has_group("residents") && !snap.has_group("gone"));
        assert!(!snap.is_stale(&c, at + Duration::from_secs(3600)));
        assert!(snap.is_stale(&c, at + Duration::from_secs(3601)));
        // a clock that went back: not stale
        assert!(!snap.is_stale(&c, at - Duration::from_secs(10)));
        c.stale_after_minutes = 1;
        assert_eq!(c.stale_after(), Duration::from_secs(1800));
    }

    #[tokio::test]
    async fn a_list_that_changed_between_its_pages_is_an_error() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        std::env::set_var("MXBOT_TEST_CHANGING_TOKEN", "secret");
        let user = |n: &str| serde_json::json!({"uuid": n, "username": n, "groups_obj": [{"name": "residents"}]});
        let server = MockServer::start().await;
        // the total grew from 3 to 4 between the pages
        Mock::given(method("GET"))
            .and(path("/core/users/"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": {"next": 2, "count": 3}, "results": [user("a"), user("b")]})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/core/users/"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": {"next": 0, "count": 4}, "results": [user("c"), user("d")]})))
            .mount(&server)
            .await;
        let dir = reader(server.uri(), "MXBOT_TEST_CHANGING_TOKEN");
        assert!(dir.people().await.is_err());

        // the total stayed, but the pages give fewer (someone deleted in between)
        let short = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": {"next": 0, "count": 3}, "results": [user("a"), user("b")]})))
            .mount(&short)
            .await;
        let dir = reader(short.uri(), "MXBOT_TEST_CHANGING_TOKEN");
        assert!(dir.people().await.is_err());

        // a delete and a create between the pages: the total stays, b is read twice, c not
        let shifted = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/core/users/"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": {"next": 2, "count": 3}, "results": [user("a"), user("b")]})))
            .mount(&shifted)
            .await;
        Mock::given(method("GET"))
            .and(path("/core/users/"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": {"next": 0, "count": 3}, "results": [user("b")]})))
            .mount(&shifted)
            .await;
        let dir = reader(shifted.uri(), "MXBOT_TEST_CHANGING_TOKEN");
        let error = dir.people().await.unwrap_err();
        assert!(error.to_string().contains("b read twice"), "{error:#}");

        // an answer without the total (or without pagination) cannot be checked: an error
        for answer in [
            serde_json::json!({"pagination": {"next": 0}, "results": [user("a")]}),
            serde_json::json!({"results": [user("a")]}),
        ] {
            let uncounted = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_json(answer))
                .mount(&uncounted)
                .await;
            let dir = reader(uncounted.uri(), "MXBOT_TEST_CHANGING_TOKEN");
            let error = dir.people().await.unwrap_err();
            assert!(error.to_string().contains("users"), "{error:#}");
            assert!(dir
                .existing_groups(&["residents".to_owned()].into())
                .await
                .is_err());
        }

        // everything agrees: a snapshot with the groups that exist
        let fine = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/core/users/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": {"next": 0, "count": 2},
                "results": [user("a"), {"uuid": "z", "username": "z", "is_active": false, "groups_obj": []}]})))
            .mount(&fine)
            .await;
        Mock::given(method("GET"))
            .and(path("/core/groups/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": {"next": 0, "count": 1}, "results": [{"name": "residents"}]})))
            .mount(&fine)
            .await;
        let dir = reader(fine.uri(), "MXBOT_TEST_CHANGING_TOKEN");
        let wanted: BTreeSet<String> = ["residents".to_owned(), "gone".to_owned()].into();
        let snap = dir.snapshot(&wanted).await.unwrap();
        assert_eq!(snap.people.all().len(), 2);
        assert_eq!(snap.people.active().count(), 1);
        assert_eq!(snap.groups, ["residents".to_owned()].into());
        assert_eq!(dir.people().await.unwrap().len(), 1);
        assert_eq!(dir.server_name(), "house.org");
    }

    fn requests(users: serde_json::Value) -> Vec<LinkRequest> {
        let users: Vec<User> = serde_json::from_value(users).unwrap();
        link_requests_of(&users, &cfg(), "house.org", &managed())
    }

    /// The groups whose rooms the bot manages, in the link-request tests.
    fn managed() -> BTreeSet<String> {
        ["residents".to_owned(), "kitchen-floor-1".to_owned()].into()
    }

    #[test]
    fn a_link_request_is_an_id_and_a_time_and_anything_else_is_none() {
        let at = |s: i64| DateTime::from_timestamp(s, 0).unwrap();
        let value = |v: serde_json::Value| link_request_value(&v);
        assert_eq!(
            value(serde_json::json!({"id": " @anna:matrix.org ", "at": 1_800_000_000})),
            Some((id("@anna:matrix.org"), at(1_800_000_000)))
        );
        // a whole number written as a float is still one
        assert_eq!(
            value(serde_json::json!({"id": "@anna:matrix.org", "at": 1_800_000_000.0})),
            Some((id("@anna:matrix.org"), at(1_800_000_000)))
        );
        for broken in [
            serde_json::json!("@anna:matrix.org"),
            serde_json::json!(["@anna:matrix.org", 1_800_000_000]),
            serde_json::json!(null),
            serde_json::json!({"id": "@anna:matrix.org"}),
            serde_json::json!({"at": 1_800_000_000}),
            serde_json::json!({"id": "anna", "at": 1_800_000_000}),
            serde_json::json!({"id": 5, "at": 1_800_000_000}),
            serde_json::json!({"id": "@anna:matrix.org", "at": "1800000000"}),
            serde_json::json!({"id": "@anna:matrix.org", "at": 1_800_000_000.5}),
            serde_json::json!({"id": "@anna:matrix.org", "at": 1e300}),
            serde_json::json!({"id": "@anna:matrix.org", "at": i64::MAX}),
        ] {
            assert_eq!(value(broken.clone()), None, "{broken}");
        }
    }

    #[test]
    fn link_requests_of_active_people_with_their_proved_accounts() {
        let r = requests(serde_json::json!([
            {"uuid": "u-anna", "username": "anna", "name": " Anna Schmidt ",
             "groups_obj": [{"name": "residents"}],
             "attributes": {"matrix_verified": ["@anna:example.org", "@anna:house.org"],
                            "matrix_ids": ["@anna:admin.org"],
                            "matrix_link_request": {"id": "@anna:matrix.org", "at": 1_800_000_000}}},
            {"uuid": "u-ben", "username": "ben", "groups_obj": [{"name": "residents"}],
             "attributes": {}},
            {"uuid": "u-cara", "username": "cara", "groups_obj": [{"name": "residents"}],
             "attributes": {"matrix_link_request": "garbage"}},
            {"uuid": "u-old", "username": "old", "is_active": false,
             "groups_obj": [{"name": "residents"}],
             "attributes": {"matrix_link_request": {"id": "@old:matrix.org", "at": 1_800_000_000}}},
            {"uuid": "u-svc", "username": "svc", "type": "service_account",
             "groups_obj": [{"name": "residents"}],
             "attributes": {"matrix_link_request": {"id": "@svc:matrix.org", "at": 1_800_000_000}}},
            {"username": "noid", "groups_obj": [{"name": "residents"}],
             "attributes": {"matrix_link_request": {"id": "@noid:matrix.org", "at": 1_800_000_000}}}
        ]));
        assert_eq!(r.len(), 1, "{r:?}");
        let anna = &r[0];
        assert_eq!(
            (anna.person.as_str(), anna.username.as_str()),
            ("u-anna", "anna")
        );
        assert_eq!(anna.account, id("@anna:matrix.org"));
        // proved links only (not the admin's, never on the house's server)
        assert_eq!(anna.verified, ids(&["@anna:example.org"]));
        assert_eq!(anna.key(), "u-anna @anna:matrix.org 1800000000");
        // another attribute name
        let mut c = cfg();
        c.link_request_attribute = "other".into();
        let users: Vec<User> = serde_json::from_value(serde_json::json!([
            {"uuid": "1", "username": "a", "groups_obj": [{"name": "kitchen-floor-1"}],
             "attributes": {"other": {"id": "@a:matrix.org", "at": 5}}}
        ]))
        .unwrap();
        assert_eq!(
            link_requests_of(&users, &c, "house.org", &managed()).len(),
            1
        );
    }

    #[test]
    fn only_the_people_of_the_managed_groups_may_ask_for_a_link() {
        let ask =
            |n: &str| serde_json::json!({"id": format!("@{n}:matrix.org"), "at": 1_800_000_000});
        let users = serde_json::json!([
            {"uuid": "1", "username": "resident", "groups_obj": [{"name": "residents"}],
             "attributes": {"matrix_link_request": ask("resident")}},
            {"uuid": "2", "username": "cook", "groups_obj": [{"name": "guests"}, {"name": "kitchen-floor-1"}],
             "attributes": {"matrix_link_request": ask("cook")}},
            // a guest and a former resident (still active): the house rooms are not for them
            {"uuid": "3", "username": "guest", "groups_obj": [{"name": "guests"}],
             "attributes": {"matrix_link_request": ask("guest")}},
            {"uuid": "4", "username": "former", "groups_obj": [{"name": "former-residents"}],
             "attributes": {"matrix_link_request": ask("former")}},
            {"uuid": "5", "username": "nogroups", "groups_obj": [],
             "attributes": {"matrix_link_request": ask("nogroups")}},
            // an answer without the groups: nobody to tell who they are
            {"uuid": "6", "username": "unknown",
             "attributes": {"matrix_link_request": ask("unknown")}}
        ]);
        let mut names: Vec<String> = requests(users.clone())
            .into_iter()
            .map(|r| r.username)
            .collect();
        names.sort();
        assert_eq!(names, ["cook", "resident"]);
        // a bot that manages no group serves nobody
        let users: Vec<User> = serde_json::from_value(users).unwrap();
        assert!(link_requests_of(&users, &cfg(), "house.org", &BTreeSet::new()).is_empty());
    }

    #[test]
    fn a_link_request_is_fresh_for_a_slot_and_a_little_ahead() {
        let request = |at: i64| LinkRequest {
            person: "1".into(),
            username: "anna".into(),
            account: id("@anna:matrix.org"),
            at: DateTime::from_timestamp(at, 0).unwrap(),
            verified: Vec::new(),
        };
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let r = request(1_800_000_000);
        assert!(r.is_fresh(now));
        assert!(r.is_fresh(now + Duration::from_secs(1799)));
        assert!(!r.is_fresh(now + Duration::from_secs(1800)));
        assert_eq!(r.until(), now + Duration::from_secs(1800));
        // the provider's clock a little ahead: fine; far ahead: a broken value
        assert!(request(1_800_000_300).is_fresh(now));
        assert!(!request(1_800_000_301).is_fresh(now));
    }

    #[test]
    fn the_link_request_query_filters_when_the_name_allows() {
        assert_eq!(
            link_request_query("matrix_link_request", true),
            "core/users/?is_active=true&include_groups=true\
             &attributes=%7B%22matrix_link_request__isnull%22%3Afalse%7D"
        );
        for name in ["matrix_link_request", "a__b", "a-b", "a.b", ""] {
            let q = link_request_query(name, false);
            assert_eq!(q, "core/users/?is_active=true&include_groups=true");
            if name != "matrix_link_request" {
                assert_eq!(link_request_query(name, true), q, "{name}");
            }
        }
    }

    #[tokio::test]
    async fn link_requests_are_read_with_the_filter_and_without_it_when_refused() {
        use wiremock::matchers::{method, path, query_param, query_param_is_missing};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        std::env::set_var("MXBOT_TEST_REQUESTS_TOKEN", "secret");
        let answer = serde_json::json!({
            "pagination": {"next": 0, "count": 2},
            "results": [
                {"uuid": "1", "username": "anna", "groups_obj": [{"name": "residents"}],
                 "attributes": {"matrix_link_request": {"id": "@anna:matrix.org", "at": 1_800_000_000}}},
                {"uuid": "2", "username": "ben", "groups_obj": [{"name": "residents"}],
                 "attributes": {}}
            ]
        });
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/core/users/"))
            .and(query_param("is_active", "true"))
            .and(query_param("include_groups", "true"))
            .and(query_param(
                "attributes",
                r#"{"matrix_link_request__isnull":false}"#,
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer.clone()))
            .expect(1)
            .mount(&server)
            .await;
        let dir = reader(server.uri(), "MXBOT_TEST_REQUESTS_TOKEN");
        let found = dir.link_requests(&managed()).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].account, id("@anna:matrix.org"));

        // a provider without the filter: refused once, then read without it for good (not
        // the filter again every round)
        let refusing = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param(
                "attributes",
                r#"{"matrix_link_request__isnull":false}"#,
            ))
            .respond_with(ResponseTemplate::new(400))
            .expect(1)
            .mount(&refusing)
            .await;
        Mock::given(method("GET"))
            .and(path("/core/users/"))
            .and(query_param("is_active", "true"))
            .and(query_param_is_missing("attributes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer))
            .expect(3)
            .mount(&refusing)
            .await;
        let dir = reader(refusing.uri(), "MXBOT_TEST_REQUESTS_TOKEN");
        for _ in 0..3 {
            assert_eq!(dir.link_requests(&managed()).await.unwrap().len(), 1);
        }

        // any other refusal is an error (no second read)
        let forbidden = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&forbidden)
            .await;
        let dir = reader(forbidden.uri(), "MXBOT_TEST_REQUESTS_TOKEN");
        let e = dir.link_requests(&managed()).await.unwrap_err();
        assert!(!is_outage(&e), "a refused token is no outage: {e:#}");
        // nobody with a request: an empty answer, no error (unlike a snapshot)
        let none = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"pagination": {"next": 0, "count": 0}, "results": []}),
            ))
            .mount(&none)
            .await;
        let dir = reader(none.uri(), "MXBOT_TEST_REQUESTS_TOKEN");
        assert!(dir.link_requests(&managed()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_outage_is_told_apart_from_a_refusal_or_an_unreadable_answer() {
        use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};
        std::env::set_var("MXBOT_TEST_OUTAGE_TOKEN", "secret");
        let read = |status: u16, body: &'static str| async move {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(&server)
                .await;
            let dir = reader(server.uri(), "MXBOT_TEST_OUTAGE_TOKEN");
            dir.snapshot(&BTreeSet::new()).await.unwrap_err()
        };
        for status in [500, 502, 503, 504, 429] {
            let e = read(status, "").await;
            assert!(is_outage(&e), "{status}: {e:#}");
        }
        for status in [400, 401, 403, 404] {
            let e = read(status, "").await;
            assert!(!is_outage(&e), "{status}: {e:#}");
        }
        // a schema the bot does not know, or a read it finds wrong itself: a bug, no outage
        let e = read(200, r#"{"users": []}"#).await;
        assert!(!is_outage(&e), "{e:#}");
        let e = read(
            200,
            r#"{"pagination": {"next": 0, "count": 0}, "results": []}"#,
        )
        .await;
        assert!(!is_outage(&e), "{e:#}");
        // nobody listening (a port just freed): an outage
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let e = reader(
            format!("http://127.0.0.1:{port}"),
            "MXBOT_TEST_OUTAGE_TOKEN",
        )
        .snapshot(&BTreeSet::new())
        .await
        .unwrap_err();
        assert!(is_outage(&e), "{e:#}");
        // a timeout: an outage
        let slow = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
            .mount(&slow)
            .await;
        let mut dir = reader(slow.uri(), "MXBOT_TEST_OUTAGE_TOKEN");
        dir.http = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let e = dir.snapshot(&BTreeSet::new()).await.unwrap_err();
        assert!(is_outage(&e), "{e:#}");
        // an error without anything from HTTP in it
        assert!(!is_outage(&anyhow::anyhow!("the directory names nobody")));
    }

    #[test]
    fn claimants_name_every_active_person_claiming_an_account() {
        let p = index(serde_json::json!([
            {"uuid": "1", "username": "anna", "groups_obj": [],
             "attributes": {"matrix_verified": ["@shared:matrix.org"]}},
            {"uuid": "2", "username": "ben", "groups_obj": [],
             "attributes": {"matrix_ids": ["@shared:matrix.org", "@ben:matrix.org"]}},
            {"uuid": "3", "username": "gone", "is_active": false, "groups_obj": [],
             "attributes": {"matrix_ids": ["@gone:matrix.org"]}}
        ]));
        let names = |a: &str| {
            let mut n: Vec<&str> = p.claimants(&id(a)).map(|p| p.id.as_str()).collect();
            n.sort_unstable();
            n
        };
        assert_eq!(names("@shared:matrix.org"), ["1", "2"]);
        assert_eq!(names("@ben:matrix.org"), ["2"]);
        assert!(names("@gone:matrix.org").is_empty());
        assert!(names("@nobody:matrix.org").is_empty());
    }
}
