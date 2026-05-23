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

/// Matrix credentials and homeserver. Identical in all six bots.
///
/// In each bot's config.toml this maps to the `[matrix]` table.
#[derive(Deserialize, Clone)]
pub struct MatrixConfig {
    pub homeserver:   String,
    pub user_id:      String,
    pub access_token: String,
    pub device_id:    String,
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
            EncryptionStrategy::AllDevices    => CollectStrategy::AllDevices,
            EncryptionStrategy::IdentityBased => CollectStrategy::IdentityBasedStrategy,
            EncryptionStrategy::OnlyTrusted   => CollectStrategy::OnlyTrustedDevices,
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
}
