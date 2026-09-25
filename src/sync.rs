//! Initial sync, the long-running sync loop and shutdown handling.

use std::time::{Duration, Instant};

use matrix_sdk::{config::SyncSettings, ruma::api::client::filter::FilterDefinition, Client};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::retry::Backoff;

const RETRY_INITIAL: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(300);
/// A sync session that ran this long counts as healthy and resets backoff.
const HEALTHY_AFTER: Duration = Duration::from_secs(60);

/// Sync settings with lazy-loaded members, used by all bots.
pub fn sync_settings() -> SyncSettings {
    SyncSettings::default().filter(FilterDefinition::with_lazy_loading().into())
}

/// Run the first sync, retrying with backoff until it succeeds.
///
/// Never gives up: background tasks (schedulers, pollers) keep running
/// while the homeserver is unreachable, and the bot catches up once it is
/// back.
pub async fn initial_sync(client: &Client) {
    let mut backoff = Backoff::new(RETRY_INITIAL, RETRY_MAX);
    let mut attempt = 1u32;
    loop {
        match client.sync_once(sync_settings()).await {
            Ok(_) => {
                info!(attempt, "Initial Matrix sync completed");
                return;
            }
            Err(error) => {
                let delay = backoff.next_delay();
                warn!(attempt, retry_in_secs = delay.as_secs(), %error, "Initial Matrix sync failed");
                sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

/// Sync forever, reconnecting with backoff that resets after a healthy run.
///
/// `client.sync()` already retries transient errors internally; returning
/// means it gave up (e.g. an extended homeserver outage).
///
/// `on_reconnect` is called each time the loop restarts.
pub async fn sync_forever_with(client: &Client, mut on_reconnect: impl FnMut()) -> ! {
    let mut backoff = Backoff::new(RETRY_INITIAL, RETRY_MAX);
    loop {
        let started = Instant::now();
        match client.sync(sync_settings()).await {
            Ok(()) => warn!("Matrix sync loop exited cleanly; reconnecting"),
            Err(error) => warn!(%error, "Matrix sync loop failed; reconnecting"),
        }
        on_reconnect();
        if started.elapsed() >= HEALTHY_AFTER {
            backoff.reset();
        }
        let delay = backoff.next_delay();
        warn!(
            retry_in_secs = delay.as_secs(),
            "Waiting before Matrix sync reconnect"
        );
        sleep(delay).await;
    }
}

/// Resolves on SIGTERM (what `docker stop` sends) or SIGINT (Ctrl-C).
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            warn!(%error, "Failed to listen for SIGINT");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                warn!(%error, "Failed to listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("Received SIGINT — shutting down"),
        _ = terminate => info!("Received SIGTERM — shutting down"),
    }
}
