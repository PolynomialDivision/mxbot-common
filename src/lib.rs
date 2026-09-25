//! Shared Matrix infrastructure for the mxbot fleet.
//!
//! * [`Bot`] — session restore, verification, invite handling, admin console
//!   and the sync loop in a few calls.
//! * [`format`] / [`send`] — mention-aware messages and idempotent sending.
//! * [`admin`] — admin commands in direct chats with configured admins.
//! * [`settings`] / [`persist`] — persisted runtime settings and JSON state.
//! * [`matrix_sdk`] — the fleet's (forked) Matrix SDK; bots use this
//!   re-export instead of depending on the SDK themselves.

pub mod admin;
mod bot;
pub mod config;
pub mod format;
pub mod logging;
pub mod persist;
pub mod retry;
pub mod rooms;
pub mod send;
pub mod session;
pub mod settings;
pub mod sync;
pub mod verify;

pub use bot::{Bot, BotBuilder};
pub use matrix_sdk;
