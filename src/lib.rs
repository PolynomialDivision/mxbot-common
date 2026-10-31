//! Shared Matrix infrastructure for the mxbot fleet.
//!
//! * [`Bot`]: session restore, verification, invite handling, admin console
//!   and the sync loop in a few calls.
//! * [`format`] / [`send`]: mention-aware messages and idempotent sending.
//! * [`media`]: files and images, encrypted in encrypted rooms, uploads retried.
//! * [`admin`]: admin commands in direct chats with configured admins.
//! * [`settings`] / [`persist`]: persisted runtime settings and JSON state.
//! * [`private`]: private 1:1 chats and fresh membership checks.
//! * [`matrix_sdk`]: the fleet's (forked) Matrix SDK; bots use this
//!   re-export instead of depending on the SDK themselves.
//! * [`events`]: history from the first sync of an empty store is not acted on.
//! * [`heartbeat`]: optional liveness file for container health checks.
//! * [`supervise`]: background loops whose end restarts the bot, and whose
//!   missing rounds stop the heartbeat.
//! * [`status`]: the bot's status file for monitoring (tasks, queues, Matrix sync).
//! * [`trust`]: whose devices count (`[security] trust`).
//! * [`command`] / [`reply`] / [`reaction`]: a bot's command table (parse, help,
//!   who may run what), the replies' status emoji, reactions as buttons.
//! * `calendar` (feature `calendar`): iCalendar and CalDAV, for calendar-bot and plenum-bot;
//!   a Nextcloud account's own calendars (make, share, write events), for plenum-bot and
//!   cleaning-bot.
//! * `directory` (feature `directory`): the house's directory (people, accounts, groups).
//! * `link` (feature `directory`): link codes that prove a person's other Matrix account.
//! * `membership` (feature `directory`): rooms whose members follow the directory (the plan of
//!   a round, its guards, what is remembered); plenum-bot does the Matrix side.
//! * `clock` / `schedule` (feature `schedule`): the bot's time zone and one DST rule; daily,
//!   weekly and monthly jobs with per-period slots, a ledger, catch-up after a restart and
//!   posts with stable transaction IDs.
//! * `sqlite` (feature `sqlite`): a bot's SQLite store, numbered migrations, a database from a
//!   newer image refused.
//! * `web` (feature `web`): who asks a bot's website through the house's forward auth
//!   (proxy secret, login, groups, Matrix accounts) and the language of its pages.

pub mod admin;
mod bot;
#[cfg(feature = "calendar")]
pub mod calendar;
#[cfg(feature = "schedule")]
pub mod clock;
pub mod command;
pub mod config;
#[cfg(feature = "directory")]
pub mod directory;
pub mod events;
pub mod format;
pub mod heartbeat;
#[cfg(feature = "directory")]
pub mod link;
pub mod logging;
pub mod media;
#[cfg(feature = "directory")]
pub mod membership;
pub mod persist;
pub mod private;
pub mod reaction;
pub mod reply;
pub mod retry;
pub mod rooms;
#[cfg(feature = "schedule")]
pub mod schedule;
pub mod send;
pub mod session;
pub mod settings;
#[cfg(feature = "sqlite")]
pub mod sqlite;
pub mod status;
pub mod supervise;
pub mod sync;
pub mod trust;
pub mod verify;
#[cfg(feature = "web")]
pub mod web;

pub use bot::{Bot, BotBuilder};
pub use matrix_sdk;
