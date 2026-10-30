//! Supervised background tasks: a bot's loops that must run as long as the bot does.
//!
//! A loop spawned with plain `tokio::spawn` can end (a bug, a closed channel, a panic caught
//! by tokio) while the sync keeps the heartbeat fresh: the container stays "healthy" and the
//! bot is half dead. Spawned with [`supervise`] instead, its end, error or panic is logged and
//! ends the process; the container's restart policy starts the bot again, and health-bot sees
//! the restart. Bots build with `panic = "abort"` too, so a panic anywhere does the same.
//!
//! A loop that works in rounds (every minute, every few minutes) can also hang, waiting for an
//! answer that never comes. [`supervise_ticking`] gives it an [`Alive`] to [`tick`](Alive::tick)
//! once per round: when no tick came for the given time, [`crate::heartbeat`] stops refreshing
//! the heartbeat file, the health check fails and health-bot reports the container unhealthy.
//! A loop that sleeps until a date (a monthly summary) uses [`supervise`] without ticks.
//! A loop that can fail a round reports it with [`Alive::ok`] / [`Alive::failed`]: failing for
//! longer than its time counts as unhealthy too.
//!
//! Not for tasks that end by design (per request, one-shot, a test mode that posts once).

use std::{
    future::Future,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use tokio::task::JoinHandle;
use tracing::{error, info};

/// How a supervised task ended: `()` or a `Result` whose error is shown.
pub trait TaskEnd {
    fn describe(self) -> String;
}

impl TaskEnd for () {
    fn describe(self) -> String {
        "returned".to_owned()
    }
}

impl<E: std::fmt::Display> TaskEnd for Result<(), E> {
    fn describe(self) -> String {
        match self {
            Ok(()) => "returned".to_owned(),
            Err(e) => format!("failed: {e:#}"),
        }
    }
}

/// Run `task` for the rest of the bot's life: if it ever ends, returns an error or panics, the
/// process exits (status 1) and the container is restarted.
pub fn supervise<Fut>(name: impl Into<String>, task: Fut) -> JoinHandle<()>
where
    Fut: Future + Send + 'static,
    Fut::Output: TaskEnd + Send,
{
    watch(name.into(), task, exit)
}

/// As [`supervise`], for a loop that works in rounds: `task` gets an [`Alive`] to tick once
/// per round. No tick for `within` (counted from the start, then from the last tick): the
/// heartbeat stops until the next tick. Choose `within` well above a round's longest normal
/// duration plus its pause.
pub fn supervise_ticking<F, Fut>(
    name: impl Into<String>,
    within: Duration,
    task: F,
) -> JoinHandle<()>
where
    F: FnOnce(Alive) -> Fut,
    Fut: Future + Send + 'static,
    Fut::Output: TaskEnd + Send,
{
    let name = name.into();
    let alive = register(name.clone(), within);
    watch(name, task(alive), exit)
}

/// A ticking task's handle: [`tick`](Self::tick) once per round of its loop.
///
/// A loop that can tell a good round from a bad one says so with [`ok`](Self::ok) and
/// [`failed`](Self::failed): a loop that keeps ticking but fails every round (a wrong
/// password, a service that refuses it) counts as unhealthy once it has failed for longer
/// than its `within`, as a hung loop does. Only [`tick`](Self::tick): hangs are noticed,
/// failures are not.
#[derive(Clone, Debug)]
pub struct Alive(Arc<Slot>);

impl Alive {
    /// The task finished a round (or is still making progress in a long one).
    pub fn tick(&self) {
        *lock(&self.0.last) = Instant::now();
    }

    /// A round that did its work: ticks, and ends a run of failures.
    pub fn ok(&self) {
        self.tick();
        *lock(&self.0.failing) = None;
    }

    /// A round that failed: ticks (the loop is not hung), and starts or continues a run of
    /// failures; `reason` is shown while the task counts as unhealthy.
    pub fn failed(&self, reason: impl std::fmt::Display) {
        self.tick();
        let mut failing = lock(&self.0.failing);
        let since = failing
            .as_ref()
            .map_or_else(Instant::now, |(since, _)| *since);
        *failing = Some((since, reason.to_string()));
    }
}

#[derive(Debug)]
struct Slot {
    name: String,
    within: Duration,
    last: Mutex<Instant>,
    /// Since when every round failed, and the last reason.
    failing: Mutex<Option<(Instant, String)>>,
}

/// A supervised task that counts as unhealthy: hung, or failing for too long.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unhealthy {
    pub name: String,
    /// For the log and the health check: "12 min without a round" or "failing for 12 min: …".
    pub why: String,
}

fn slots() -> &'static Mutex<Vec<Arc<Slot>>> {
    static SLOTS: OnceLock<Mutex<Vec<Arc<Slot>>>> = OnceLock::new();
    SLOTS.get_or_init(|| Mutex::new(Vec::new()))
}

fn register(name: String, within: Duration) -> Alive {
    let slot = Arc::new(Slot {
        name,
        within,
        last: Mutex::new(Instant::now()),
        failing: Mutex::new(None),
    });
    lock(slots()).push(Arc::clone(&slot));
    Alive(slot)
}

/// The ticking tasks without a tick within their time, with the seconds since their last one.
pub fn stalled() -> Vec<(String, u64)> {
    stalled_at(Instant::now())
}

fn stalled_at(now: Instant) -> Vec<(String, u64)> {
    lock(slots())
        .iter()
        .filter_map(|slot| {
            let quiet = now.saturating_duration_since(*lock(&slot.last));
            (quiet > slot.within).then(|| (slot.name.clone(), quiet.as_secs()))
        })
        .collect()
}

/// The ticking tasks that count as unhealthy: no tick within their time, or failing for
/// longer than it.
pub fn unhealthy() -> Vec<Unhealthy> {
    unhealthy_at(Instant::now())
}

fn unhealthy_at(now: Instant) -> Vec<Unhealthy> {
    lock(slots())
        .iter()
        .filter_map(|slot| {
            let quiet = now.saturating_duration_since(*lock(&slot.last));
            if quiet > slot.within {
                return Some(Unhealthy {
                    name: slot.name.clone(),
                    why: format!("{} without a round", minutes(quiet)),
                });
            }
            let failing = lock(&slot.failing);
            let (since, reason) = failing.as_ref()?;
            let long = now.saturating_duration_since(*since);
            (long > slot.within).then(|| Unhealthy {
                name: slot.name.clone(),
                why: format!("failing for {}: {reason}", minutes(long)),
            })
        })
        .collect()
}

fn minutes(d: Duration) -> String {
    match d.as_secs() {
        s if s < 120 => format!("{s} s"),
        s => format!("{} min", s / 60),
    }
}

/// A poisoned lock still holds a usable value here (an Instant, a list of slots).
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The task in a task of its own (so a panic is seen even when it unwinds), then `on_end`.
fn watch<Fut>(name: String, task: Fut, on_end: fn(&str, &str)) -> JoinHandle<()>
where
    Fut: Future + Send + 'static,
    Fut::Output: TaskEnd + Send,
{
    tokio::spawn(async move {
        info!(task = %name, "Background task started");
        let how = match tokio::spawn(task).await {
            Ok(output) => output.describe(),
            Err(e) if e.is_panic() => "panicked".to_owned(),
            Err(e) => format!("was cancelled: {e}"),
        };
        on_end(&name, &how);
    })
}

fn exit(name: &str, how: &str) {
    error!(task = %name, "Background task {how}; exiting so that the container is restarted");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENDED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

    fn record(name: &str, how: &str) {
        lock(&ENDED).push((name.to_owned(), how.to_owned()));
    }

    fn ended(name: &str) -> Option<String> {
        lock(&ENDED)
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, how)| how.clone())
    }

    #[tokio::test]
    async fn every_way_of_ending_is_seen() {
        watch("returns".into(), async {}, record).await.unwrap();
        watch(
            "fails".into(),
            async { Err::<(), _>(anyhow::anyhow!("no answer")) },
            record,
        )
        .await
        .unwrap();
        watch("panics".into(), async { panic!("bug") as () }, record)
            .await
            .unwrap();
        assert_eq!(ended("returns").as_deref(), Some("returned"));
        assert_eq!(ended("fails").as_deref(), Some("failed: no answer"));
        assert_eq!(ended("panics").as_deref(), Some("panicked"));
    }

    #[tokio::test]
    async fn a_running_task_is_not_ended() {
        let handle = watch("runs".into(), std::future::pending::<()>(), record);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handle.is_finished());
        assert_eq!(ended("runs"), None);
        handle.abort();
    }

    #[test]
    fn a_task_that_fails_every_round_turns_unhealthy_after_its_time() {
        let alive = register("imap under test".into(), Duration::from_secs(600));
        let mine = |at| {
            unhealthy_at(at)
                .into_iter()
                .find(|u| u.name == "imap under test")
        };
        alive.failed("LOGIN refused");
        assert_eq!(mine(Instant::now()), None, "just started failing");
        // the run started 11 minutes ago; the loop still ticks every round
        let started = Instant::now()
            .checked_sub(Duration::from_secs(660))
            .unwrap();
        lock(&alive.0.failing).as_mut().unwrap().0 = started;
        alive.failed("LOGIN refused again"); // keeps the run's start
        let late = mine(Instant::now()).expect("failing too long");
        assert_eq!(late.why, "failing for 11 min: LOGIN refused again");
        alive.ok();
        assert_eq!(mine(Instant::now()), None);
    }

    #[test]
    fn a_hung_task_is_unhealthy_with_its_quiet_time() {
        let _alive = register("hung under test".into(), Duration::from_secs(60));
        let u = unhealthy_at(Instant::now() + Duration::from_secs(185))
            .into_iter()
            .find(|u| u.name == "hung under test")
            .expect("hung");
        assert_eq!(u.why, "3 min without a round");
    }

    #[test]
    fn a_task_without_a_tick_in_time_is_stalled_until_it_ticks() {
        let alive = register("ticker under test".into(), Duration::from_secs(60));
        let mine = |at| {
            stalled_at(at)
                .into_iter()
                .find(|(name, _)| name == "ticker under test")
        };
        let now = Instant::now();
        assert_eq!(mine(now), None);
        assert_eq!(mine(now + Duration::from_secs(59)), None);
        assert_eq!(
            mine(now + Duration::from_secs(61)).map(|(_, secs)| secs >= 61),
            Some(true)
        );
        alive.tick();
        assert_eq!(mine(Instant::now() + Duration::from_secs(30)), None);
    }
}
