//! Initial sync, the long-running sync loop and shutdown handling.

use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};

use matrix_sdk::{
    config::SyncSettings, ruma::api::client::filter::FilterDefinition, Client, LoopCtrl,
};
use tokio::{sync::watch, time::sleep};
use tracing::{info, warn};

use crate::retry::Backoff;

const RETRY_INITIAL: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(300);
/// A sync session that ran this long counts as healthy and resets backoff.
const HEALTHY_AFTER: Duration = Duration::from_secs(60);
/// After SIGTERM: how long a sync batch being handled may take to finish.
/// Under Docker's default 10 s before it kills the process.
pub const STOP_GRACE: Duration = Duration::from_secs(8);

/// Set once SIGTERM or SIGINT arrived (see [`listen_for_stop`]).
static STOP: OnceLock<watch::Sender<bool>> = OnceLock::new();

fn stop_flag() -> &'static watch::Sender<bool> {
    STOP.get_or_init(|| watch::channel(false).0)
}

/// Start listening for SIGTERM / SIGINT. [`crate::Bot`] does this right at
/// start, so a stop during startup is noticed too (a process without a
/// handler ignores SIGTERM as PID 1 and is killed after Docker's timeout).
pub fn listen_for_stop() {
    static LISTENING: OnceLock<()> = OnceLock::new();
    LISTENING.get_or_init(|| {
        tokio::spawn(async {
            shutdown_signal().await;
            stop_flag().send_replace(true);
        });
    });
}

/// Whether the bot was asked to stop.
pub fn stopping() -> bool {
    *stop_flag().borrow()
}

/// Resolves once the bot was asked to stop.
pub async fn stop_requested() {
    let mut rx = stop_flag().subscribe();
    let _ = rx.wait_for(|stop| *stop).await;
}

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
                crate::heartbeat::beat().await;
                return;
            }
            Err(error) => {
                let delay = backoff.next_delay();
                warn!(attempt, retry_in_secs = delay.as_secs(), %error, "Initial Matrix sync failed");
                tokio::select! {
                    () = sleep(delay) => {}
                    () = stop_requested() => return,
                }
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
/// `on_reconnect` is called each time the loop restarts. Every successful
/// sync response refreshes the optional [`crate::heartbeat`] file.
pub async fn sync_forever_with(client: &Client, mut on_reconnect: impl FnMut()) -> ! {
    let mut backoff = Backoff::new(RETRY_INITIAL, RETRY_MAX);
    loop {
        let started = Instant::now();
        let result = client
            .sync_with_callback(sync_settings(), |_| async {
                crate::heartbeat::beat().await;
                LoopCtrl::Continue
            })
            .await;
        match result {
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

/// [`sync_forever_with`], but ends once the bot is asked to stop, always
/// between two sync batches: the SDK saves the sync position *before* the
/// event handlers run, so an event whose handler is cut off is never
/// delivered again. A batch being handled finishes first (the next one is
/// not started); see [`crate::Bot::run_with`] for the time limit.
pub async fn sync_until_stopped(client: &Client, mut on_reconnect: impl FnMut()) {
    let mut backoff = Backoff::new(RETRY_INITIAL, RETRY_MAX);
    while !stopping() {
        let started = Instant::now();
        let result = client
            .sync_with_callback(sync_settings(), |_| async {
                crate::heartbeat::beat().await;
                if stopping() {
                    LoopCtrl::Break
                } else {
                    LoopCtrl::Continue
                }
            })
            .await;
        if stopping() {
            return;
        }
        match result {
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
        tokio::select! {
            () = sleep(delay) => {}
            () = stop_requested() => return,
        }
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
        _ = ctrl_c => info!("Received SIGINT. Shutting down"),
        _ = terminate => info!("Received SIGTERM. Shutting down"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_stop_reaches_everyone_waiting_and_later_checks() {
        let waiting = tokio::spawn(stop_requested());
        assert!(!stopping());
        stop_flag().send_replace(true);
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("woken")
            .unwrap();
        assert!(stopping());
        // Asked after the fact: resolves at once.
        tokio::time::timeout(Duration::from_secs(1), stop_requested())
            .await
            .expect("already stopping");
    }
}
