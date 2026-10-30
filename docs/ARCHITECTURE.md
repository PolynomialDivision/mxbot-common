# mxbot-common: architecture and migration plan

Status: plan of 2026-10-30, from five read-only analyses of mxbot-common and the 14 deployed
bots (identity, scheduling, messaging, commands and i18n, observability and release); carried
out 2026-10-30/31 (uncommitted at the time of writing), reviewed independently and the
review's findings fixed. "Result" sums it up; the work packages and notes below have the
detail.

## Result (2026-10-31)

### What mxbot-common is now

One library, no runtime: each bot is still its own process, account, store and container.
New modules (features in brackets; everything else is always built):

| Module | For | Replaces |
|---|---|---|
| `supervise` | background loops: an ending/failing/panicking loop ends the bot (Docker restarts it); `Alive::tick` for hangs, `ok`/`failed` for loops that fail every round | plain `tokio::spawn`, email's `spawn_task` |
| `heartbeat` (extended) | withheld while a loop is unhealthy; `<file>.reason` names it, the house-infra healthcheck prints it | sync-only liveness |
| `events` | `is_replayed`: history from the first sync of an empty store is not a command | old commands run again after a store restore |
| `send` (extended) | `txn_for(key)` stable transaction IDs, `send_until(deadline)` | ten-plus ad-hoc transaction-ID formats, 4 attempts and lost |
| `media` | files/images encrypted in encrypted rooms, uploads retried, re-checked before the send | plaintext uploads in email, geoguessr, quiz |
| `persist` (extended) | unique temp file, flush + fsync, `load_json_or_quarantine` | a race and silent truncation |
| `config` (extended) | unknown keys named (warning; refused with `MXBOT_STRICT_CONFIG`) | silent typos |
| `retry` (extended) | `Backoff::delay_for(attempt)` | hand-rolled formulas |
| `clock` [schedule] | one zone (config, `$TZ`, Berlin) and one DST rule | naive local sleeps in calendar and radar |
| `schedule` [schedule] | rules, per-period slots, a ledger (reads the old files), a runner (at-most-once / at-least-once explicit), `post` with stable IDs | seven scheduler designs |
| `web` [web] | forward-auth identity (secret ≥ 32 bytes, constant-time, fail-closed groups, one login rule), website language pick and text tables | three drifted copies |
| `sqlite` [sqlite] | numbered migrations, newer schema refused, pragmas, async bridge | two identical `db.rs` |
| `command`, `reply`, `reaction` | spec table → parse + help, status emoji, one emoji key, reaction buttons | 11 parsers, 8 normalisers, 7 button helpers (where migrated) |

### Bots migrated

| Bot | Uses now |
|---|---|
| calendar-bot | `schedule`, `clock`, `supervise` |
| radar-bot | `schedule`, `clock`, `supervise`, stable alert keys (`alert_post_keys`) |
| cleaning-bot | `web`, `reaction`, `supervise` |
| schnaeppchenjaeger-bot | `web`, `sqlite`, `reaction`, `supervise` |
| kummerkasten-bot | `sqlite`, `reaction`, `supervise` |
| garbage-bot | `command`, `reply`, `reaction`, `supervise` |
| health-bot | `command`, `supervise` |
| email-bot | `media`, `supervise` with `ok`/`failed`, mail seen only after posting |
| geoguessr-bot, quiz-bot | `media`, `supervise` |
| heating, plenum, telegram-mirror, translate | `supervise` (and every core fix by `cargo update`) |

Every bot gets the core fixes (`persist`, `events`, console-only trust commands, config
warnings) by `cargo update -p mxbot-common`.

### Duplicates removed

About 360 lines of forward-auth and website i18n (cleaning, deals), about 150 of SQLite
plumbing (kummerkasten, deals), garbage's parser/help/normaliser/seed block (about 45 lines),
four reaction normalisers, email's `spawn_task`, kummerkasten's per-tick task, the radar and
calendar sleep-until-next-run loops. Where a migration added lines (health's `Sub` enum, the
pinning tests), it bought one table for parse and help that cannot drift.

### Problems fixed

- Lost posts in a homeserver outage, and posts recorded as done after failing (calendar,
  radar weather and digest); DST an hour off in March and October (calendar, radar, with a
  second October digest); the `posted.json` race between two loops.
- Mail lost by email-bot (seen before posting, M_TOO_LARGE dropped); plaintext media in
  encrypted rooms (email, geoguessr, quiz); the Mapillary token in geoguessr's logs; unescaped
  HTML in translate's formatted body.
- Old commands (admin commands too) run again after a store restore; `!reset-trust` /
  `!verify-device` accepted in any room on the sender ID alone.
- Website auth: a hand-rolled compare, empty `allowed_groups` = everyone, a Unicode lookalike
  login mapping to another account; deals without a newer-schema guard.
- A recurring weather warning swallowed by an old transaction ID (radar); background loops
  dying silently; loops failing every round looking healthy.
- Release: tests, clippy and advisories gated nothing; locks committed untested; mxbot-common
  pushed unchecked; images built from unchecked trees.
- The house guide's wrong help commands for plenum and kummerkasten (house-infra).

### Tests (2026-10-31, against the local mxbot-common)

mxbot-common 172 (all features; clippy clean with all features and each alone; fmt clean).
Bots: calendar 33, cleaning 336, email 61, garbage 82, geoguessr 137, health 31, heating 20,
kummerkasten 170, plenum 92, quiz 106, radar 79, schnaeppchenjaeger 261, telegram-mirror 15,
translate 99: 1,522 tests, 0 failures, clippy -D warnings and fmt clean in all. Every bot's
tests from before pass unchanged (where an expectation changed, it is a named fix). house-infra:
update-bots tests 33, validate 0 errors, yamllint; shellcheck on the four scripts.
Not tested: against a real homeserver (MTX-11: stop tuwunel for a minute at a post time and
expect exactly one post), and the SQLite migration against the production databases (tested
against databases the old code built).

### Rollout (owner)

1. Commit mxbot-common, push it. 2. In every bot `cargo update -p mxbot-common`, then
`./check-bots.sh` (all must pass), commit, `./push-bots.sh` (health and heating have no
remote). 3. house-infra: commit (`docs`, `matrix.yml` help commands, the healthcheck in
`stacks/matrix-bots/compose.yaml.j2`, `update_bots.py`), then `make update-bots`.
On the first start: radar records the latest digest slot as done (no repeat), creates the
`alert_post_keys` table; calendar reads `posted.json` as before; kummerkasten and deals open
their databases unchanged. A rollback reads the same files (radar ignores the new table).

### Easy next steps

A new bot: `Bot::builder`, `supervise`/`supervise_ticking`, `schedule::Job` for posts, a
`command` table for its commands, `web` for a website, `sqlite` for a store: the plumbing is
in the library. Next migrations that pay: quiz/geoguessr schedulers onto `schedule` (both mark
the slot before the round), cleaning/plenum/kummerkasten private chats onto one helper,
`command` tables for calendar, radar and translate, a `[security] room_admin_commands`
setting on `parse_as`, metrics from `supervise`/`send` once the network question is decided.

## Principles

- **A library, not a runtime.** Every bot stays its own process with its own account, store
  and container. Nothing central whose failure stops all bots; no new service.
- **Opt-in modules.** Bots use what they need; heavier parts sit behind cargo features (as
  `calendar` and `directory` do today). A new module must remove code from at least two bots
  or fix a bug in at least two, or it stays in the bot.
- **Compatible by default.** Existing functions keep their signatures and behaviour. Changes
  in behaviour are bug or security fixes, named in the changelog of the commit.
- **Business logic stays in the bot.** Plenum's meeting engine, cleaning's rotation,
  kummerkasten's outbox and screens, game input, the deals chat: bespoke on purpose.
- **Configuration stays declarative** in house-infra (matrix.yml → rendered `config.toml`).
- **No new heavy crates.** rusqlite (already in every bot through the SDK), chrono/chrono-tz,
  `subtle` (already through vodozemac) are reused. Rejected: tokio-cron-scheduler, apalis,
  fluent, rust-i18n, clap, figment, the SDK's `send_queue` (no idempotency key, one failure
  blocks a room).

## Layers

| Layer | Modules | Who |
|---|---|---|
| Start and run | `Bot`, `session`, `sync`, `config`, `logging`, `heartbeat`, `supervise` | every bot |
| Sending | `send` (idempotent retries, stable transaction IDs), `format`, `private`, `rooms` | every bot |
| Time | `clock` (zone, DST rule), `schedule` (rules, slots, ledger, runner) | posting bots |
| State | `persist` (atomic JSON), `sqlite` (migrations) | bots with stores |
| People | `admin`, `trust`, `verify`, `directory`, `web` (forward-auth identity) | per bot |
| Surface | `command` (spec table, parse, help), `reply`, `reaction`, `i18n` | per bot |

## Work packages

| # | Package | Fixes | Pilot, then | Status |
|---|---|---|---|---|
| 0 | Release gate: `check-bots.sh` (fmt, clippy -D warnings, `cargo test --locked`, `cargo audit` with a reviewed ignore list), `--locked` in Dockerfiles, `update-crates.sh` and update-bots gated | MTX-3, locks committed untested | all bots | done (uncommitted): `check-bots.sh`, `deny.toml` (cargo-deny, reviewed ignore list), `build-bots.sh`/`update-crates.sh`/`update-sdk-fork.sh`/update-bots gated, `--locked` in the 14 Dockerfiles. See notes below |
| 1 | Core fixes (no bot change needed): `persist` race and fsync, stale-event filter in the admin console, trust commands only in the console, `send` with stable transaction IDs and deadlines, `Backoff::delay_for`, warnings for unknown config keys | persist race, replayed admin commands, MTX-5 groundwork | every bot by `cargo update` | done: `config::load_toml` names unknown keys (warning; refused with `MXBOT_STRICT_CONFIG`; not inside sections with `#[serde(flatten)]`), `persist` (unique temp file, fsync, `load_json_or_quarantine`), `events::is_replayed` (first sync of an empty store) used by the admin console, `!reset-trust`/`!verify-device` only in the console, `send::txn_for` + `send_until`, `Backoff::delay_for` |
| 2 | `clock` + `schedule`: one DST rule, per-period slots, a ledger that reads the old files, a runner that re-plans from UTC and ticks `supervise` | DST in calendar and radar, posts recorded as done after failing (MTX-5) | calendar-bot, then radar-bot | done in the working trees (2026-10-30), not committed; see note 2 |
| 3 | `web`: forward-auth identity (secret required, constant-time compare, fail-closed groups, one login → Matrix ID rule) and the website language pick | three drifted copies, empty groups = everyone | cleaning-bot, then schnaeppchenjaeger-bot | done (uncommitted): `web` (feature `web`) with tests; cleaning-bot and schnaeppchenjaeger-bot migrated, their tests unchanged and green. See notes below |
| 4 | `command` + `reply` + `reaction`: a spec table per bot, parse, help generated, one refusal text, one emoji normaliser and button helper | 11 parsers, 8 emoji normalisers, 7 button helpers | garbage-bot, then small bots | done (uncommitted): modules with tests; garbage-bot and health-bot migrated with pinned replies; heating-bot left (one exact admin-room command, nothing to gain). See notes below |
| 5 | `sqlite`: numbered migrations, refuses a newer schema | identical db.rs in kummerkasten and deals | kummerkasten-bot, schnaeppchenjaeger-bot | done (uncommitted): `sqlite` (feature `sqlite`) with tests; kummerkasten-bot and schnaeppchenjaeger-bot migrated (schema, `schema_migrations` and pragmas unchanged, tested against databases built by the old code); reaction keys in cleaning, kummerkasten and deals' text answers through `reaction`. See notes below |
| 6 | Bot fixes found on the way: email marks mail seen before posting it, a token in geoguessr's logs, unescaped HTML in translate, radar alert transaction IDs, the guide's wrong help commands (house-infra) | lost mail, secret in logs, injection | per bot | done (uncommitted) except radar's transaction IDs (radar-bot is migrated in package 2): email, geoguessr, translate fixed with tests; the guide says `!plenum help` and `!kummerkasten`. See notes below |
| 7 | Health reasons: `Alive::ok`/`failed` (a loop failing every round turns unhealthy after its time, not only a hung one), `supervise::unhealthy()`, `<heartbeat>.reason` printed by the house-infra healthcheck (State.Health.Log) | a loop that fails every round looked healthy (email IMAP with a wrong password) | email-bot | mxbot-common and the healthcheck done; bots adopt `ok`/`failed` per loop |
| 8 | `media`: files and images encrypted in encrypted rooms (`Attachment`, `upload`, `send`/`send_txn`), uploads retried; email-bot adopts `Alive::ok`/`failed` (package 7) | plaintext media in E2EE rooms (messaging #7/#10), uploads not retried | email-bot, geoguessr-bot, quiz-bot | done (uncommitted): module with tests, the three bots migrated with content tests, their tests unchanged and green. See notes below |

### Notes on package 0

- `~/matrixbots/check-bots.sh [-f] [-v] [bot …]`: per bot `cargo fetch --locked`, `cargo fmt
  --check` (enforcing: every bot was formatted), `cargo clippy --locked --all-targets -- -D
  warnings`, `cargo test --locked`, `cargo deny check advisories sources` (mxbot-common, when
  named: `--all-features`). A pass is recorded in `.build-logs/<bot>.checked` keyed by the git
  tree ID of the tracked files (the commit's tree when clean, so committing exactly the checked
  lock keeps it valid), the rustc version and a check version; an unchanged bot only gets the
  advisory check again. `-v` only verifies (build-bots.sh uses it). `SKIPCHECKS=1` skips the
  gate everywhere, loudly.
- cargo-deny, not cargo-audit: one `~/matrixbots/deny.toml` for every crate, `unmaintained =
  "all"` so every new notice is decided there, sources limited to crates.io and the two
  PolynomialDivision forks. Ignored (a reason per entry in the file): RUSTSEC-2026-0187 (lopdf
  0.26 parser, unreachable: plenum only writes PDFs, genpdf/printpdf never call lopdf's reader),
  plenum's other PDF-stack crates (encoding, lzw, rusttype, stb_truetype), anymap2 (inside
  matrix-sdk 0.19), ttf-parser (geoguessr, only its compiled-in fonts). derivative
  (RUSTSEC-2024-0388) is not in any bot's Linux graph.
- `update-sdk-fork.sh` derives the bots from the directories (plenum-bot was missing), runs
  `cargo test --locked` instead of `cargo check`, and commits each bot's lock only after
  `check-bots.sh` passed. `update-crates.sh` reverts a lock that fails. house-infra
  `update_bots.py` checks a new mxbot-common lock before committing it and checks every bot it
  builds.
- Not done: `rust-toolchain.toml` pinned to the image's Rust (the local rustc 1.97.1 lints, the
  image builds with 1.99.1; a pin touches every repository), `--locked` on `cargo chef cook`,
  `ARG MXBOT_BUILD`, kummerkasten's `.dockerignore`, the `--check-config` step (needs package 1's
  strict config in the bots), checking mxbot-common before `push-bots.sh` in update-bots.

### Notes on package 6

- email-bot: the IMAP side hands each email to the send loop and waits for the outcome; the UID
  is marked seen (and `last_uid` moves) only when the email was posted, put into the retry
  queue, or deliberately not posted (not from the list, bridge echo, unparseable). A failed post
  that cannot be queued stays unseen and is fetched again next cycle (`UidCursor` as for a
  failed fetch). Reposting after a restart is harmless (transaction IDs from the email). Text +
  HTML above 32 KB (Matrix events: 65,535 bytes after encryption and JSON escapes) are cut at a
  line end with "✂️ [Shortened: the full email is too long for Matrix]" and HTML made from the
  kept text (it used to fail with M_TOO_LARGE ten times and be dropped). Fetch and post now run
  one email at a time instead of pipelined.
- geoguessr-bot: the Mapillary token goes in `Authorization: OAuth <token>` (sensitive header
  value), no longer in the URL that reqwest errors, the warnings and the returned error carried.
  Mapillary answers a fake token in the header exactly as in `access_token` (checked
  2026-10-30). A test captures the log events of a network and a decode error.
- translate-bot: the translation of a plain-text message is HTML-escaped in `formatted_body`
  (`build_text_line`); the plain body is unchanged. HTML-mode translations (of a formatted
  source) are unchanged.
- house-infra `matrix.yml`: `help_command` of plenum-bot `!plenum help` (it ignores `!help`),
  kummerkasten-bot `!kummerkasten` (opens its private chat, where `!help` works).
- Open: radar alert transaction IDs (recurring NINA/DWD keys), with radar's package 2 migration.

### Notes on package 5

- `sqlite` is kummerkasten's `db.rs` moved up: `Db` (`open`, `from_connection`, `run` on
  `spawn_blocking`, `with`), `migrate`/`configure`/`open_connection`/`memory_connection`,
  `Options { secure_delete }`, migrations a `&[(i64, &str)]` slice. rusqlite 0.40 without
  default features (the SDK's, system SQLite in mxbot-common's own tests); the bots keep their
  `bundled`. `applied_at` is written by SQLite (`strftime`), in the bots' `time::to_text` shape.
- The one difference between the two copies: deals had no newer-database guard and no
  `secure_delete`. Deals now refuses a database from a newer image like kummerkasten (a rollback
  fix, pinned in its tests); `secure_delete` stays per bot (on for kummerkasten, deals keeps
  SQLite's default).
- Each bot's `db.rs` keeps `MIGRATIONS`, its `Options` and `open`; its tests are unchanged and
  call the same `migrate`/`configure`/`memory_connection`. A new test per bot builds databases
  with a frozen copy of the old code (fresh, and at every older version with data) and checks
  that the new code leaves the same `sqlite_master`, `schema_migrations` history and
  `user_version` (0).
- Not done: `ensure_column` and `Fn` migrations (no migrated bot needs them; they come with
  radar, translate, quiz, geoguessr and email), a `time` helper.
- Reaction keys: cleaning (help board, onboarding numbers, trades) and kummerkasten
  (`screens::same_key`) use `reaction::is_one_of`, deals' typed answers (`commands::answer`)
  `reaction::key`. Newly ignored: U+FE00..FE0E, and in deals' answers a U+FE0F inside the text
  (none of its answer words has one). Deals' reaction handler (chat.rs, the trailing U+FE0F trim)
  is kept: a keycap (1️⃣) carries U+FE0F before U+20E3, and stripping it would newly accept a
  keycap sent without it.

### Notes on package 4

- `command` is a parsing and help toolkit, not a dispatcher: `Commands::family` / `flat`, a
  `const` table of `Spec`s, `parse` (access left to the bot) or `parse_as` with an admin hook,
  `help`, `admin_help`. Per table: case (default insensitive), reply-fallback stripping (default
  on), help separator (default `": "`). Migrated bots set `.raw_body()` (and health
  `.case_sensitive()`) to keep today's behaviour; the open owner decisions (capitalised heads, one
  refusal text, help layout) stay open.
- The one input that changed in garbage-bot: a tab or line break after `!garbage` now separates
  the verb like a space (it used to make the message no command). Pinned in its tests.
- `reply::NOT_ADMIN` is garbage's and quiz's wording. Bots with another wording keep theirs.
- `reaction::key` strips U+FE00..FE0F (garbage's rule, the widest of the copies); skin tones stay
  part of the key (only cleaning's 🙋 accepts them, bespoke). `add_buttons` takes an optional
  transaction-ID base; garbage keeps its `<reminder txn>-seed` ID.
- Not done: the quote-aware tokenizer, `Renamed` hints and "did you mean" (`closest`) from the
  analysis. No migrated bot uses them; they belong with the cleaning and deals migrations.
- `admin.rs` is unchanged: `.admin_help(COMMANDS.admin_help())` works with the existing builder.

### Notes on package 3

- `web` is a module of mxbot-common behind feature `web` (deps `http`, `subtle`; no axum), not a
  separate crate: simpler for the fleet; tv-pilot would need the split (below).
- Security fixes: an empty `allowed_groups` now lets nobody with a login in (was everyone); both
  production templates render `identity_access.<service>.access`, non-empty, so production is
  unchanged. `ForwardAuth::new` refuses an unset `AllowList` and warns on an empty one. The
  secret compare uses `subtle` instead of a hand-rolled fold. Both bots already refused to start
  without the secret (house-infra renders it from the vault for both).
- Kept as it was: the bots' `[web] allowed_groups` stay plain lists (no `"all"`), `Settings`
  keeps its shape so the tests did not change. Deals keeps `@web-<hash>` for logins that are no
  plain localpart (changing it would change who those people are in its audit log and limits)
  and `acting_account` (editors/admins), CSRF and Origin checks.
- The one login → Matrix ID rule (`web::house_account`) is the websites' strict one. The
  directory (`accounts_of`) still uses `matrix_localpart` and `UserId::parse`; the forward-auth
  headers carry no localpart, so a renamed resident is a different person on the websites than in
  chat. Closing it needs a provider-side header (house-infra: scope mapping + Caddy), then
  `identify` can prefer it; not done here (it changes who someone is).
- Language: `web::lang::pick` is cleaning's rule. Deals passes no query (no `?lang=` switcher) and
  now reads every Accept-Language line (was the first) and ignores a malformed `q` (was: the item
  dropped); browsers send neither. Defaults stay German on both websites; texts unchanged.
- tv-pilot (not migrated): axum 0.8 has the same `http::HeaderMap`, but mxbot-common pulls the
  Matrix SDK, so it needs `web` as a matrix-free crate first. Then: make `TV_PROXY_SECRET`
  required (`ProxySecret`), keep its `trusted_proxies` peer check and time-bucketed CSRF locally,
  map `resident`/`admin` to `allowed_groups`/`admin_groups`, and decide its stricter user rule
  (≤ 64, no whitespace or `|`, ASCII `to_str`) and its 400 for a bad user.

### Notes on package 8

- `media::Attachment::upload(&room)` asks the room's encryption state (`latest_encryption_state`,
  an error rather than a plain upload when it cannot be had), then `upload_encrypted_file`
  (event `file`, key inside the encrypted event) or a plain upload (`url`). The upload is
  retried with `send`'s attempts and classification (`send::retry_request`, the private
  `retry_send_limited` made generic over the result); one source per kind is kept, so email's
  attachment for two plain rooms is uploaded once, as before.
- Not `Room::send_attachment`: it re-uploads on every retry, sends `audio/*`/`video/*` as
  `m.audio`/`m.video` (email sends them as `m.file`), keeps the source to itself and fetches the
  replied-to event for a relation (quiz's thread without fallback). The SDK's
  `AttachmentConfig::txn_id` exists; here the transaction ID goes to `send_with_retry_txn`.
- Unencrypted rooms get the same events as before: the bots keep their own builders (email: image
  with `mimetype`/`size`, everything else a bare `m.file`; geoguessr: label as body, sanitized
  `filename`, `w`/`h`/`size`; quiz: answer as body, the server's content type, thread without
  fallback) and only take a `MediaSource` instead of an mxc URI; tests pin the JSON.
  `Attachment::content` (m.image/m.file with `info`, MSC2530 caption) is for new callers.
- Small differences: email uploads per room kind inside the room loop (an upload failure now
  skips that room only, and nothing is uploaded when no room is joined); geoguessr's result map is
  not uploaded when the main room is unknown. No thumbnails (no bot sends any).
- email-bot health (package 7): `Alive::ok()` after a poll cycle without errors,
  `Alive::failed(reason)` when connect, TLS, LOGIN, SELECT, SEARCH or a FETCH fails. The reason
  is our own step description plus, from the cause, only the IMAP NO/BAD text (first line, at most
  200 characters), the I/O error kind or the TLS library's message; an unreadable server answer
  is named, never shown (async-imap's `DataNotUtf8` carries raw bytes), and the password is cut
  out should a server echo it. LOGIN's error keeps the server's answer as its source (the log's
  `error` no longer repeats it; `error_chain` does). A message that cannot be fetched cycle after
  cycle turns the container unhealthy after the loop's time (45 min up to a 300 s poll interval).

## Not planned (and why)

- **A fleet-wide persistent outbox.** Scheduled posts get at-least-once delivery from the
  schedule ledger plus stable transaction IDs (tuwunel remembers them per device), without a
  second store per bot. Live replies (commands, translations, games) are not worth queuing.
- **Per-user language in chat**, case-insensitive commands, one refusal wording everywhere,
  always setting `m.mentions`: they change what residents see; owner decisions.
- **Admin commands only from verified devices in rooms** (identity analysis, finding 1: nine
  places accept room admin commands by sender ID alone): not done. `command::Commands::parse_as`
  has the admin hook a `[security] room_admin_commands = any | trusted_device | console_only`
  setting would plug into; turning it on stops admins in the unencrypted game rooms, so it is an
  owner decision. `!reset-trust` and `!verify-device` (mxbot-common's own) are console-only now.
- **Metrics endpoint and collector**: needs a network decision (who may reach Prometheus).
- **One `App::start` for every main.rs**: churn in 14 bots for little gain; new bots can use it
  once it exists.
- **Room reconciliation beyond plenum** (INT-2/3): needs a decision on each room's audience.

## Notes

2. Package 2 (2026-10-30). `clock::Zone` and `schedule` (`Rule`, `Period`/`Slot`, `Outcome`,
   `Delivery`, `Ledger`/`JsonLedger`, `Job`, `Runner`, `run`, `post`) behind the feature
   `schedule`. calendar-bot keeps `store/posted.json` in its old shape (`open_dated`), so a
   rollback still reads it; radar-bot records its slots as `alerts` rows (`weather:<date>` as
   before, `digest:<date> <time>`). radar's alert transaction IDs now name the occurrence
   (DWD: the warning's end, a repost: the messages it replaces; NINA: the version's key),
   kept from the first attempt until every room has it (`alert_post_keys`), so a retry after
   DWD moved the end or NINA sent a new version posts no second copy. Behaviour changes: none
   intended for calendar-bot: a dated ledger counts a slot as handled only on its own date,
   as before (moving the time within the day posts nothing more, moving the weekly or
   monthly day posts on the new day, also within the same week; an overlap, never a gap),
   and its transaction keys name that date. radar's digest: the first start of this version
   records the latest digest slot as done instead of catching it up (the old version wrote
   no `digest:` rows, and had posted it). Not done: `Bot::broadcast_once` (bot.rs is shared; `schedule::post` covers it), calendar's
   catch-up after midnight rendering the new day (B10, it changes what residents see).
