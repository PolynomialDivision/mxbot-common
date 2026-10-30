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
    .admin_help("!mycommand: …")         // optional, listed by `!admin`
    .start(&config.matrix, &config.security)
    .await?;                               // session, verification, invites, admin console

bot.client.add_event_handler(/* bot handlers, see below */);
bot.initial_sync().await;                  // retried; also joins pending invites
bot.run().await                            // sync until SIGTERM/SIGINT (finishes the batch in hand)
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
`allowed_rooms`, never into direct chats (admin direct chats only with
`receive_broadcasts`).

## Configuration (`[matrix]` / `[security]`)

| Key | Meaning |
| --- | --- |
| `matrix.access_token`, `matrix.recovery_key` | May instead be set via `MATRIX_ACCESS_TOKEN` / `MATRIX_RECOVERY_KEY`. |
| `security.allowed_inviters` | **Required.** `"all"` or a list of user IDs; `[]` allows nobody. Omitted = the bot refuses to start. |
| `security.allowed_rooms` | **Required.** `"all"` or a list of room IDs the bot may join and post to; `[]` for none. Omitted = the bot refuses to start. |
| `security.dm_rooms` | Rooms whose current members may open a private chat with the bot (direct invite; the bot leaves unless it is just the two of them, encrypted). Bots check such chats with `bot.is_member_dm(&room, &user)`. |
| `security.admin_users` | Admins. Invalid IDs are a startup error. |
| `security.encryption_strategy` | `identity_based` (default), `only_trusted`, `all_devices`. |
| `security.trust` | Whose devices count. `verified` (default): an identity verified with the bot (`!admin verify`); if that person resets their keys, nothing reaches them until another admin runs `!reset-trust` and they verify again. `cross_signed`: every device signed with the person's current identity; a new identity is accepted (logged), unsigned devices still get nothing. Goes with `identity_based`. Gives up protection against a full account takeover with a key reset. |
| `security.verification.*` | Who may verify with the bot (admins always may), flow limits. |
| `security.admin_dm.*` | Admin direct-chat policy, see below. |

`STORE_PATH` (default `store`) holds the SDK stores and the bot's own files.

`MXBOT_HEARTBEAT_FILE` (optional) names a file that gets the current Unix
time after every successful sync, for container health checks such as
`f="$MXBOT_HEARTBEAT_FILE"; [ -f "$f" ] && [ $(( $(date +%s) - $(cat "$f") )) -lt 120 ]`.
Unset, nothing is written.

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
on) and (with `require_verified_device = true`) it came from a device that
counts under `security.trust`: verified with the bot, or (`cross_signed`)
signed by the admin's own current identity. Bot-specific commands (e.g.
`!startquiz`) are passed back to the bot as `Dispatch::AdminDm` and
answered in the direct chat.
`!reset-trust` / `!verify-device` keep working silently in shared rooms.

## Runtime settings

`settings::Settings<T>` holds non-secret knobs admins may change at runtime.
The config file provides the defaults; overrides are validated against `T`
and stored in `store/settings.json`. Secrets never belong in `T`.

## Commands, replies, reaction buttons

`command` holds a bot's commands as one `const` table: parsing and the help
come from it, the bot keeps its own command enum and handlers.

```rust
use mxbot_common::command::{Commands, Parsed, Spec};

const COMMANDS: Commands = Commands::family(
    "garbage",                                         // `!garbage <verb> …`
    &[
        Spec::new("", "📋", "next per type"),           // `!garbage` alone
        Spec::new("next", "⏭️", "next pickup").aliases(&["n"]),
        Spec::new("refresh", "🔄", "reload").admin(),
        Spec::new("help", "", "this help").aliases(&["?"]).hidden(),
    ],
);

match COMMANDS.parse_as(body, || bot.is_admin(&sender)) {
    Parsed::Run(cmd) => { /* cmd.spec.name, cmd.rest */ }
    Parsed::Refused(_) => { /* reply::NOT_ADMIN or the bot's own text */ }
    Parsed::Unknown(word) => { /* `!garbage <word>`: help or a hint */ }
    Parsed::NotOurs => {}                              // stay silent
}
let help = COMMANDS.help("🗑️ Garbage commands");    // "📋 !garbage: next per type\n⏭️ …"
```

* Families (`!garbage <verb>`) answer unknown verbs (`Parsed::Unknown`); flat
  tables (`Commands::flat`, `!today`) leave unknown heads alone, since bots
  share rooms.
* Who may run a spec: everyone, `.admin()` (others get `Refused`) or
  `.admin_silent()` (others are ignored). `parse` skips the check, for bots
  that check in their handler; `parse_as` asks the hook only when needed.
* Per table: `.case_sensitive()` (default: ASCII case-insensitive),
  `.raw_body()` (keep a reply's quoted fallback, default: stripped),
  `.separator(" · ")` in the help (default `": "`). `admin_help()` lists the
  admin specs for `BotBuilder::admin_help`.

`reply` names the status emoji replies start with (`DONE`, `FAILED`,
`FORBIDDEN`, `WARNING`, `NOT_UNDERSTOOD`, `NOTHING`, `HINT`, `HELP`), offers
`NOT_ADMIN` and `plain_and_html` (escaped, line breaks as `<br>`).

`reaction::key` / `is_one_of` compare reaction keys without variation
selectors (skin tones still count); `reaction::add_buttons` seeds button
reactions under a message, best effort, with stable transaction IDs if given.

## Scheduled posts (feature `schedule`)

`clock::Zone` is a bot's time zone (`timezone` from its config, else `TZ`,
else Europe/Berlin) with one DST rule: a local time in the spring gap runs
at the first valid minute after it, an ambiguous autumn time at the earlier
instant. `schedule` runs daily, weekly and monthly jobs on it:

```rust
use mxbot_common::{clock::Zone, schedule::{self, Delivery, Job, JsonLedger, Outcome, Rule}};

let zone = Zone::resolve(config.timezone.as_deref())?;
let ledger = JsonLedger::open(store.join("schedule.json")).await?;  // {"report": "2026-W44"}
let job = Job::new("weekly report", zone, Delivery::AtLeastOnce, Duration::from_secs(12 * 3600),
                   move || settings.get().enabled.then(|| Rule::Weekly { day: Weekday::Mon, at: time }))
    .entry("report")
    .wake_on(settings.store().subscribe());          // settings changes apply at once
supervise_ticking("weekly report", job.stall_after(Duration::from_secs(20 * 60)), |alive| {
    schedule::run(job, ledger, alive, move |slot| {
        let bot = bot.clone();
        async move {
            let key = format!("my-bot:report:{}", slot.key());   // job + slot (+ room, added by post)
            let deadline = Instant::now() + Duration::from_secs(600);
            schedule::post(bot.broadcast_rooms(), content, &key, deadline)
                .await
                .outcome(Duration::from_secs(900))           // Done, or RetryIn when a room may still get it
        }
    })
});
```

* A slot stands for its period (`2026-10-30`, `2026-10-30 08:00` with several
  times a day, `2026-W44`, `2026-10`): moving the time after a run does not
  run that period again, and a recorded later period covers earlier ones.
* The runner plans from UTC every round and sleeps at most 15 minutes
  (ticking `alive`). A slot missed while the bot was down runs at the start
  if it is at most the job's catch-up window old.
* `Delivery::AtLeastOnce` records a slot after the work returned `Done` or
  `Skipped`; `RetryIn(d)` tries again after `d` within the window. Sends must
  then use stable transaction IDs (`schedule::post`, or `send::txn_for` with a
  key naming job, slot and room) so a second try returns the first event.
  `Delivery::AtMostOnce` records the slot before the work (games).
* `JsonLedger::open_dated` writes the slot's date instead of its key: the
  shape of calendar-bot's `posted.json`. A bot with a database implements
  `schedule::Ledger` on a table (radar-bot: its `alerts` rows).
* `Runner::round(ledger, now, work)` is one round at a given time, for tests.

## Other modules

* `format`: `mentionify` & co.: `@user:server` → pill + `m.mentions`,
  `**bold**`, `~~strike~~`, `[label](url)`, newlines.
* `send`: idempotent `send_with_retry`, and `send_with_retry_txn` /
  `send_raw_with_retry_txn` with a transaction ID the bot stores itself
  (retry the same logical message after a restart); thread-reply helpers.
* `media`: files and images for a room, encrypted in an encrypted room
  (never `client.media().upload` and a plain `url` there). An `Attachment`
  (file name, MIME type, bytes; `caption`, an image's `dimensions`) uploads
  itself on `upload(&room)`: asks the room's encryption state (an error, not
  a plain upload, when it cannot), encrypts if needed, retries transient
  failures as sends do, and keeps one source per kind for further rooms.
  `content(source)` is `m.image` for `image/*`, `m.file` otherwise, with
  `info`; the caption is the body and the file name `filename` (MSC2530).
  `media::send` / `send_txn` upload and post in one go. A bot that builds
  its own event takes `media::upload(&room, &mime, bytes)` and puts the
  `MediaSource` into `ImageMessageEventContent::new` / `FileMessageEventContent::new`.

  ```rust
  let map = media::Attachment::new("map.png", mime::IMAGE_PNG, png)
      .caption("🗺️ Guess 2/5")
      .dimensions(w, h);
  media::send_txn(&room, &map, &send::txn_for(&key)).await?;
  ```

  Not the SDK's `Room::send_attachment`: it uploads again on every retry,
  sends audio and video as `m.audio` / `m.video`, keeps the source to itself
  and fetches the replied-to event for a relation. No thumbnails (no bot
  sends any).
* `private`: encrypted 1:1 chats the bot creates (`create_private_dm`,
  `PREFERRED_ROOM_VERSIONS`), fresh checks asked of the server
  (`is_private_dm`, `is_joined_now`, and `is_private_chat_of_member`:
  joined to the main room and the chat still just the bot and them).
* `persist`: atomic JSON load/save for bot state files.
* `clock` / `schedule` (feature `schedule`): see above.
* `retry`: `Backoff` with optional jitter.
* `session`: build a client for secondary accounts.
* `calendar` (feature `calendar`): `ics` (parsing, recurrence), `caldav`
  (reading ICS URLs and CalDAV collections; a source's password may come from
  the environment with `password_env` and `Source::resolve_password_env`) and
  `nextcloud`: a Nextcloud account's own calendars, made with a display name
  (`ensure_calendar`), listed and deleted, shared (`share`, `unshare`,
  `shares`, `plan_shares`), and single events written by UID (`put_event`
  with `Precondition::Create` / `Match(etag)` / `Overwrite`, `delete_event`,
  where a missing event is no error) and listed with their ETags
  (`list_events`, one PROPFIND without event data).
* `directory` (feature `directory`): the house's directory (people, their
  Matrix accounts and groups).

## Websites behind the house login (feature `web`)

For a bot's website behind the house's reverse proxy with forward auth
(cleaning-bot, schnaeppchenjaeger-bot). Framework-free: it reads an
`http::HeaderMap` (axum's is the same type).

```rust
use mxbot_common::{config::AllowList, web::{self, Access, ForwardAuth, ProxySecret, Visitor}};

// At the start: no secret, or a short one (< 32 bytes), stops the bot.
let secret = ProxySecret::from_env("DEALS_PROXY_SECRET", "x-deals-proxy-secret")?;
let access = Access {
    allowed_groups: AllowList::Only(["residents".to_owned()].into()), // [] = nobody
    admin_groups: vec!["it-admins".into()], // always in; `admin_group` set
    guests: false,                          // no login: a guest instead of 401
};
let auth = ForwardAuth::new(secret, access, "example.org")?; // allowed_groups unset: refused

// Per request:
match auth.identify(&headers) {
    Ok(Visitor::Person(p)) => { /* p.login, p.name, p.email, p.groups, p.house_account, p.accounts */ }
    Ok(Visitor::Guest) => { /* only with `guests` */ }
    Err(refusal) => { /* Proxy: 403, NoLogin: 401, NotAllowed: 403, in the page's language */ }
}
let lang = web::lang::pick(&headers, query.as_deref(), web::lang::Lang::De);
```

* The secret is compared in constant time (`subtle`; `web::secret_eq` for
  other bearer checks); only the website's own header counts.
* Groups fail closed like `AllowList`: `[]` lets nobody with a login in,
  `"all"` everybody; a guest is not group-checked.
* `house_account(login, server)`: `@<login>:<server>`, lowercased, for a
  plain localpart (a–z, 0–9, `._=-/`, up to 64), else `None`. The headers
  carry no `matrix_localpart`, so a person renamed at the login is their new
  login's account on a website (the directory honours the attribute). The
  deals website keeps its `@web-<hash>` stand-in for other logins.
* `accounts`: the house account first, then `X-House-Matrix-Ids`, each once,
  eleven at most.
* `web::lang`: `Lang` (de, en, uk), `pick` (`?lang=`, the `house_lang`
  cookie, Accept-Language by weight then order, else the website's default),
  and `text` / `fill` over a website's own `(key, de, en, uk)` table.

## SQLite stores (feature `sqlite`)

A bot's own database (kummerkasten-bot, schnaeppchenjaeger-bot): one
connection behind a mutex, used from `spawn_blocking`, and numbered
migrations applied once each, in order, every one in its own transaction.

```rust
use mxbot_common::sqlite::{Db, Options};

/// Never edit one that has shipped: add the next.
const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../migrations/001_initial.sql")),
    (2, include_str!("../migrations/002_more.sql")),
];

let db = Db::open(&store.join("bot.sqlite"), MIGRATIONS, Options { secure_delete: true }).await?;
let n: i64 = db.run(|c| Ok(c.query_row("SELECT count(*) FROM things", [], |r| r.get(0))?)).await?;
```

* Every database: WAL, foreign keys on, `synchronous = NORMAL`, a busy
  timeout of 5 s; `secure_delete` per bot (an extra measure, not erasure:
  the WAL, the filesystem and backups may still hold old data).
* Applied migrations are rows of `schema_migrations (version, applied_at)`
  (`applied_at` in UTC, `2026-10-30T12:00:00Z`). A failing migration rolls
  back and stops the start; the database stays at the version before.
* A database with a migration newer than the image knows is refused ("The
  database is from a newer version of the bot (migration 7, this one knows up
  to 6) …"): an older image would write what the schema no longer takes.
  Run the newer image again, or restore a backup from before it.
* `sqlite::migrate`, `configure`, `open_connection` and `memory_connection`
  are the same steps without the runtime, for tests; `Db::with` runs on the
  connection synchronously.
* It is the SQLite the SDK's store already links (rusqlite 0.40, re-exported
  as `sqlite::rusqlite`); a bot that compiles SQLite in keeps rusqlite's
  `bundled` in its own `Cargo.toml`.

## Developing against a local checkout

Cargo resolves the git dependency from GitHub. To build a bot against local
changes, temporarily add to its `Cargo.toml` (do not commit):

```toml
[patch."https://github.com/PolynomialDivision/mxbot-common"]
mxbot-common = { path = "../mxbot-common" }
```

After pushing a new `mxbot-common` commit, run `cargo update -p mxbot-common`
in each bot so its `Cargo.lock` (used by `--locked` Docker builds) picks it up.
