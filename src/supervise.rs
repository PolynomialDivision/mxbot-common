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
//! Not for tasks that end by design (per request, one-shot, a test mode that posts once):
//! those can still count their rounds for the status file with [`report`].
//!
//! Every task keeps a record for the bot's status file ([`crate::status`], [`snapshot`]): last
//! round, last success ([`Alive::ok`] or [`Alive::idle`]), last error and its short reason,
//! failures in a row, totals per outcome, and optionally how often it should succeed
//! ([`Alive::expect_every`]) and when its next piece of work is due ([`Alive::next_due`]).

use std::{
    future::Future,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime},
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
///
/// The task is listed in the bot's status file ([`crate::status`]) as `supervised`, without
/// rounds.
pub fn supervise<Fut>(name: impl Into<String>, task: Fut) -> JoinHandle<()>
where
    Fut: Future + Send + 'static,
    Fut::Output: TaskEnd + Send,
{
    let name = name.into();
    register(name.clone(), Kind::Supervised, None);
    watch(name, task, exit)
}

/// As [`supervise`], for a loop that works in rounds: `task` gets an [`Alive`] to tick once
/// per round. No tick for `within` (counted from the start, then from the last tick): the
/// heartbeat stops until the next tick. Choose `within` well above a round's longest normal
/// duration plus its pause.
///
/// A task whose work must succeed at least so often (a daily post, a poll every 10 min)
/// declares it with [`Alive::expect_every`]; it only goes into the status file, the health
/// check stays as `within` says.
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
    let alive = register(name.clone(), Kind::Ticking, Some(within));
    watch(name, task(alive), exit)
}

/// An [`Alive`] for work that is not a supervised loop (a request handler, a job started by
/// an event): its rounds go into the status file, and it never affects the heartbeat, however
/// long it is quiet or failing. Asking twice for the same name gives the same record; a name
/// that a supervised task already has gives that task's record (one series per name).
pub fn report(name: impl Into<String>) -> Alive {
    let name = name.into();
    // Looked up and registered under one lock: two first reports at once make one record.
    let mut slots = lock(slots());
    if let Some(slot) = slots.iter().find(|slot| slot.name == name) {
        return Alive(Arc::clone(slot));
    }
    let slot = new_slot(name, Kind::Reported, None);
    slots.push(Arc::clone(&slot));
    Alive(slot)
}

/// A ticking task's handle: [`tick`](Self::tick) once per round of its loop.
///
/// A loop that can tell a good round from a bad one says so with [`ok`](Self::ok),
/// [`idle`](Self::idle) and [`failed`](Self::failed): a loop that keeps ticking but fails
/// every round (a wrong password, a service that refuses it) counts as unhealthy once it has
/// failed for longer than its `within`, as a hung loop does. Only [`tick`](Self::tick): hangs
/// are noticed, failures are not.
///
/// Every call is also counted for the bot's status file ([`crate::status`]): last success,
/// last error and a short reason, failures in a row, totals per outcome.
#[derive(Clone, Debug)]
pub struct Alive(Arc<Slot>);

impl Alive {
    /// The task finished a round (or is still making progress in a long one).
    pub fn tick(&self) {
        let mut state = lock(&self.0.state);
        state.last = Instant::now();
        state.last_tick = Some(SystemTime::now());
    }

    /// A round that did its work: ticks, and ends a run of failures.
    pub fn ok(&self) {
        self.healthy(Outcome::Ok);
    }

    /// A round that found nothing to do (no new mail, an empty day): as healthy as
    /// [`ok`](Self::ok), counted apart.
    pub fn idle(&self) {
        self.healthy(Outcome::Idle);
    }

    /// A round that failed: ticks (the loop is not hung), and starts or continues a run of
    /// failures; `reason` is shown while the task counts as unhealthy. It is shortened and
    /// cleaned of URL credentials and tokens ([`crate::status::sanitize_reason`]) first, so
    /// an error message can be passed as it is.
    pub fn failed(&self, reason: impl std::fmt::Display) {
        self.failure(Outcome::Failed, &reason.to_string());
    }

    /// The task gave a piece of work up for good (a slot outside its catch-up window, a
    /// message dropped after its last retry): a failed round, counted as given up too. It
    /// ends a run of failures for the heartbeat (nothing is retried any more); the status
    /// file keeps it ([`crate::status`], the alert BotTaskGaveUp).
    pub fn gave_up(&self, reason: impl std::fmt::Display) {
        self.failure(Outcome::GaveUp, &reason.to_string());
    }

    /// The task's work should succeed ([`ok`](Self::ok) or [`idle`](Self::idle)) at least
    /// this often: a daily job says a day, a poller its interval. For the status file and the
    /// alerts built on it only; the heartbeat keeps following `within`.
    pub fn expect_every(&self, every: Duration) -> &Self {
        lock(&self.0.state).expected = Some(every);
        self
    }

    /// When the task's next piece of work is due (a scheduled post), `None` when nothing is
    /// planned. For the status file: a due time long past means the work did not happen.
    pub fn next_due(&self, at: Option<SystemTime>) -> &Self {
        lock(&self.0.state).next_due = at;
        self
    }

    /// The task's name.
    pub fn name(&self) -> &str {
        &self.0.name
    }

    fn healthy(&self, outcome: Outcome) {
        let mut state = lock(&self.0.state);
        let now = SystemTime::now();
        state.last = Instant::now();
        state.last_tick = Some(now);
        state.failing = None;
        state.last_ok = Some(now);
        state.consecutive_failures = 0;
        state.last_outcome = Some(outcome);
        match outcome {
            Outcome::Idle => state.idle_total += 1,
            _ => state.ok_total += 1,
        }
    }

    fn failure(&self, outcome: Outcome, reason: &str) {
        let reason = crate::status::sanitize_reason(reason);
        let mut state = lock(&self.0.state);
        let now = SystemTime::now();
        state.last = Instant::now();
        state.last_tick = Some(now);
        let since = state
            .failing
            .as_ref()
            .map_or_else(Instant::now, |(since, _)| *since);
        state.failing = Some((since, reason.clone()));
        state.last_err = Some(now);
        state.last_err_reason = Some(reason);
        state.consecutive_failures += 1;
        state.failed_total += 1;
        if outcome == Outcome::GaveUp {
            state.gave_up_total += 1;
            state.last_gave_up = Some(now);
            // Given up for good: nothing is being retried, so the task is not failing now
            // (the heartbeat would stay unhealthy until a daily job's next round); the status
            // file and its alert keep the give-up.
            state.failing = None;
        }
        state.last_outcome = Some(outcome);
    }

    /// [`expect_every`](Self::expect_every), or none.
    #[cfg_attr(not(feature = "schedule"), allow(dead_code))]
    pub(crate) fn set_expected(&self, every: Option<Duration>) {
        lock(&self.0.state).expected = every;
    }

    /// Shown as a scheduled job ([`crate::schedule::run`] says so).
    #[cfg_attr(not(feature = "schedule"), allow(dead_code))]
    pub(crate) fn mark_scheduled(&self) {
        lock(&self.0.state).kind = Kind::Scheduled;
    }
}

/// What kind of task a record is, as the status file names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// [`supervise_ticking`]: rounds, watched by the heartbeat.
    Ticking,
    /// [`supervise`]: no rounds; only its end is seen (it ends the bot).
    Supervised,
    /// [`crate::schedule`]'s jobs (also ticking).
    Scheduled,
    /// [`report`]: rounds counted, never watched by the heartbeat.
    Reported,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ticking => "ticking",
            Self::Supervised => "supervised",
            Self::Scheduled => "scheduled",
            Self::Reported => "reported",
        }
    }
}

/// How a task's last round ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Idle,
    Failed,
    GaveUp,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Idle => "idle",
            Self::Failed => "failed",
            Self::GaveUp => "gave_up",
        }
    }
}

#[derive(Debug)]
struct Slot {
    name: String,
    /// `None`: not watched by the heartbeat ([`supervise`], [`report`]).
    within: Option<Duration>,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    kind: Kind,
    last: Instant,
    /// Since when every round failed, and the last reason.
    failing: Option<(Instant, String)>,
    last_tick: Option<SystemTime>,
    last_ok: Option<SystemTime>,
    last_err: Option<SystemTime>,
    last_err_reason: Option<String>,
    consecutive_failures: u64,
    ok_total: u64,
    failed_total: u64,
    idle_total: u64,
    gave_up_total: u64,
    last_gave_up: Option<SystemTime>,
    last_outcome: Option<Outcome>,
    expected: Option<Duration>,
    next_due: Option<SystemTime>,
}

/// A task's record as the status file shows it ([`snapshot`]).
#[derive(Clone, Debug, PartialEq)]
pub struct TaskStatus {
    pub name: String,
    pub kind: Kind,
    pub within: Option<Duration>,
    pub expected: Option<Duration>,
    pub last_tick: Option<SystemTime>,
    /// The last healthy round: [`Alive::ok`] or [`Alive::idle`].
    pub last_ok: Option<SystemTime>,
    pub last_err: Option<SystemTime>,
    pub last_err_reason: Option<String>,
    pub consecutive_failures: u64,
    pub ok_total: u64,
    pub failed_total: u64,
    pub idle_total: u64,
    /// Also counted in `failed_total`.
    pub gave_up_total: u64,
    /// The last [`Alive::gave_up`]: a give-up is seen even when it came before the first
    /// look at the totals (a counter that starts at 1 shows no increase).
    pub last_gave_up: Option<SystemTime>,
    pub last_outcome: Option<Outcome>,
    pub next_due: Option<SystemTime>,
}

/// Every task of this process, in the order they were started.
pub fn snapshot() -> Vec<TaskStatus> {
    lock(slots())
        .iter()
        .map(|slot| {
            let state = lock(&slot.state);
            TaskStatus {
                name: slot.name.clone(),
                kind: state.kind,
                within: slot.within,
                expected: state.expected,
                last_tick: state.last_tick,
                last_ok: state.last_ok,
                last_err: state.last_err,
                last_err_reason: state.last_err_reason.clone(),
                consecutive_failures: state.consecutive_failures,
                ok_total: state.ok_total,
                failed_total: state.failed_total,
                idle_total: state.idle_total,
                gave_up_total: state.gave_up_total,
                last_gave_up: state.last_gave_up,
                last_outcome: state.last_outcome,
                next_due: state.next_due,
            }
        })
        .collect()
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

fn register(name: String, kind: Kind, within: Option<Duration>) -> Alive {
    let slot = new_slot(name, kind, within);
    lock(slots()).push(Arc::clone(&slot));
    Alive(slot)
}

fn new_slot(name: String, kind: Kind, within: Option<Duration>) -> Arc<Slot> {
    Arc::new(Slot {
        name,
        within,
        state: Mutex::new(State {
            kind,
            last: Instant::now(),
            failing: None,
            last_tick: None,
            last_ok: None,
            last_err: None,
            last_err_reason: None,
            consecutive_failures: 0,
            ok_total: 0,
            failed_total: 0,
            idle_total: 0,
            gave_up_total: 0,
            last_gave_up: None,
            last_outcome: None,
            expected: None,
            next_due: None,
        }),
    })
}

/// The ticking tasks without a tick within their time, with the seconds since their last one.
pub fn stalled() -> Vec<(String, u64)> {
    stalled_at(Instant::now())
}

fn stalled_at(now: Instant) -> Vec<(String, u64)> {
    lock(slots())
        .iter()
        .filter_map(|slot| {
            let within = slot.within?;
            let quiet = now.saturating_duration_since(lock(&slot.state).last);
            (quiet > within).then(|| (slot.name.clone(), quiet.as_secs()))
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
            let within = slot.within?;
            let state = lock(&slot.state);
            let quiet = now.saturating_duration_since(state.last);
            if quiet > within {
                return Some(Unhealthy {
                    name: slot.name.clone(),
                    why: format!("{} without a round", minutes(quiet)),
                });
            }
            let (since, reason) = state.failing.as_ref()?;
            let long = now.saturating_duration_since(*since);
            (long > within).then(|| Unhealthy {
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
    fn a_give_up_ends_the_unhealthy_run_but_is_kept_in_the_status() {
        let alive = register(
            "daily post under test".into(),
            Kind::Ticking,
            Some(Duration::from_secs(600)),
        );
        let mine = |at| {
            unhealthy_at(at)
                .into_iter()
                .any(|u| u.name == "daily post under test")
        };
        alive.failed("calendar not read");
        lock(&alive.0.state).failing.as_mut().unwrap().0 = Instant::now()
            .checked_sub(Duration::from_secs(660))
            .unwrap();
        assert!(mine(Instant::now()), "failing too long");
        // the slot is given up: nothing is retried, the heartbeat is healthy again
        alive.gave_up("slot 2026-10-30 not done");
        assert!(!mine(Instant::now()));
        let t = status_of("daily post under test");
        assert_eq!(
            (t.gave_up_total, t.last_outcome),
            (1, Some(Outcome::GaveUp))
        );
    }

    #[test]
    fn a_task_that_fails_every_round_turns_unhealthy_after_its_time() {
        let alive = register(
            "imap under test".into(),
            Kind::Ticking,
            Some(Duration::from_secs(600)),
        );
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
        lock(&alive.0.state).failing.as_mut().unwrap().0 = started;
        alive.failed("LOGIN refused again"); // keeps the run's start
        let late = mine(Instant::now()).expect("failing too long");
        assert_eq!(late.why, "failing for 11 min: LOGIN refused again");
        alive.ok();
        assert_eq!(mine(Instant::now()), None);
    }

    fn status_of(name: &str) -> TaskStatus {
        snapshot()
            .into_iter()
            .find(|t| t.name == name)
            .expect("listed")
    }

    #[test]
    fn rounds_are_counted_for_the_status_file() {
        let alive = register(
            "counted under test".into(),
            Kind::Ticking,
            Some(Duration::from_secs(600)),
        );
        let fresh = status_of("counted under test");
        assert_eq!(fresh.kind, Kind::Ticking);
        assert_eq!(fresh.within, Some(Duration::from_secs(600)));
        assert_eq!(
            (fresh.last_tick, fresh.last_ok, fresh.last_outcome),
            (None, None, None)
        );

        alive.expect_every(Duration::from_secs(300)).tick();
        alive.ok();
        alive.idle();
        alive.idle();
        alive.failed("connect https://user:secret@imap.example/ refused");
        alive.failed("still refused");
        let t = status_of("counted under test");
        assert_eq!(t.expected, Some(Duration::from_secs(300)));
        assert_eq!(
            (t.ok_total, t.idle_total, t.failed_total, t.gave_up_total),
            (1, 2, 2, 0)
        );
        assert_eq!(t.consecutive_failures, 2);
        assert_eq!(t.last_outcome, Some(Outcome::Failed));
        assert_eq!(t.last_err_reason.as_deref(), Some("still refused"));
        assert!(t.last_ok.is_some() && t.last_err.is_some() && t.last_tick.is_some());

        assert_eq!(t.last_gave_up, None);
        alive.gave_up("slot 2026-10-30 not done");
        let t = status_of("counted under test");
        assert_eq!(
            (t.failed_total, t.gave_up_total, t.consecutive_failures),
            (3, 1, 3)
        );
        assert_eq!(t.last_outcome, Some(Outcome::GaveUp));
        assert!(t.last_gave_up.is_some_and(|at| at >= t.last_ok.unwrap()));

        alive.idle();
        let t = status_of("counted under test");
        assert_eq!(t.consecutive_failures, 0, "a healthy round ends the run");
        assert_eq!(t.last_outcome, Some(Outcome::Idle));
        assert!(t.last_gave_up.is_some(), "the last give-up stays");
        assert_eq!(
            t.last_err_reason.as_deref(),
            Some("slot 2026-10-30 not done")
        );
        assert!(unhealthy().iter().all(|u| u.name != "counted under test"));

        let due = SystemTime::now() + Duration::from_secs(3600);
        alive.next_due(Some(due));
        assert_eq!(status_of("counted under test").next_due, Some(due));
    }

    #[test]
    fn reasons_are_sanitised_before_anyone_sees_them() {
        let alive = register(
            "sanitised under test".into(),
            Kind::Ticking,
            Some(Duration::from_secs(60)),
        );
        alive.failed("GET https://api.example/v1?access_token=syt_secret failed");
        let t = status_of("sanitised under test");
        assert_eq!(
            t.last_err_reason.as_deref(),
            Some("GET https://api.example/v1 failed")
        );
        let failing = lock(&alive.0.state).failing.clone().unwrap().1;
        assert_eq!(
            failing, "GET https://api.example/v1 failed",
            "the heartbeat's reason too"
        );
    }

    #[tokio::test]
    async fn every_kind_of_task_is_listed_and_only_ticking_ones_watched() {
        let handle = supervise("plain under test", std::future::pending::<()>());
        let reported = report("reported under test");
        assert_eq!(status_of("plain under test").kind, Kind::Supervised);
        assert_eq!(status_of("reported under test").kind, Kind::Reported);
        assert_eq!(status_of("plain under test").within, None);
        // A reported task failing for ages or quiet for ages is never unhealthy.
        reported.failed("down");
        lock(&reported.0.state).failing.as_mut().unwrap().0 = Instant::now()
            .checked_sub(Duration::from_secs(7200))
            .unwrap();
        let later = Instant::now() + Duration::from_secs(7200);
        assert!(unhealthy_at(later)
            .iter()
            .all(|u| u.name != "reported under test" && u.name != "plain under test"));
        assert!(stalled_at(later)
            .iter()
            .all(|(n, _)| n != "reported under test" && n != "plain under test"));
        // The same name reports into the same record.
        report("reported under test").ok();
        assert_eq!(status_of("reported under test").ok_total, 1);
        assert_eq!(
            snapshot()
                .iter()
                .filter(|t| t.name == "reported under test")
                .count(),
            1
        );
        // A supervised task's name reports into that task, not a second record.
        report("plain under test").ok();
        assert_eq!(status_of("plain under test").kind, Kind::Supervised);
        assert_eq!(status_of("plain under test").ok_total, 1);
        assert_eq!(
            snapshot()
                .iter()
                .filter(|t| t.name == "plain under test")
                .count(),
            1
        );
        handle.abort();
    }

    #[test]
    fn first_reports_at_once_make_one_record() {
        let start = Arc::new(std::sync::Barrier::new(16));
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    report("raced under test").ok();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let records: Vec<_> = snapshot()
            .into_iter()
            .filter(|t| t.name == "raced under test")
            .collect();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].ok_total, 16,
            "every report counted in the one record"
        );
    }

    #[test]
    fn a_hung_task_is_unhealthy_with_its_quiet_time() {
        let _alive = register(
            "hung under test".into(),
            Kind::Ticking,
            Some(Duration::from_secs(60)),
        );
        let u = unhealthy_at(Instant::now() + Duration::from_secs(185))
            .into_iter()
            .find(|u| u.name == "hung under test")
            .expect("hung");
        assert_eq!(u.why, "3 min without a round");
    }

    #[test]
    fn a_task_without_a_tick_in_time_is_stalled_until_it_ticks() {
        let alive = register(
            "ticker under test".into(),
            Kind::Ticking,
            Some(Duration::from_secs(60)),
        );
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
