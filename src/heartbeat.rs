//! Optional liveness file for container health checks.
//!
//! When the environment variable [`HEARTBEAT_ENV`] names a file, the bot
//! writes the current Unix time (seconds) into it after every successful sync
//! response. A health check can then tell "connected and syncing" apart from
//! "process alive but not talking to the homeserver":
//!
//! ```sh
//! f="$MXBOT_HEARTBEAT_FILE"; [ -f "$f" ] && [ $(( $(date +%s) - $(cat "$f") )) -lt 120 ]
//! ```
//!
//! Long-polling sync returns at least every sync timeout (30 s by default),
//! so the file stays fresh even when nothing happens. Without the variable
//! nothing is written. Write errors are logged once and never stop the bot.
//!
//! While a loop started with [`crate::supervise::supervise_ticking`] has not
//! ticked in its time, or has failed every round for longer than that, the file
//! is not refreshed: the health check fails as for a dead sync, and
//! `<file>.reason` names the loop and why (the check can print it). Logged when
//! the set of unhealthy loops changes.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use tracing::{info, warn};

/// Environment variable naming the heartbeat file, e.g. `/app/store/.heartbeat`.
pub const HEARTBEAT_ENV: &str = "MXBOT_HEARTBEAT_FILE";

static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
static WARNED: AtomicBool = AtomicBool::new(false);

fn configured_path() -> Option<&'static PathBuf> {
    PATH.get_or_init(|| {
        std::env::var_os(HEARTBEAT_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    })
    .as_ref()
}

/// Record a successful sync, if a heartbeat file is configured and no supervised loop is
/// unhealthy. While one is, the file is left alone and `<file>.reason` says which and why
/// (for the health check's output); it is removed once all are healthy again.
pub async fn beat() {
    let unhealthy = crate::supervise::unhealthy();
    log_change(&unhealthy);
    let Some(path) = configured_path() else {
        return;
    };
    let reason = sibling(path, "reason");
    if !unhealthy.is_empty() {
        let text: Vec<String> = unhealthy
            .iter()
            .map(|u| format!("{}: {}", u.name, u.why))
            .collect();
        if let Err(error) = write(&reason, &text.join("; ")).await {
            warn_once(path, &error);
        }
        return;
    }
    match tokio::fs::remove_file(&reason).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => warn_once(path, &error),
    }
    if let Err(error) = write(path, &unix_now_secs().to_string()).await {
        warn_once(path, &error);
    }
}

fn warn_once(path: &Path, error: &std::io::Error) {
    if !WARNED.swap(true, Ordering::Relaxed) {
        warn!(path = %path.display(), %error, "Failed to write heartbeat file (further errors suppressed)");
    }
}

/// Say so when the set of unhealthy loops changes (not at every sync).
fn log_change(unhealthy: &[crate::supervise::Unhealthy]) {
    static LAST: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let names: Vec<String> = unhealthy.iter().map(|u| u.name.clone()).collect();
    let mut last = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if *last == names {
        return;
    }
    if names.is_empty() {
        info!("Background tasks healthy again; heartbeat resumed");
    } else {
        let detail: Vec<String> = unhealthy
            .iter()
            .map(|u| format!("{} ({})", u.name, u.why))
            .collect();
        warn!(unhealthy = %detail.join(", "), "Background task unhealthy; heartbeat withheld");
    }
    *last = names;
}

/// `path` with `.suffix` appended to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

/// Write `text` to `path` atomically (temporary file plus rename), so a
/// health check never reads a half-written value.
async fn write(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = sibling(path, "tmp");
    tokio::fs::write(&tmp, text).await?;
    tokio::fs::rename(&tmp, path).await
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_replaces_the_file_atomically() {
        let dir = std::env::temp_dir().join(format!("mxbot-heartbeat-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let file = dir.join(".heartbeat");

        write(&file, "42").await.unwrap();
        write(&file, "1700000000").await.unwrap();

        assert_eq!(
            tokio::fs::read_to_string(&file).await.unwrap(),
            "1700000000"
        );
        assert!(!dir.join(".heartbeat.tmp").exists());
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
