//! Shared Matrix configuration structs.
//!
//! These are identical across radar-bot, calendar-bot, translate-bot, and
//! garbage-bot. The telegram-mirror-bot and email-bot have diverged
//! SecurityConfig variants (different allow-list types, extra fields) and
//! should not use SecurityConfig from this crate.
//!
//! All bots can use MatrixConfig and EncryptionStrategy regardless.

use matrix_sdk_crypto::CollectStrategy;
use serde::Deserialize;

fn default_verification_flow_timeout_secs() -> u64 {
    300
}

fn default_verification_grant_ttl_secs() -> u64 {
    600
}

fn default_verification_max_concurrent() -> usize {
    8
}

/// Matrix credentials and homeserver. Identical in all six bots.
///
/// In each bot's config.toml this maps to the `[matrix]` table.
#[derive(Deserialize, Clone)]
pub struct MatrixConfig {
    pub homeserver: String,
    pub user_id: String,
    pub access_token: String,
    pub device_id: String,
    /// Recovery key from Element's "Set up Secure Backup".
    /// Used once at startup to restore cross-signing keys.
    pub recovery_key: Option<String>,
}

/// Which devices receive room keys. Maps to the `encryption_strategy` field
/// inside `[security]` in config.toml.
#[derive(Deserialize, Default, Clone, Debug)]
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

/// Standard security config used by the four bots with a simple allow-list.
/// Not suitable for telegram-mirror-bot or email-bot (they have additional
/// fields and a different allow-list representation).
///
/// Maps to the `[security]` table in config.toml.
#[derive(Deserialize, Default, Clone)]
pub struct SecurityConfig {
    /// Users allowed to invite the bot to rooms. Empty = accept from anyone.
    #[serde(default)]
    pub allowed_inviters: Vec<String>,
    /// Users allowed to run !reset-trust. Empty = command disabled.
    #[serde(default)]
    pub admin_users: Vec<String>,
    #[serde(default)]
    pub encryption_strategy: EncryptionStrategy,
    #[serde(default)]
    pub verification: VerificationConfig,
}

/// Shared verification policy for bots using [`SecurityConfig`].
#[derive(Deserialize, Clone, Debug)]
pub struct VerificationConfig {
    /// Matrix users allowed to verify with the bot. Empty lets each bot use its
    /// inviter allow-list as a compatibility fallback.
    #[serde(default)]
    pub allowed_users: Vec<String>,
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
            flow_timeout_secs: default_verification_flow_timeout_secs(),
            grant_ttl_secs: default_verification_grant_ttl_secs(),
            max_concurrent: default_verification_max_concurrent(),
        }
    }
}
