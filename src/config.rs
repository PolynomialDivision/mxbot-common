//! Shared configuration: the `[matrix]` and `[security]` tables every bot
//! has, allow-lists, and helpers for locating/loading the config file.
//!
//! Secrets (access token, recovery key) live in the config file or in
//! environment variables, never in the runtime settings that admins can
//! change over chat (see [`crate::settings`]).

use std::{
    collections::HashSet,
    hash::Hash,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{bail, Context, Result};
use matrix_sdk::ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};
use matrix_sdk_crypto::CollectStrategy;
use serde::{de::DeserializeOwned, Deserialize, Deserializer};
use tracing::warn;

/// Matrix credentials and homeserver: the `[matrix]` table.
///
/// `access_token` and `recovery_key` can be supplied (or overridden) through
/// the `MATRIX_ACCESS_TOKEN` and `MATRIX_RECOVERY_KEY` environment variables
/// so they need not be written into the config file.
#[derive(Deserialize, Clone)]
pub struct MatrixConfig {
    pub homeserver: String,
    pub user_id: String,
    #[serde(default)]
    pub access_token: String,
    pub device_id: String,
    /// Recovery key from Element's "Set up Secure Backup".
    /// Used once at startup to restore cross-signing keys.
    pub recovery_key: Option<String>,
}

impl MatrixConfig {
    /// Apply `MATRIX_ACCESS_TOKEN` / `MATRIX_RECOVERY_KEY` overrides.
    ///
    /// Only applies to the bot's primary account; secondary accounts (e.g.
    /// upload peers) keep what their own config table says.
    pub fn apply_env_overrides(&mut self) {
        if let Some(token) = non_empty_env("MATRIX_ACCESS_TOKEN") {
            self.access_token = token;
        }
        if let Some(key) = non_empty_env("MATRIX_RECOVERY_KEY") {
            self.recovery_key = Some(key);
        }
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Which devices receive room keys (`[security] encryption_strategy`).
#[derive(Deserialize, Default, Clone, Copy, Debug)]
#[serde(rename_all = "snake_case")]
pub enum EncryptionStrategy {
    AllDevices,
    #[default]
    IdentityBased,
    OnlyTrusted,
}

impl From<EncryptionStrategy> for CollectStrategy {
    fn from(s: EncryptionStrategy) -> Self {
        match s {
            EncryptionStrategy::AllDevices => CollectStrategy::AllDevices,
            EncryptionStrategy::IdentityBased => CollectStrategy::IdentityBasedStrategy,
            EncryptionStrategy::OnlyTrusted => CollectStrategy::OnlyTrustedDevices,
        }
    }
}

/// Whose devices count (`[security] trust`), see [`crate::trust`].
#[derive(Deserialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    /// Verified with the bot (`!admin verify`). A new identity (someone
    /// reset their keys) stops everything until they verify again.
    #[default]
    Verified,
    /// Every device a person signed with their current identity. A new
    /// identity is accepted (and logged); a device they never signed still
    /// counts for nothing.
    CrossSigned,
}

/// An allow-list written as either the string `"all"` or a list of IDs.
///
/// Only an explicit `"all"` allows everything; an empty list (`[]`)
/// allows nothing. An omitted list is [`AllowList::Unset`]: it allows
/// nothing, and [`SecurityConfig::validate`] (run by `Bot::start`) refuses
/// it, so a forgotten entry never opens the bot up.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AllowList<T: Eq + Hash> {
    /// Not configured.
    #[default]
    Unset,
    All,
    Only(HashSet<T>),
}

pub type UserAllowList = AllowList<OwnedUserId>;
pub type RoomAllowList = AllowList<OwnedRoomId>;

impl<T: Eq + Hash> AllowList<T> {
    pub fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }

    /// Allows nobody: an empty list, or not configured.
    pub fn is_none(&self) -> bool {
        matches!(self, Self::Unset) || matches!(self, Self::Only(set) if set.is_empty())
    }

    /// Configured at all (`"all"` or a list, also an empty one).
    pub fn is_set(&self) -> bool {
        !matches!(self, Self::Unset)
    }

    /// The explicit entries, empty for `"all"`.
    pub fn entries(&self) -> impl Iterator<Item = &T> {
        let set = match self {
            Self::Unset | Self::All => None,
            Self::Only(set) => Some(set),
        };
        set.into_iter().flatten()
    }

    pub fn describe(&self) -> String
    where
        T: std::fmt::Display,
    {
        match self {
            Self::Unset => "not set (nobody)".to_owned(),
            Self::All => "everyone".to_owned(),
            Self::Only(set) if set.is_empty() => "nobody".to_owned(),
            Self::Only(set) => {
                let mut entries: Vec<String> = set.iter().map(ToString::to_string).collect();
                entries.sort();
                entries.join(", ")
            }
        }
    }
}

impl UserAllowList {
    pub fn allows(&self, user_id: &UserId) -> bool {
        match self {
            Self::Unset => false,
            Self::All => true,
            Self::Only(set) => set.contains(user_id),
        }
    }
}

impl RoomAllowList {
    pub fn allows(&self, room_id: &RoomId) -> bool {
        match self {
            Self::Unset => false,
            Self::All => true,
            Self::Only(set) => set.contains(room_id),
        }
    }
}

impl<'de, T> Deserialize<'de> for AllowList<T>
where
    T: Eq + Hash + FromStr,
    T::Err: std::fmt::Display,
{
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Keyword(String),
            List(Vec<String>),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Keyword(keyword) if keyword == "all" => Ok(Self::All),
            Raw::Keyword(other) => Err(serde::de::Error::custom(format!(
                "expected \"all\" or a list of Matrix IDs, got {other:?}"
            ))),
            Raw::List(list) => list
                .iter()
                .map(|entry| {
                    entry.parse().map_err(|error| {
                        serde::de::Error::custom(format!("invalid Matrix ID {entry:?}: {error}"))
                    })
                })
                .collect::<Result<HashSet<T>, _>>()
                .map(Self::Only),
        }
    }
}

fn default_verification_flow_timeout_secs() -> u64 {
    300
}

fn default_verification_grant_ttl_secs() -> u64 {
    600
}

fn default_verification_max_concurrent() -> usize {
    8
}

fn default_true() -> bool {
    true
}

/// The `[security]` table shared by all bots.
#[derive(Deserialize, Default, Clone)]
pub struct SecurityConfig {
    /// Users allowed to invite the bot into rooms (`"all"` or a list).
    /// Configured admins may always invite the bot into a direct chat.
    /// Required: an omitted entry stops the bot at startup.
    #[serde(default)]
    pub allowed_inviters: UserAllowList,
    /// Rooms the bot is allowed to join (`"all"` or a list). Direct chats
    /// opened by an admin are always allowed. Required like
    /// `allowed_inviters`.
    #[serde(default)]
    pub allowed_rooms: RoomAllowList,
    /// Rooms whose members may open a private chat with the bot: a direct
    /// chat invite from someone joined to one of these rooms right now is
    /// accepted, and the bot leaves again unless the chat turns out to be
    /// just the two of them, encrypted.
    #[serde(default)]
    pub dm_rooms: Vec<String>,
    /// Users allowed to run administrative commands.
    #[serde(default)]
    pub admin_users: Vec<String>,
    /// Directory groups whose members' house accounts are admins too, for bots that read the
    /// directory and follow it ([`crate::admin::Admins::follow`]); linked accounts never are.
    #[serde(default)]
    pub admin_groups: Vec<String>,
    #[serde(default)]
    pub encryption_strategy: EncryptionStrategy,
    #[serde(default)]
    pub trust: Trust,
    #[serde(default)]
    pub verification: VerificationConfig,
    #[serde(default)]
    pub admin_dm: AdminDmConfig,
}

impl SecurityConfig {
    /// Refuse a configuration that would silently open the bot up: both
    /// allow-lists must be set (to a list or to `"all"`), and `dm_rooms`
    /// must hold valid room IDs.
    pub fn validate(&self) -> Result<()> {
        if !self.allowed_inviters.is_set() {
            bail!(
                "[security] allowed_inviters is not set: list the users who may invite the bot \
                 (admins always may), [] for nobody, or \"all\" for everyone"
            );
        }
        if !self.allowed_rooms.is_set() {
            bail!(
                "[security] allowed_rooms is not set: list the room IDs the bot may join, \
                 [] for none, or \"all\" to allow every room"
            );
        }
        self.dm_rooms()?;
        Ok(())
    }

    /// `dm_rooms`, parsed.
    pub fn dm_rooms(&self) -> Result<Vec<OwnedRoomId>> {
        self.dm_rooms
            .iter()
            .map(|room| {
                room.parse()
                    .with_context(|| format!("Invalid room ID in dm_rooms: {room:?}"))
            })
            .collect()
    }

    /// Parse `admin_users`; invalid entries are an error rather than being
    /// silently dropped, since they gate privileged access.
    pub fn admins(&self) -> Result<HashSet<OwnedUserId>> {
        self.admin_users
            .iter()
            .map(|user| {
                user.parse()
                    .with_context(|| format!("Invalid Matrix user ID in admin_users: {user:?}"))
            })
            .collect()
    }
}

/// Verification policy: `[security.verification]`.
#[derive(Deserialize, Clone, Debug)]
pub struct VerificationConfig {
    /// Matrix users allowed to verify with the bot. Empty falls back to an
    /// explicit `allowed_inviters` list. Admins are always allowed.
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// Allow first-time TOFU verification for users who currently share at
    /// least one joined room with the bot.
    #[serde(default)]
    pub allow_users_from_joined_rooms: bool,
    /// Maximum lifetime of one verification flow.
    #[serde(default = "default_verification_flow_timeout_secs")]
    pub flow_timeout_secs: u64,
    /// Lifetime of a one-shot administrative re-verification grant.
    #[serde(default = "default_verification_grant_ttl_secs")]
    pub grant_ttl_secs: u64,
    /// Maximum number of verification flows handled at once.
    #[serde(default = "default_verification_max_concurrent")]
    pub max_concurrent: usize,
}

impl Default for VerificationConfig {
    fn default() -> Self {
        Self {
            allowed_users: Vec::new(),
            allow_users_from_joined_rooms: false,
            flow_timeout_secs: default_verification_flow_timeout_secs(),
            grant_ttl_secs: default_verification_grant_ttl_secs(),
            max_concurrent: default_verification_max_concurrent(),
        }
    }
}

/// Admin direct-chat policy: `[security.admin_dm]`.
#[derive(Deserialize, Clone, Debug)]
pub struct AdminDmConfig {
    /// Accept admin commands in direct chats with configured admins.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Only accept commands in end-to-end encrypted direct chats.
    #[serde(default = "default_true")]
    pub require_encryption: bool,
    /// Only accept commands sent from a device that counts under
    /// `[security] trust`: cross-signed by an identity the bot verified
    /// (`verified`, after `!admin verify`), or by the admin's current
    /// identity (`cross_signed`).
    #[serde(default)]
    pub require_verified_device: bool,
    /// Also deliver the bot's broadcast posts (digests, reminders, …) into
    /// admin direct chats.
    #[serde(default)]
    pub receive_broadcasts: bool,
}

impl Default for AdminDmConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            require_encryption: true,
            require_verified_device: false,
            receive_broadcasts: false,
        }
    }
}

/// Config file path: the first `*.toml` argument, else `config.toml`.
pub fn config_path_from_args() -> PathBuf {
    std::env::args()
        .skip(1)
        .find(|arg| arg.ends_with(".toml"))
        .unwrap_or_else(|| "config.toml".to_owned())
        .into()
}

/// Store directory: `$STORE_PATH`, else `store`.
pub fn store_path_from_env() -> PathBuf {
    std::env::var("STORE_PATH")
        .unwrap_or_else(|_| "store".to_owned())
        .into()
}

/// Environment variable: set (to anything but `0` or empty), [`load_toml`] refuses a config
/// with keys the bot does not know, instead of warning about them.
pub const STRICT_CONFIG_ENV: &str = "MXBOT_STRICT_CONFIG";

/// Read and parse a TOML config file.
///
/// Keys the bot does not know (a typo such as `require_verfied_device`, which would silently
/// keep the default) are logged as warnings, or refused with [`STRICT_CONFIG_ENV`] set.
/// Sections that deny unknown fields still refuse them as before; a section with a
/// `#[serde(flatten)]` field cannot tell its unknown keys apart and says nothing about them.
pub fn load_toml<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Reading config {}", path.display()))?;
    let (value, unknown) =
        parse_toml(&raw).with_context(|| format!("Parsing config {}", path.display()))?;
    if unknown.is_empty() {
        return Ok(value);
    }
    let strict = std::env::var(STRICT_CONFIG_ENV).is_ok_and(|v| !v.is_empty() && v != "0");
    if strict {
        anyhow::bail!(
            "Config {} has unknown keys: {} ({STRICT_CONFIG_ENV} is set)",
            path.display(),
            unknown.join(", ")
        );
    }
    for key in &unknown {
        // Some bots read their config before logging starts: then on stderr.
        if tracing::dispatcher::has_been_set() {
            warn!(config = %path.display(), key, "Unknown config key (a typo?); ignored");
        } else {
            eprintln!(
                "Unknown config key in {} (a typo?), ignored: {key}",
                path.display()
            );
        }
    }
    Ok(value)
}

/// Parse TOML into `T`, with the dotted paths of the keys `T` ignored.
fn parse_toml<T: DeserializeOwned>(raw: &str) -> Result<(T, Vec<String>)> {
    let mut unknown = Vec::new();
    let deserializer = toml::Deserializer::parse(raw)?;
    let value = serde_ignored::deserialize(deserializer, |key| unknown.push(key.to_string()))?;
    Ok((value, unknown))
}

/// Parse a list of user IDs, warning about (and skipping) invalid entries.
pub fn parse_user_ids(field: &str, users: &[String]) -> HashSet<OwnedUserId> {
    users
        .iter()
        .filter_map(|user| match user.parse() {
            Ok(user_id) => Some(user_id),
            Err(error) => {
                warn!(field, configured_user_id = user, %error, "Ignoring invalid Matrix user ID");
                None
            }
        })
        .collect()
}

/// A wall-clock time written as `"HH:MM"` in config and settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimeOfDay {
    pub hour: u32,
    pub minute: u32,
}

impl TimeOfDay {
    pub fn new(hour: u32, minute: u32) -> Self {
        assert!(hour < 24 && minute < 60, "invalid time of day");
        Self { hour, minute }
    }
}

impl FromStr for TimeOfDay {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (hour, minute) = parse_hh_mm(value)?;
        Ok(Self { hour, minute })
    }
}

impl std::fmt::Display for TimeOfDay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

impl serde::Serialize for TimeOfDay {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for TimeOfDay {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Parse `HH:MM` (00:00–23:59).
pub fn parse_hh_mm(value: &str) -> Result<(u32, u32)> {
    let Some((hour, minute)) = value.trim().split_once(':') else {
        bail!("invalid time {value:?} (expected HH:MM)");
    };
    let hour: u32 = hour
        .parse()
        .with_context(|| format!("invalid hour in {value:?}"))?;
    let minute: u32 = minute
        .parse()
        .with_context(|| format!("invalid minute in {value:?}"))?;
    if hour > 23 || minute > 59 {
        bail!("invalid time {value:?} (expected 00:00-23:59)");
    }
    Ok((hour, minute))
}

#[cfg(test)]
mod tests {
    /// A section with `#[serde(flatten)]` hands its leftovers to the flattened part, so its
    /// unknown keys go unnoticed: no false alarm, but no warning either.
    #[test]
    fn flattened_sections_report_no_known_key() {
        #[derive(serde::Deserialize, Debug)]
        struct Inner {
            allowed: Vec<String>,
        }
        #[derive(serde::Deserialize, Debug)]
        struct Outer {
            #[serde(flatten)]
            inner: Inner,
            own: bool,
        }
        #[derive(serde::Deserialize, Debug)]
        struct Conf {
            security: Outer,
        }
        let (conf, unknown) =
            super::parse_toml::<Conf>("[security]\nallowed = [\"a\"]\nown = true\ntypo = 1\n")
                .unwrap();
        assert!(conf.security.own && conf.security.inner.allowed == ["a"]);
        assert!(unknown.is_empty(), "{unknown:?}");
    }

    #[test]
    fn unknown_keys_are_named_known_ones_parse() {
        #[derive(serde::Deserialize, Debug)]
        struct Sec {
            #[serde(default)]
            require_verified_device: bool,
        }
        #[derive(serde::Deserialize, Debug)]
        struct Conf {
            name: String,
            security: Sec,
        }
        let (conf, unknown) = super::parse_toml::<Conf>(
            "name = \"x\"\nextra = 1\n[security]\nrequire_verfied_device = true\n",
        )
        .unwrap();
        assert_eq!(conf.name, "x");
        assert!(
            !conf.security.require_verified_device,
            "the typo keeps the default"
        );
        assert_eq!(unknown, vec!["extra", "security.require_verfied_device"]);
    }

    #[test]
    fn omitted_allow_lists_are_unset_and_refused() {
        let unset: SecurityConfig = toml::from_str("").unwrap();
        assert!(!unset.allowed_rooms.is_set() && unset.allowed_rooms.is_none());
        assert!(unset.validate().is_err());
        let one: SecurityConfig = toml::from_str("allowed_inviters = []").unwrap();
        assert!(one
            .validate()
            .unwrap_err()
            .to_string()
            .contains("allowed_rooms"));
        let ok: SecurityConfig =
            toml::from_str("allowed_inviters = []\nallowed_rooms = \"all\"").unwrap();
        assert!(ok.validate().is_ok() && ok.allowed_rooms.is_all());
        let bad_dm: SecurityConfig = toml::from_str(
            "allowed_inviters = []\nallowed_rooms = []\ndm_rooms = [\"not a room\"]",
        )
        .unwrap();
        assert!(bad_dm.validate().is_err());
        let dm: SecurityConfig = toml::from_str(
            "allowed_inviters = []\nallowed_rooms = []\ndm_rooms = [\"!house:example.org\"]",
        )
        .unwrap();
        assert_eq!(dm.dm_rooms().unwrap().len(), 1);
    }

    use super::*;
    use matrix_sdk::ruma::user_id;

    #[derive(Deserialize)]
    struct Wrapper {
        #[serde(default)]
        security: SecurityConfig,
    }

    #[test]
    fn shared_room_verification_is_opt_in() {
        let default: VerificationConfig = toml::from_str("").unwrap();
        assert!(!default.allow_users_from_joined_rooms);

        let enabled: VerificationConfig =
            toml::from_str("allow_users_from_joined_rooms = true").unwrap();
        assert!(enabled.allow_users_from_joined_rooms);
    }

    #[test]
    fn allow_lists_accept_all_keyword_lists_and_omitted_allows_nobody() {
        let omitted: Wrapper = toml::from_str("").unwrap();
        assert!(!omitted.security.allowed_inviters.is_all());
        assert!(!omitted
            .security
            .allowed_inviters
            .allows(user_id!("@alice:example.org")));
        assert!(!omitted.security.allowed_rooms.is_set());

        let all: Wrapper = toml::from_str("[security]\nallowed_inviters = \"all\"").unwrap();
        assert!(all.security.allowed_inviters.is_all());

        let list: Wrapper =
            toml::from_str("[security]\nallowed_inviters = [\"@alice:example.org\"]").unwrap();
        assert!(list
            .security
            .allowed_inviters
            .allows(user_id!("@alice:example.org")));
        assert!(!list
            .security
            .allowed_inviters
            .allows(user_id!("@bob:example.org")));

        let none: Wrapper = toml::from_str("[security]\nallowed_inviters = []").unwrap();
        assert!(none.security.allowed_inviters.is_none());
        assert!(!none
            .security
            .allowed_inviters
            .allows(user_id!("@alice:example.org")));
    }

    #[test]
    fn allow_lists_reject_garbage() {
        assert!(toml::from_str::<Wrapper>("[security]\nallowed_inviters = \"some\"").is_err());
        assert!(toml::from_str::<Wrapper>("[security]\nallowed_rooms = [\"not-a-room\"]").is_err());
    }

    #[test]
    fn trust_stays_verified_unless_cross_signed_is_chosen() {
        let default: SecurityConfig = toml::from_str("").unwrap();
        assert_eq!(default.trust, Trust::Verified);
        let chosen: SecurityConfig = toml::from_str("trust = \"cross_signed\"").unwrap();
        assert_eq!(chosen.trust, Trust::CrossSigned);
        assert!(toml::from_str::<SecurityConfig>("trust = \"anyone\"").is_err());
    }

    #[test]
    fn admin_dm_defaults_are_restrictive() {
        let cfg = AdminDmConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.require_encryption);
        assert!(!cfg.receive_broadcasts);
    }

    #[test]
    fn invalid_admin_ids_are_errors() {
        let cfg: Wrapper = toml::from_str("[security]\nadmin_users = [\"nope\"]").unwrap();
        assert!(cfg.security.admins().is_err());
    }

    #[test]
    fn time_of_day_round_trips_through_serde() {
        let t: TimeOfDay = serde_json::from_str("\"07:30\"").unwrap();
        assert_eq!(t, TimeOfDay::new(7, 30));
        assert_eq!(serde_json::to_string(&t).unwrap(), "\"07:30\"");
        assert!(serde_json::from_str::<TimeOfDay>("\"7pm\"").is_err());
    }

    #[test]
    fn hh_mm_parsing() {
        assert_eq!(parse_hh_mm("07:05").unwrap(), (7, 5));
        assert!(parse_hh_mm("24:00").is_err());
        assert!(parse_hh_mm("7").is_err());
    }
}
