# mxbot-common

Shared Matrix infrastructure for the mxbot fleet (calendar, cleaning, email,
garbage, geoguessr, health, quiz, radar, telegram-mirror, transcribe,
translate and tuneguessr bots).

Bots depend on it via git:

```toml
mxbot-common = { git = "https://github.com/PolynomialDivision/mxbot-common" }
```

## Matrix SDK

This crate is the single place the fleet's Matrix SDK is declared. It uses
the `msc3952-mentions-encrypted-0.19` fork, which copies `m.mentions` into
the cleartext wrapper of encrypted events so mention notifications work in
encrypted rooms. Bots do **not** depend on `matrix-sdk` themselves and need
no `[patch.crates-io]` section; they use the re-export:

```rust
use mxbot_common::matrix_sdk::{ruma::OwnedUserId, Room};
```

Tests that need the SDK's mock client enable the `testing` feature in
`[dev-dependencies]`.

## Starting a bot

```rust
let config: Config = mxbot_common::config::load_toml(&mxbot_common::config::config_path_from_args())?;
mxbot_common::logging::init("my_bot");

let bot = Bot::builder("my-bot", env!("CARGO_PKG_VERSION"))
    .settings(settings.store().clone())   // optional runtime settings
    .admin_help("!mycommand — …")         // optional, listed by `!admin`
    .start(&config.matrix, &config.security)
    .await?;                               // session, verification, invites, admin console

bot.client.add_event_handler(/* bot handlers, see below */);
bot.initial_sync().await;                  // retried; also joins pending invites
bot.run().await                            // sync with backoff until SIGTERM/SIGINT
```

A message handler starts with the admin console:

```rust
move |ev: OriginalSyncRoomMessageEvent, room: Room, encryption: Option<EncryptionInfo>| async move {
    match bot.admin.handle(&room, &ev, encryption.as_ref()).await {
        Dispatch::Handled => return,          // console command, answered
        Dispatch::AdminDm => { /* bot command from an admin's DM: reply in `room` */ }
        Dispatch::Continue => { /* normal processing */ }
    }
}
```

Broadcasting bots post to `bot.broadcast_rooms()`: joined rooms allowed by
`allowed_rooms`, without admin direct chats.

## Configuration (`[matrix]` / `[security]`)

| Key | Meaning |
| --- | --- |
| `matrix.access_token`, `matrix.recovery_key` | May instead be set via `MATRIX_ACCESS_TOKEN` / `MATRIX_RECOVERY_KEY`. |
| `security.allowed_inviters` | `"all"` or a list of user IDs; `[]` allows nobody. Omitted = everyone. |
| `security.allowed_rooms` | `"all"` or a list of room IDs the bot may join and post to. |
| `security.admin_users` | Admins. Invalid IDs are a startup error. |
| `security.encryption_strategy` | `identity_based` (default), `only_trusted`, `all_devices`. |
| `security.verification.*` | Who may verify with the bot (admins always may), flow limits. |
| `security.admin_dm.*` | Admin direct-chat policy, see below. |

`STORE_PATH` (default `store`) holds the SDK stores and the bot's own files.

## Admin direct chats

Admins can open a direct chat with any bot and run commands there instead
of in shared rooms:

```
!admin                      help (includes the bot's own admin commands)
!admin status               bot, session and trust status
!admin verify               mutual emoji (SAS) verification with you
!admin settings             runtime settings
!admin set <key> <value>    change a runtime setting (persisted)
!admin unset <key>          revert a setting to the config file value
!reset-trust <user>         allow one re-verification of <user>
!verify-device <user> <dev> verify a specific device
```

A message is accepted only if the sender is an admin, the room is the bot's
direct chat with exactly that admin (the bot's own `m.direct` data, no other
members), the event was end-to-end encrypted (`require_encryption`, default
on) and — with `require_verified_device = true` — it came from a device the
bot has verified. Bot-specific commands (e.g. `!startquiz`) are passed back
to the bot as `Dispatch::AdminDm` and answered in the direct chat.
`!reset-trust` / `!verify-device` keep working silently in shared rooms.

## Runtime settings

`settings::Settings<T>` holds non-secret knobs admins may change at runtime.
The config file provides the defaults; overrides are validated against `T`
and stored in `store/settings.json`. Secrets never belong in `T`.

## Other modules

* `format` — `mentionify` & co.: `@user:server` → pill + `m.mentions`,
  `**bold**`, `~~strike~~`, `[label](url)`, newlines.
* `send` — idempotent `send_with_retry`, thread-reply helpers.
* `persist` — atomic JSON load/save for bot state files.
* `retry` — `Backoff` with optional jitter.
* `session` — build a client for secondary accounts.

## Developing against a local checkout

Cargo resolves the git dependency from GitHub. To build a bot against local
changes, temporarily add to its `Cargo.toml` (do not commit):

```toml
[patch."https://github.com/PolynomialDivision/mxbot-common"]
mxbot-common = { path = "../mxbot-common" }
```

After pushing a new `mxbot-common` commit, run `cargo update -p mxbot-common`
in each bot so its `Cargo.lock` (used by `--locked` Docker builds) picks it up.
