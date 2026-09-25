//! Initial sync and the long-running sync loop.

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
pub async fn sync_forever(client: &Client) -> ! {
    sync_forever_with(client, || {}).await
}

/// [`sync_forever`], calling `on_reconnect` each time the loop restarts.
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
