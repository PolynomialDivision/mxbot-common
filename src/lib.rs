//! Shared Matrix infrastructure for the mxbot fleet.
//!
//! * [`Bot`]: session restore, verification, invite handling, admin console
//!   and the sync loop in a few calls.
//! * [`format`] / [`send`]: mention-aware messages and idempotent sending.
//! * [`admin`]: admin commands in direct chats with configured admins.
//! * [`settings`] / [`persist`]: persisted runtime settings and JSON state.
//! * [`private`]: private 1:1 chats and fresh membership checks.
//! * [`matrix_sdk`]: the fleet's (forked) Matrix SDK; bots use this
//!   re-export instead of depending on the SDK themselves.
//! * [`heartbeat`]: optional liveness file for container health checks.
//! * [`supervise`]: background loops whose end restarts the bot, and whose
//!   missing rounds stop the heartbeat.
//! * [`trust`]: whose devices count (`[security] trust`).
//! * `calendar` (feature `calendar`): iCalendar and CalDAV, for calendar-bot and plenum-bot;
//!   a Nextcloud account's own calendars (make, share, write events), for plenum-bot and
//!   cleaning-bot.
//! * `directory` (feature `directory`): the house's directory (people, accounts, groups).

pub mod admin;
mod bot;
#[cfg(feature = "calendar")]
pub mod calendar;
pub mod config;
#[cfg(feature = "directory")]
pub mod directory;
pub mod format;
pub mod heartbeat;
pub mod logging;
pub mod persist;
pub mod private;
pub mod retry;
pub mod rooms;
pub mod send;
pub mod session;
pub mod settings;
pub mod supervise;
pub mod sync;
pub mod trust;
pub mod verify;

pub use bot::{Bot, BotBuilder};
pub use matrix_sdk;
