//! The bot's status file: how its tasks, queues and Matrix sync are doing, for monitoring.
//!
//! When the environment variable [`STATUS_ENV`] names a file (e.g. `/app/status/status.json`),
//! [`crate::Bot`] writes it at start and every [`WRITE_EVERY`], atomically, whether the
//! homeserver answers or not: a monitoring exporter reads only this file (never the store) and
//! can tell "Matrix is down" (old `matrix.last_sync_ok`, every bot) from "a task is broken"
//! (one task's `last_err`, `consecutive_failures`) and from "the bot is frozen or dead" (an old
//! `written_at`). Without the variable nothing is written.
//!
//! What goes in:
//! * tasks: every [`crate::supervise`]d loop and [`crate::schedule`] job, and every
//!   [`crate::supervise::report`]: last round, last success, last error with a short
//!   sanitised reason ([`sanitize_reason`]), failures in a row, totals per outcome, the
//!   expected interval and the next due time where declared;
//! * Matrix: the last successful sync and the failed sync attempts since ([`sync_ok`],
//!   [`sync_failed`], called by [`crate::sync`]);
//! * queues: work waiting to be sent ([`queue`]), with the age of the oldest item;
//! * gauges: a few named numbers of the bot's own ([`gauge`]).
//!
//! Schema (version 1), timestamps in Unix seconds, `null` when unknown:
//!
//! ```json
//! {"version": 1, "bot": "email-bot", "pid": 7, "started_at": 1761900000, "written_at": 1761900030,
//!  "matrix": {"last_sync_ok": 1761900029, "sync_failures": 0},
//!  "tasks": [{"name": "imap sync", "kind": "ticking", "expected_interval_s": 300, "within_s": 600,
//!             "last_tick": 1761900020, "last_ok": 1761900020, "last_err": null, "last_err_reason": null,
//!             "consecutive_failures": 0, "ok_total": 3, "failed_total": 0, "idle_total": 10,
//!             "gave_up_total": 0, "last_gave_up": null, "unavailable_total": 0,
//!             "last_outcome": "idle", "next_due": null}],
//!  "queues": [{"name": "email retry", "pending": 0, "oldest_age_s": null}],
//!  "gauges": [{"name": "members_held_rooms", "value": 2.0}]}
//! ```
//!
//! `last_ok` is the last healthy round, [`Alive::ok`](crate::supervise::Alive::ok) or
//! [`Alive::idle`](crate::supervise::Alive::idle); `gave_up_total` is counted in
//! `failed_total` too, `last_gave_up` is the last give-up (seen even when it came before a
//! reader's first look at the totals). `unavailable_total` counts the rounds another service
//! was down ([`Alive::unavailable`](crate::supervise::Alive::unavailable), in `failed_total`
//! too; `last_outcome` "unavailable"). New fields may be added within version 1
//! (`last_gave_up`, `unavailable_total` were); readers ignore unknown ones.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use tracing::warn;

use crate::supervise::{self, TaskStatus};

/// Environment variable naming the status file, e.g. `/app/status/status.json`.
pub const STATUS_ENV: &str = "MXBOT_STATUS_FILE";

/// How often the file is written.
pub const WRITE_EVERY: Duration = Duration::from_secs(30);

/// The schema version in the file.
pub const VERSION: u32 = 1;

/// The longest reason kept, in characters.
pub const REASON_MAX: usize = 160;

static STARTED_AT: OnceLock<SystemTime> = OnceLock::new();

fn started_at() -> SystemTime {
    *STARTED_AT.get_or_init(SystemTime::now)
}

/// Start writing the status file named by [`STATUS_ENV`] for `bot`: now and every
/// [`WRITE_EVERY`]. Nothing without the variable; a second call does nothing. Write errors
/// are logged once and never stop the bot. [`crate::Bot`] calls this at start.
pub fn start(bot: impl Into<String>) {
    static STARTED: OnceLock<()> = OnceLock::new();
    started_at();
    let Some(path) = std::env::var_os(STATUS_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    else {
        return;
    };
    let bot = bot.into();
    STARTED.get_or_init(|| {
        tokio::spawn(async move {
            let warned = AtomicBool::new(false);
            loop {
                if let Err(error) = write(&path, &bot).await {
                    if !warned.swap(true, Ordering::Relaxed) {
                        warn!(path = %path.display(), "Failed to write the status file (further errors suppressed): {error:#}");
                    }
                }
                tokio::time::sleep(WRITE_EVERY).await;
            }
        });
    });
}

/// Write the status of `bot` to `path` now, atomically, readable by everyone (0644): the
/// monitoring exporter reads it as another user, and it holds nothing secret.
///
/// A temporary file next to it, given its mode before the rename (the exporter never sees the
/// file without it), then renamed over the old one. Never synced to disk: the file is
/// rewritten every [`WRITE_EVERY`] and only describes the running process, so a file lost or
/// cut short by a power loss costs nothing (the exporter reports it unreadable until the next
/// write), while an fsync every 30 s per bot would wear the cache drive for no gain.
pub async fn write(path: &Path, bot: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::sync::atomic::AtomicU64;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent).await?;
    }
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = PathBuf::from(tmp);
    let data = serde_json::to_vec_pretty(&document(bot, SystemTime::now()))?;
    let written = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o644);
        let mut file = options.open(&tmp).await?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &data).await?;
        // tokio writes in the background: only flush reports a failed write (a full disk).
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        drop(file);
        // The umask may have taken bits away at creation.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).await?;
        }
        tokio::fs::rename(&tmp, path).await
    }
    .await;
    if let Err(error) = written {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(error).with_context(|| format!("Writing {}", path.display()));
    }
    Ok(())
}

// ── Matrix ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MatrixState {
    last_sync_ok: Option<SystemTime>,
    sync_failures: u64,
}

fn matrix_state() -> &'static Mutex<MatrixState> {
    static MATRIX: Mutex<MatrixState> = Mutex::new(MatrixState {
        last_sync_ok: None,
        sync_failures: 0,
    });
    &MATRIX
}

/// A sync response arrived: the homeserver answers. Ends a run of failed attempts.
pub fn sync_ok() {
    let mut state = lock(matrix_state());
    state.last_sync_ok = Some(SystemTime::now());
    state.sync_failures = 0;
}

/// A sync attempt failed (the first sync, or the sync loop gave up and reconnects).
pub fn sync_failed() {
    lock(matrix_state()).sync_failures += 1;
}

/// The last successful sync and the failed attempts since.
pub fn matrix() -> (Option<SystemTime>, u64) {
    let state = *lock(matrix_state());
    (state.last_sync_ok, state.sync_failures)
}

// ── Queues ────────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct QueueState {
    pending: u64,
    /// The oldest item's age when it was set, and when it was set: the file shows its age at
    /// the time of writing.
    oldest: Option<(Duration, Instant)>,
}

type Named<T> = Mutex<Vec<(String, Arc<Mutex<T>>)>>;

fn queues() -> &'static Named<QueueState> {
    static QUEUES: Named<QueueState> = Mutex::new(Vec::new());
    &QUEUES
}

/// Work waiting to be sent (an outbox, a retry queue), in the status file under `name`.
/// The same name gives the same queue.
#[derive(Clone, Debug)]
pub struct Queue(Arc<Mutex<QueueState>>);

/// The queue named `name`, registered on first use (empty).
pub fn queue(name: impl Into<String>) -> Queue {
    Queue(named(queues(), name.into()))
}

impl Queue {
    /// `pending` items wait; the oldest has waited `oldest_age` (`None` when empty or unknown).
    /// The file adds the time since this call, so a queue set once an hour still shows the
    /// right age.
    pub fn set(&self, pending: u64, oldest_age: Option<Duration>) {
        let mut state = lock(&self.0);
        state.pending = pending;
        state.oldest = oldest_age.map(|age| (age, Instant::now()));
    }

    /// [`set`](Self::set) from the oldest item's creation time.
    pub fn set_since(&self, pending: u64, oldest: Option<SystemTime>) {
        let age = oldest.map(|at| SystemTime::now().duration_since(at).unwrap_or_default());
        self.set(pending, age);
    }
}

// ── Gauges ────────────────────────────────────────────────────────────────────

fn gauges() -> &'static Named<f64> {
    static GAUGES: Named<f64> = Mutex::new(Vec::new());
    &GAUGES
}

/// A number of the bot's own (rooms held, members waiting), in the status file under `name`.
/// The same name gives the same gauge.
#[derive(Clone, Debug)]
pub struct Gauge(Arc<Mutex<f64>>);

/// The gauge named `name`, registered on first use (0).
pub fn gauge(name: impl Into<String>) -> Gauge {
    Gauge(named(gauges(), name.into()))
}

impl Gauge {
    pub fn set(&self, value: f64) {
        *lock(&self.0) = value;
    }
}

fn named<T: Default>(registry: &Named<T>, name: String) -> Arc<Mutex<T>> {
    let mut registry = lock(registry);
    if let Some((_, value)) = registry.iter().find(|(n, _)| *n == name) {
        return Arc::clone(value);
    }
    let value = Arc::new(Mutex::new(T::default()));
    registry.push((name, Arc::clone(&value)));
    value
}

// ── The document ──────────────────────────────────────────────────────────────

/// The status file's contents.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Document {
    pub version: u32,
    pub bot: String,
    pub pid: u32,
    pub started_at: Option<u64>,
    pub written_at: Option<u64>,
    pub matrix: MatrixDoc,
    pub tasks: Vec<TaskDoc>,
    pub queues: Vec<QueueDoc>,
    pub gauges: Vec<GaugeDoc>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct MatrixDoc {
    pub last_sync_ok: Option<u64>,
    pub sync_failures: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct TaskDoc {
    pub name: String,
    pub kind: &'static str,
    pub expected_interval_s: Option<u64>,
    pub within_s: Option<u64>,
    pub last_tick: Option<u64>,
    pub last_ok: Option<u64>,
    pub last_err: Option<u64>,
    pub last_err_reason: Option<String>,
    pub consecutive_failures: u64,
    pub ok_total: u64,
    pub failed_total: u64,
    pub idle_total: u64,
    pub gave_up_total: u64,
    pub last_gave_up: Option<u64>,
    pub unavailable_total: u64,
    pub last_outcome: Option<&'static str>,
    pub next_due: Option<u64>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct QueueDoc {
    pub name: String,
    pub pending: u64,
    pub oldest_age_s: Option<u64>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct GaugeDoc {
    pub name: String,
    pub value: f64,
}

/// The status of this process as of `now`.
pub fn document(bot: &str, now: SystemTime) -> Document {
    let (last_sync_ok, sync_failures) = matrix();
    let queues = lock(queues())
        .iter()
        .map(|(name, state)| {
            let state = lock(state);
            QueueDoc {
                name: name.clone(),
                pending: state.pending,
                oldest_age_s: state.oldest.map(|(age, at)| (age + at.elapsed()).as_secs()),
            }
        })
        .collect();
    let gauges = lock(gauges())
        .iter()
        .map(|(name, value)| GaugeDoc {
            name: name.clone(),
            value: *lock(value),
        })
        .filter(|gauge| gauge.value.is_finite())
        .collect();
    Document {
        version: VERSION,
        bot: bot.to_owned(),
        pid: std::process::id(),
        started_at: unix(Some(started_at())),
        written_at: unix(Some(now)),
        matrix: MatrixDoc {
            last_sync_ok: unix(last_sync_ok),
            sync_failures,
        },
        tasks: supervise::snapshot().into_iter().map(task_doc).collect(),
        queues,
        gauges,
    }
}

fn task_doc(task: TaskStatus) -> TaskDoc {
    TaskDoc {
        name: task.name,
        kind: task.kind.as_str(),
        expected_interval_s: task.expected.map(|d| d.as_secs()),
        within_s: task.within.map(|d| d.as_secs()),
        last_tick: unix(task.last_tick),
        last_ok: unix(task.last_ok),
        last_err: unix(task.last_err),
        last_err_reason: task.last_err_reason,
        consecutive_failures: task.consecutive_failures,
        ok_total: task.ok_total,
        failed_total: task.failed_total,
        idle_total: task.idle_total,
        gave_up_total: task.gave_up_total,
        last_gave_up: unix(task.last_gave_up),
        unavailable_total: task.unavailable_total,
        last_outcome: task.last_outcome.map(supervise::Outcome::as_str),
        next_due: unix(task.next_due),
    }
}

fn unix(at: Option<SystemTime>) -> Option<u64> {
    at.and_then(|at| at.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

// ── Reasons ───────────────────────────────────────────────────────────────────

/// Parts of a name that make it a secret's key in `key=value` (anywhere in the name).
const SECRET_PARTS: [&str; 10] = [
    "token", "secret", "password", "passwd", "pass", "key", "auth", "sig", "session", "cookie",
];

/// Names that make it a secret's key in `key: value` too (the whole name, any case).
const SECRET_NAMES: [&str; 16] = [
    "token",
    "access_token",
    "password",
    "passwd",
    "pass",
    "secret",
    "api_key",
    "apikey",
    "x-api-key",
    "authorization",
    "auth",
    "bearer",
    "basic",
    "cookie",
    "session",
    "key",
];

/// Endings that make a name a secret's key in `key: value` (`refresh_token`, `client_secret`,
/// `private_key`, `Proxy-Authorization`, `Set-Cookie`).
const SECRET_ENDINGS: [&str; 9] = [
    "token",
    "password",
    "passwd",
    "secret",
    "_key",
    "-key",
    "apikey",
    "cookie",
    "authorization",
];

/// What ends a value without quotes: `password=x&next`, `{"token":x,"next"}`, `key: x y`.
const VALUE_ENDS: [char; 9] = [' ', ',', ';', '&', '}', ']', ')', '"', '\''];

/// What ends a cookie header's value without quotes (`Cookie: a=1; b=2`): only a quote.
const COOKIE_ENDS: [char; 2] = ['"', '\''];

/// `raw` (an error message) made safe and short for a status file and a health check: one
/// line; URLs without user info, query or fragment; the value of every secret-ish key
/// (`token`, `password`, `api_key`, `Authorization`, … followed by `:` or `=`, with or
/// without spaces and quotes, any case, as in `{"access_token":"…"}` or `password:x`) and
/// `Bearer`/`Basic` credentials replaced by `<redacted>`, long token-like words too; at most
/// [`REASON_MAX`] characters.
pub fn sanitize_reason(raw: &str) -> String {
    let mut tokens = Vec::new();
    let mut rest = raw;
    while let Some(at) = rest.find("://") {
        let scheme_start = rest[..at]
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            .last()
            .map_or(at, |(i, _)| i);
        if scheme_start == at {
            lex(&rest[..at + 3], &mut tokens);
            rest = &rest[at + 3..];
            continue;
        }
        lex(&rest[..scheme_start], &mut tokens);
        let end = rest[at + 3..]
            .find(|c: char| {
                c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '<' | '>' | '`')
            })
            .map_or(rest.len(), |i| at + 3 + i);
        tokens.push(Token::Done(clean_url(
            &rest[scheme_start..at],
            &rest[at + 3..end],
        )));
        rest = &rest[end..];
    }
    lex(rest, &mut tokens);
    truncate(redact(&tokens).trim(), REASON_MAX)
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Word(String),
    /// One separator; white space of any length is one `' '`.
    Sep(char),
    /// Already cleaned (a URL).
    Done(String),
}

/// Words and the separators between them: white space, quotes, brackets, braces, parentheses,
/// `,` `:` `;` `=` `&` `?`.
fn lex(text: &str, out: &mut Vec<Token>) {
    let mut word = String::new();
    for c in text.chars() {
        let sep = if c.is_whitespace() || c.is_control() {
            Some(' ')
        } else if matches!(
            c,
            '"' | '\''
                | '`'
                | '{'
                | '}'
                | '['
                | ']'
                | '('
                | ')'
                | ','
                | ':'
                | ';'
                | '='
                | '&'
                | '?'
        ) {
            Some(c)
        } else {
            None
        };
        match sep {
            None => word.push(c),
            Some(sep) => {
                if !word.is_empty() {
                    out.push(Token::Word(std::mem::take(&mut word)));
                }
                if !(sep == ' ' && out.last() == Some(&Token::Sep(' '))) {
                    out.push(Token::Sep(sep));
                }
            }
        }
    }
    if !word.is_empty() {
        out.push(Token::Word(word));
    }
}

fn redact(tokens: &[Token]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            Token::Sep(c) => {
                out.push(*c);
                i += 1;
            }
            Token::Done(text) => {
                out.push_str(text);
                i += 1;
            }
            Token::Word(word) => {
                if let Some(value) = secret_value_at(tokens, i) {
                    out.push_str(word);
                    for between in &tokens[i + 1..value] {
                        if let Token::Sep(c) = between {
                            out.push(*c);
                        }
                    }
                    // A cookie header's value is a list of pairs: all of it, to its end.
                    let ends: &[char] = if word.to_ascii_lowercase().contains("cookie") {
                        &COOKIE_ENDS
                    } else {
                        &VALUE_ENDS
                    };
                    i = redact_value(tokens, value, ends, &mut out);
                } else if let Some(value) = credential_at(tokens, i) {
                    out.push_str(word);
                    out.push(' ');
                    i = redact_value(tokens, value, &VALUE_ENDS, &mut out);
                } else {
                    out.push_str(if looks_like_token(word) {
                        "<redacted>"
                    } else {
                        word
                    });
                    i += 1;
                }
            }
        }
    }
    out
}

/// When the word at `i` is a secret's key (followed by quotes or spaces, then `:` or `=`,
/// then spaces): where its value starts.
fn secret_value_at(tokens: &[Token], i: usize) -> Option<usize> {
    let Token::Word(word) = &tokens[i] else {
        return None;
    };
    let name = word
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    let mut j = i + 1;
    while matches!(tokens.get(j), Some(Token::Sep('"' | '\'' | '`' | ' '))) {
        j += 1;
    }
    let secret = match tokens.get(j) {
        Some(Token::Sep(':')) => secret_name(&name),
        Some(Token::Sep('=')) => {
            secret_name(&name) || SECRET_PARTS.iter().any(|part| name.contains(part))
        }
        _ => false,
    };
    if !secret {
        return None;
    }
    j += 1;
    while matches!(tokens.get(j), Some(Token::Sep(' '))) {
        j += 1;
    }
    Some(j)
}

fn secret_name(name: &str) -> bool {
    SECRET_NAMES.contains(&name)
        || SECRET_ENDINGS.iter().any(|end| name.ends_with(end))
        || name.starts_with("session")
}

/// When the word at `i` is `Bearer` or `Basic` followed by a space and a value: where the
/// value starts.
fn credential_at(tokens: &[Token], i: usize) -> Option<usize> {
    let Some(Token::Word(word)) = tokens.get(i) else {
        return None;
    };
    let scheme = word.eq_ignore_ascii_case("bearer") || word.eq_ignore_ascii_case("basic");
    (scheme
        && tokens.get(i + 1) == Some(&Token::Sep(' '))
        && matches!(tokens.get(i + 2), Some(Token::Word(_) | Token::Done(_))))
    .then_some(i + 2)
}

/// The value at `start` replaced by `<redacted>` (quoted: up to its closing quote, which is
/// kept; a `Bearer`/`Basic` before the credential is kept too); returns where it ends.
fn redact_value(tokens: &[Token], start: usize, ends: &[char], out: &mut String) -> usize {
    if let Some(Token::Sep(quote @ ('"' | '\'' | '`'))) = tokens.get(start) {
        out.push(*quote);
        let mut j = start + 1;
        while j < tokens.len() && tokens[j] != Token::Sep(*quote) {
            j += 1;
        }
        if j > start + 1 {
            out.push_str("<redacted>");
        }
        if j < tokens.len() {
            out.push(*quote);
            j += 1;
        }
        return j;
    }
    let mut start = start;
    if let Some(value) = credential_at(tokens, start) {
        if let Token::Word(scheme) = &tokens[start] {
            out.push_str(scheme);
            out.push(' ');
        }
        start = value;
    }
    let mut j = start;
    while let Some(token) = tokens.get(j) {
        if matches!(token, Token::Sep(c) if ends.contains(c)) {
            break;
        }
        j += 1;
    }
    if j > start {
        out.push_str("<redacted>");
    }
    j
}

/// `scheme://user:pass@host/path?query#fragment` → `scheme://host/path`, the host and the
/// path's parts cleaned as any text. The user info ends at the last `@` before the query, so a `/` in a
/// password does not leave the rest of it as the host.
fn clean_url(scheme: &str, rest: &str) -> String {
    let rest = &rest[..rest.find(['?', '#']).unwrap_or(rest.len())];
    let rest = rest.rfind('@').map_or(rest, |at| &rest[at + 1..]);
    // The host and each part of the path (a host never holds a secret, but text glued to it
    // without a space might).
    let parts: Vec<String> = rest
        .split('/')
        .map(|part| {
            let mut tokens = Vec::new();
            lex(part, &mut tokens);
            redact(&tokens)
        })
        .collect();
    format!("{scheme}://{}", parts.join("/"))
}

/// 24+ characters of letters and digits (both) and `_-+/=.`: an access token, an API key, a
/// signature.
fn looks_like_token(word: &str) -> bool {
    let word = word.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ';' | ')' | '('));
    word.chars().count() >= 24
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '/' | '=' | '.'))
        && word.chars().any(|c| c.is_ascii_digit())
        && word.chars().any(|c| c.is_ascii_alphabetic())
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(max - 1).collect();
    short.push('…');
    short
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task<'a>(doc: &'a Document, name: &str) -> &'a TaskDoc {
        doc.tasks
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("task {name} listed"))
    }

    #[test]
    fn reasons_lose_credentials_and_stay_short() {
        assert_eq!(sanitize_reason("LOGIN refused"), "LOGIN refused");
        assert_eq!(
            sanitize_reason(
                "error sending request for url (https://user:hunter2@imap.example.org/x?access_token=syt_abc#f)"
            ),
            "error sending request for url (https://imap.example.org/x"
        );
        assert_eq!(
            sanitize_reason("GET /api?api_key=abc&city=Berlin failed"),
            "GET /api?api_key=<redacted>&city=Berlin failed"
        );
        assert_eq!(
            sanitize_reason("Authorization: Bearer syt_Zm9vYmFy_abcdef"),
            "Authorization: Bearer <redacted>"
        );
        assert_eq!(
            sanitize_reason("password: hunter2 wrong"),
            "password: <redacted> wrong"
        );
        assert_eq!(
            sanitize_reason("token mfy_0123456789abcdefABCDEF0123 expired"),
            "token <redacted> expired"
        );
        assert_eq!(
            sanitize_reason("https://graph.example/v1/a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4/images"),
            "https://graph.example/v1/<redacted>/images"
        );
        assert_eq!(sanitize_reason("two\nlines\t here"), "two lines here");
        let long = sanitize_reason(&"word ".repeat(100));
        assert_eq!(long.chars().count(), REASON_MAX);
        assert!(long.ends_with('…'));
        // Room and event IDs, plain words and short numbers stay readable.
        assert_eq!(
            sanitize_reason("M_FORBIDDEN in !abc:example.org (HTTP 403)"),
            "M_FORBIDDEN in !abc:example.org (HTTP 403)"
        );
    }

    #[test]
    fn every_shape_of_secret_loses_its_value() {
        for (raw, clean) in [
            (
                r#"{"access_token":"syt_abc","user":"anna"}"#,
                r#"{"access_token":"<redacted>","user":"anna"}"#,
            ),
            (
                r#"{"token": "two words", "next": 3}"#,
                r#"{"token": "<redacted>", "next": 3}"#,
            ),
            ("password:hunter2", "password:<redacted>"),
            (
                "PASSWORD = hunter2 rejected",
                "PASSWORD = <redacted> rejected",
            ),
            ("'passwd'='hunter 2' x", "'passwd'='<redacted>' x"),
            (
                "Authorization:Bearer x then",
                "Authorization:Bearer <redacted> then",
            ),
            (
                "authorization: Basic dXNlcjpw",
                "authorization: Basic <redacted>",
            ),
            ("Basic dXNlcjpw refused", "Basic <redacted> refused"),
            ("secret: s3cr3t, retry", "secret: <redacted>, retry"),
            ("api_key: abc123 invalid", "api_key: <redacted> invalid"),
            ("apikey=abc123", "apikey=<redacted>"),
            ("X-Api-Key: abc123", "X-Api-Key: <redacted>"),
            ("x-api-key:abc:def", "x-api-key:<redacted>"),
            (
                "client_secret=abc&grant=x",
                "client_secret=<redacted>&grant=x",
            ),
            ("refresh_token: r1", "refresh_token: <redacted>"),
            ("session: s1 expired", "session: <redacted> expired"),
            ("Cookie: sid=1; theme=dark", "Cookie: <redacted>"),
            ("key = k1", "key = <redacted>"),
            ("https://user:pa/ss@host", "https://host"),
            (
                "https://user:pa/ss@host/x?y=1#z done",
                "https://host/x done",
            ),
            (
                "url https://u:p@h:8443/a/b,password:x",
                "url https://h:8443/a/b,password:<redacted>",
            ),
        ] {
            assert_eq!(sanitize_reason(raw), clean, "{raw}");
        }
        // Ordinary messages with colons and equals signs stay as they are.
        for plain in [
            "error: connection refused at 12:30",
            "M_LIMIT_EXCEEDED retry_after_ms=2000",
            "@anna:example.org left !room:example.org",
            "token expired",
        ] {
            assert_eq!(sanitize_reason(plain), plain);
        }
        // Still bounded and on one line.
        let long = sanitize_reason(&format!("{{\"password\":\"x\"}}\n{}", "y ".repeat(200)));
        assert!(long.starts_with(r#"{"password":"<redacted>"} y"#));
        assert_eq!(long.chars().count(), REASON_MAX);
    }

    #[test]
    fn the_document_lists_tasks_queues_gauges_and_the_sync() {
        let alive = supervise::report("status doc under test");
        alive.expect_every(Duration::from_secs(600));
        alive.ok();
        alive.failed("IMAP https://u:p@mail.example/?x=1 down");
        alive.failed("still down");
        let outbox = queue("outbox under test");
        outbox.set(3, Some(Duration::from_secs(120)));
        gauge("held under test").set(2.0);
        gauge("nan under test").set(f64::NAN);
        sync_failed();
        sync_ok();
        sync_failed();

        let now = SystemTime::now();
        let doc = document("test-bot", now);
        assert_eq!(doc.version, 1);
        assert_eq!(doc.bot, "test-bot");
        assert_eq!(doc.pid, std::process::id());
        assert!(doc.started_at <= doc.written_at);
        assert!(doc.matrix.last_sync_ok.is_some());
        assert!(doc.matrix.sync_failures >= 1);

        let t = task(&doc, "status doc under test");
        assert_eq!(t.kind, "reported");
        assert_eq!(t.expected_interval_s, Some(600));
        assert_eq!(t.within_s, None);
        assert_eq!((t.ok_total, t.failed_total, t.idle_total), (1, 2, 0));
        assert_eq!(t.consecutive_failures, 2);
        assert_eq!(t.last_outcome, Some("failed"));
        assert_eq!(t.last_err_reason.as_deref(), Some("still down"));
        assert!(t.last_ok.is_some() && t.last_err.is_some());
        assert_eq!(t.last_gave_up, None);

        let q = doc
            .queues
            .iter()
            .find(|q| q.name == "outbox under test")
            .unwrap();
        assert_eq!(q.pending, 3);
        assert!(q.oldest_age_s.unwrap() >= 120);
        assert!(doc
            .gauges
            .iter()
            .any(|g| g.name == "held under test" && g.value == 2.0));
        assert!(
            !doc.gauges.iter().any(|g| g.name == "nan under test"),
            "not a number"
        );

        // The same name is the same queue / gauge / report.
        queue("outbox under test").set(0, None);
        let doc = document("test-bot", now);
        let q = doc
            .queues
            .iter()
            .find(|q| q.name == "outbox under test")
            .unwrap();
        assert_eq!((q.pending, q.oldest_age_s), (0, None));
        supervise::report("status doc under test").idle();
        let doc = document("test-bot", now);
        let t = task(&doc, "status doc under test");
        assert_eq!((t.idle_total, t.consecutive_failures), (1, 0));
        assert_eq!(t.last_outcome, Some("idle"));
        assert_eq!(
            t.last_err_reason.as_deref(),
            Some("still down"),
            "the last error stays"
        );
    }

    #[tokio::test]
    async fn the_file_is_json_in_the_documented_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.json");
        supervise::report("file shape under test").gave_up("slot 2026-10-30 given up");
        write(&path, "shape-bot").await.unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for key in [
            "version",
            "bot",
            "pid",
            "started_at",
            "written_at",
            "matrix",
            "tasks",
            "queues",
            "gauges",
        ] {
            assert!(value.get(key).is_some(), "{key}");
        }
        assert!(value["matrix"].get("last_sync_ok").is_some());
        assert!(value["matrix"]["sync_failures"].is_u64());
        let task = value["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "file shape under test")
            .unwrap();
        for key in [
            "kind",
            "expected_interval_s",
            "within_s",
            "last_tick",
            "last_ok",
            "last_err",
            "last_err_reason",
            "consecutive_failures",
            "ok_total",
            "failed_total",
            "idle_total",
            "gave_up_total",
            "last_gave_up",
            "last_outcome",
            "next_due",
        ] {
            assert!(task.get(key).is_some(), "{key}");
        }
        assert_eq!(task["last_outcome"], "gave_up");
        assert_eq!(task["gave_up_total"], 1);
        assert!(task["last_gave_up"].is_u64());
        assert_eq!(task["failed_total"], 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o644, "readable by the exporter");
        }
        // Written again over the old one, no temporary file left.
        write(&path, "shape-bot").await.unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("status.json")]);
    }
}
