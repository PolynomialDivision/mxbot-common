//! Admin console: privileged commands in direct chats with configured admins.
//!
//! Admins (`[security] admin_users`) can open a direct chat with any bot and
//! manage it there instead of in shared rooms:
//!
//! ```text
//! !admin                      help
//! !admin status               bot, session and trust status
//! !admin verify               start mutual (SAS) verification with you
//! !admin settings             runtime settings and their values
//! !admin set <key> <value>    change a runtime setting (persisted)
//! !admin unset <key>          revert a setting to the config file value
//! !reset-trust <user>         allow <user> one re-verification
//! !verify-device <user> <dev> verify a specific device of <user>
//! ```
//!
//! Access requires, in this order: the sender is a configured admin; the
//! room is the bot's direct chat with exactly that admin; the message was
//! end-to-end encrypted (unless `require_encryption = false`); and — if
//! `require_verified_device = true` — it was sent from a device the bot has
//! verified. `!reset-trust` / `!verify-device` keep working silently in
//! shared rooms for compatibility.
//!
//! Bot-specific commands sent by an admin in such a chat are reported as
//! [`Dispatch::AdminDm`] so the bot can run them and answer privately.

use std::{collections::HashSet, sync::Arc, time::Instant};

use matrix_sdk::{
    deserialized_responses::{EncryptionInfo, VerificationState},
    ruma::{
        events::room::message::{
            MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
        },
        OwnedDeviceId, OwnedUserId, UserId,
    },
    Client, Room,
};
use tracing::{info, warn};

use crate::{
    config::AdminDmConfig, rooms::admin_dm_partner, settings::SettingsStore,
    verify::VerificationService,
};

/// What the bot should do with a message after the console saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dispatch {
    /// A console command; it was answered or deliberately ignored.
    Handled,
    /// A `!command` from an authorized admin in their direct chat that the
    /// console does not know: run it as a (bot-specific) admin command and
    /// reply in this room.
    AdminDm,
    /// Not for the console: process normally.
    Continue,
}

/// Whether a message may use the admin console.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Access {
    /// Not an admin direct chat (or not from its admin).
    None,
    Denied(&'static str),
    Granted,
}

struct Inner {
    client: Client,
    bot_name: String,
    version: String,
    admins: Arc<HashSet<OwnedUserId>>,
    policy: AdminDmConfig,
    verification: VerificationService,
    settings: Option<SettingsStore>,
    help: Option<String>,
    started: Instant,
}

/// Shared admin command handling; cheap to clone.
#[derive(Clone)]
pub struct AdminConsole {
    inner: Arc<Inner>,
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
    Status,
    Verify,
    Settings,
    Set(String, String),
    Unset(String),
    ResetTrust(OwnedUserId),
    VerifyDevice(OwnedUserId, OwnedDeviceId),
    Invalid(&'static str),
}

impl AdminConsole {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        client: Client,
        bot_name: String,
        version: String,
        admins: Arc<HashSet<OwnedUserId>>,
        policy: AdminDmConfig,
        verification: VerificationService,
        settings: Option<SettingsStore>,
        help: Option<String>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                client,
                bot_name,
                version,
                admins,
                policy,
                verification,
                settings,
                help,
                started: Instant::now(),
            }),
        }
    }

    pub fn is_admin(&self, user_id: &UserId) -> bool {
        self.inner.admins.contains(user_id)
    }

    pub fn admins(&self) -> &HashSet<OwnedUserId> {
        &self.inner.admins
    }

    /// Whether `room` is a direct chat with one of the admins.
    pub fn is_admin_dm(&self, room: &Room) -> bool {
        admin_dm_partner(room, &self.inner.admins).is_some()
    }

    fn access(&self, room: &Room, sender: &UserId, encryption: Option<&EncryptionInfo>) -> Access {
        if !self.inner.policy.enabled || !self.is_admin(sender) {
            return Access::None;
        }
        if admin_dm_partner(room, &self.inner.admins).as_deref() != Some(sender) {
            return Access::None;
        }
        if self.inner.policy.require_encryption
            && (encryption.is_none() || !room.encryption_state().is_encrypted())
        {
            return Access::Denied(
                "admin commands need an end-to-end encrypted chat — enable encryption here or start a new encrypted direct chat",
            );
        }
        if self.inner.policy.require_verified_device
            && !encryption
                .is_some_and(|info| info.verification_state == VerificationState::Verified)
        {
            return Access::Denied(
                "admin commands need a verified device — verify this session with the bot (!admin verify) or with your other sessions",
            );
        }
        Access::Granted
    }

    /// Inspect a room message. Call this first in the bot's message handler
    /// and stop when it returns [`Dispatch::Handled`].
    pub async fn handle(
        &self,
        room: &Room,
        event: &OriginalSyncRoomMessageEvent,
        encryption: Option<&EncryptionInfo>,
    ) -> Dispatch {
        let MessageType::Text(text) = &event.content.msgtype else {
            return Dispatch::Continue;
        };
        let body = text.body.trim();
        if !body.starts_with('!') || self.inner.client.user_id() == Some(event.sender.as_ref()) {
            return Dispatch::Continue;
        }
        let sender = event.sender.as_ref();
        let command = parse(body);

        match self.access(room, sender, encryption) {
            Access::Granted => match command {
                Some(command) => {
                    let reply = self.run(sender, command).await;
                    self.reply(room, reply).await;
                    Dispatch::Handled
                }
                None => Dispatch::AdminDm,
            },
            Access::Denied(reason) => {
                warn!(user_id = %sender, room_id = %room.room_id(), reason, "Refusing admin command");
                self.reply(room, format!("⛔ {reason}.")).await;
                Dispatch::Handled
            }
            Access::None => match command {
                None => Dispatch::Continue,
                Some(command @ (Command::ResetTrust(_) | Command::VerifyDevice(..))) => {
                    // Shared rooms: legacy, silent handling.
                    if self.is_admin(sender) {
                        let outcome = self.run(sender, command).await;
                        info!(admin_user_id = %sender, room_id = %room.room_id(), outcome, "Handled verification command");
                    } else {
                        warn!(user_id = %sender, "Ignoring verification command from non-administrator");
                    }
                    Dispatch::Handled
                }
                Some(_) => {
                    if self.is_admin(sender) {
                        self.reply(
                            room,
                            "🔒 Admin commands are only available in a direct chat with me."
                                .to_owned(),
                        )
                        .await;
                    }
                    Dispatch::Handled
                }
            },
        }
    }

    async fn reply(&self, room: &Room, text: String) {
        if let Err(error) =
            crate::send::send_with_retry(room, RoomMessageEventContent::notice_plain(text)).await
        {
            warn!(room_id = %room.room_id(), %error, "Failed to send admin reply");
        }
    }

    async fn run(&self, sender: &UserId, command: Command) -> String {
        let verification = &self.inner.verification;
        match command {
            Command::Help => self.help(),
            Command::Status => self.status(sender).await,
            Command::Verify => match verification.trust_summary(sender).await {
                "verified" => "✅ You and I are already verified. To re-verify (e.g. after resetting your identity) run `!reset-trust` for yourself first.".to_owned(),
                _ => match verification.request_user_verification(sender).await {
                    Ok(()) => "🔐 Verification request sent — accept it in your client and compare the emojis.".to_owned(),
                    Err(error) => format!("❌ Could not start verification: {error}"),
                },
            },
            Command::Settings => match &self.inner.settings {
                Some(settings) => {
                    let described = settings.describe();
                    if described.is_empty() {
                        "This bot has no runtime settings.".to_owned()
                    } else {
                        format!("Runtime settings:\n{described}")
                    }
                }
                None => "This bot has no runtime settings.".to_owned(),
            },
            Command::Set(key, value) => match &self.inner.settings {
                Some(settings) => match settings.set(&key, &value).await {
                    Ok(value) => {
                        info!(admin_user_id = %sender, key, value, "Runtime setting changed");
                        format!("✅ {key} = {value}")
                    }
                    Err(error) => format!("❌ {error}"),
                },
                None => "This bot has no runtime settings.".to_owned(),
            },
            Command::Unset(key) => match &self.inner.settings {
                Some(settings) => match settings.unset(&key).await {
                    Ok(value) => {
                        info!(admin_user_id = %sender, key, "Runtime setting reset");
                        format!("✅ {key} reverted to the configured value ({value})")
                    }
                    Err(error) => format!("❌ {error}"),
                },
                None => "This bot has no runtime settings.".to_owned(),
            },
            Command::ResetTrust(user_id) => {
                verification.grant_user(user_id.clone()).await;
                info!(admin_user_id = %sender, target_user_id = %user_id, "Created one-shot verification grant");
                format!("✅ {user_id} may verify with me once in the next few minutes.")
            }
            Command::VerifyDevice(user_id, device_id) => {
                verification.grant_device(user_id.clone(), device_id.clone()).await;
                match verification.request_device_verification(&user_id, &device_id).await {
                    Ok(()) => {
                        info!(admin_user_id = %sender, target_user_id = %user_id, %device_id, "Started administrator-approved verification");
                        format!("🔐 Verification request sent to {user_id} ({device_id}).")
                    }
                    Err(error) => {
                        warn!(admin_user_id = %sender, target_user_id = %user_id, %device_id, %error, "Could not start administrator-approved verification");
                        format!("❌ Could not start verification: {error}")
                    }
                }
            }
            Command::Invalid(reason) => format!("❌ {reason}. Send !admin for help."),
        }
    }

    fn help(&self) -> String {
        let mut text = format!(
            "{} admin commands:\n\
             !admin status — bot, session and trust status\n\
             !admin verify — verify each other (emoji SAS)\n\
             !admin settings — show runtime settings\n\
             !admin set <key> <value> — change a runtime setting\n\
             !admin unset <key> — revert a setting to the config file\n\
             !reset-trust <user> — allow one re-verification of <user>\n\
             !verify-device <user> <device> — verify a specific device",
            self.inner.bot_name
        );
        if let Some(help) = &self.inner.help {
            text.push_str("\n\nBot commands (also usable here):\n");
            text.push_str(help);
        }
        text
    }

    async fn status(&self, sender: &UserId) -> String {
        let client = &self.inner.client;
        let uptime = self.inner.started.elapsed().as_secs();
        let cross_signing = match client.encryption().cross_signing_status().await {
            Some(s) if s.has_master && s.has_self_signing && s.has_user_signing => "complete",
            Some(_) => "incomplete",
            None => "unknown",
        };
        let own_device_verified = match client.encryption().get_own_device().await {
            Ok(Some(device)) => device.is_cross_signed_by_owner(),
            _ => false,
        };
        format!(
            "{} {}\n\
             Account: {} (device {})\n\
             Uptime: {}h {:02}m\n\
             Joined rooms: {}\n\
             Cross-signing: {cross_signing}, this device {}\n\
             You: {}\n\
             Admins: {}\n\
             Admin chats: encryption {}, verified device {}",
            self.inner.bot_name,
            self.inner.version,
            client
                .user_id()
                .map(ToString::to_string)
                .unwrap_or_default(),
            client
                .device_id()
                .map(ToString::to_string)
                .unwrap_or_default(),
            uptime / 3600,
            (uptime % 3600) / 60,
            client.joined_rooms().len(),
            if own_device_verified {
                "signed"
            } else {
                "NOT signed"
            },
            self.inner.verification.trust_summary(sender).await,
            self.inner.admins.len(),
            required(self.inner.policy.require_encryption),
            required(self.inner.policy.require_verified_device),
        )
    }
}

fn required(flag: bool) -> &'static str {
    if flag {
        "required"
    } else {
        "optional"
    }
}

/// Parse a console command; `None` when `body` is not one.
fn parse(body: &str) -> Option<Command> {
    let mut parts = body.split_whitespace();
    let head = parts.next()?;
    let command = match head {
        "!admin" => match parts.next() {
            None | Some("help") => Command::Help,
            Some("status") => Command::Status,
            Some("verify") => Command::Verify,
            Some("settings") => Command::Settings,
            Some("set") => {
                let rest = body
                    .splitn(3, char::is_whitespace)
                    .nth(2)
                    .map(str::trim)
                    .unwrap_or("");
                match rest.split_once(char::is_whitespace) {
                    Some((key, value)) if !value.trim().is_empty() => {
                        Command::Set(key.to_owned(), value.trim().to_owned())
                    }
                    _ => Command::Invalid("usage: !admin set <key> <value>"),
                }
            }
            Some("unset") => match (parts.next(), parts.next()) {
                (Some(key), None) => Command::Unset(key.to_owned()),
                _ => Command::Invalid("usage: !admin unset <key>"),
            },
            Some("reset-trust") => return Some(parse_reset_trust(parts)),
            Some("verify-device") => return Some(parse_verify_device(parts)),
            Some(_) => Command::Invalid("unknown admin command"),
        },
        "!reset-trust" => parse_reset_trust(parts),
        "!verify-device" => parse_verify_device(parts),
        _ => return None,
    };
    Some(command)
}

fn parse_reset_trust<'a>(mut parts: impl Iterator<Item = &'a str>) -> Command {
    let (Some(user), None) = (parts.next(), parts.next()) else {
        return Command::Invalid("expected exactly one Matrix user ID");
    };
    match user.parse() {
        Ok(user_id) => Command::ResetTrust(user_id),
        Err(_) => Command::Invalid("invalid Matrix user ID"),
    }
}

fn parse_verify_device<'a>(mut parts: impl Iterator<Item = &'a str>) -> Command {
    let (Some(user), Some(device), None) = (parts.next(), parts.next(), parts.next()) else {
        return Command::Invalid("expected exactly a Matrix user ID and device ID");
    };
    match user.parse() {
        Ok(user_id) => Command::VerifyDevice(user_id, OwnedDeviceId::from(device)),
        Err(_) => Command::Invalid("invalid Matrix user ID"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{device_id, user_id};

    #[test]
    fn console_commands_are_parsed_strictly() {
        assert_eq!(parse("!admin"), Some(Command::Help));
        assert_eq!(parse("!admin status"), Some(Command::Status));
        assert_eq!(
            parse("!admin set daily.time 08:00"),
            Some(Command::Set("daily.time".into(), "08:00".into()))
        );
        assert_eq!(
            parse("!admin set language  \"de\" "),
            Some(Command::Set("language".into(), "\"de\"".into()))
        );
        assert!(matches!(parse("!admin set key"), Some(Command::Invalid(_))));
        assert_eq!(
            parse("!admin unset language"),
            Some(Command::Unset("language".into()))
        );
        assert!(matches!(
            parse("!admin frobnicate"),
            Some(Command::Invalid(_))
        ));
        assert_eq!(parse("!health"), None);
        assert_eq!(parse("!set foo"), None);
    }

    #[test]
    fn verification_commands_keep_their_legacy_names() {
        assert_eq!(
            parse("!reset-trust @alice:example.org"),
            Some(Command::ResetTrust(
                user_id!("@alice:example.org").to_owned()
            ))
        );
        assert_eq!(
            parse("!verify-device @alice:example.org DEVICE"),
            Some(Command::VerifyDevice(
                user_id!("@alice:example.org").to_owned(),
                device_id!("DEVICE").to_owned()
            ))
        );
        assert_eq!(
            parse("!admin reset-trust @alice:example.org"),
            Some(Command::ResetTrust(
                user_id!("@alice:example.org").to_owned()
            ))
        );
        assert!(matches!(
            parse("!verify-device @alice:example.org"),
            Some(Command::Invalid(_))
        ));
        assert!(matches!(
            parse("!reset-trust nope"),
            Some(Command::Invalid(_))
        ));
    }
}
