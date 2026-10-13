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

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        OnceLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use tracing::warn;

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

/// Record a successful sync, if a heartbeat file is configured.
pub async fn beat() {
    let Some(path) = configured_path() else {
        return;
    };
    if let Err(error) = write(path, unix_now_secs()).await {
        if !WARNED.swap(true, Ordering::Relaxed) {
            warn!(path = %path.display(), %error, "Failed to write heartbeat file (further errors suppressed)");
        }
    }
}

/// Write `secs` to `path` atomically (temporary file plus rename), so a
/// health check never reads a half-written value.
async fn write(path: &Path, secs: u64) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    tokio::fs::write(&tmp, secs.to_string()).await?;
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

        write(&file, 42).await.unwrap();
        write(&file, 1_700_000_000).await.unwrap();

        assert_eq!(tokio::fs::read_to_string(&file).await.unwrap(), "1700000000");
        assert!(!dir.join(".heartbeat.tmp").exists());
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
