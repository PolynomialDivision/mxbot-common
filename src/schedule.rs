//! Scheduled jobs: posts at wall-clock times (daily, weekly, monthly), each run once per slot.
//!
//! * [`Rule`]: when, in local wall-clock time; pure. Its occurrences become instants through
//!   [`Zone::at`], so DST changes are handled in one place.
//! * [`Slot`] / [`Period`]: one occurrence and the period it stands for. The period's key
//!   (`2026-10-30`, `2026-10-30 08:00`, `2026-W44`, `2026-10`) names the slot in the ledger and
//!   in transaction IDs; moving the time after a run does not run that period again.
//! * [`Ledger`]: which slot of a job was handled. [`JsonLedger`] keeps it in a JSON file (and
//!   reads calendar-bot's `posted.json`, where a slot is handled only on its own date: see
//!   [`JsonLedger::open_dated`]); a bot with a database can implement it on a table.
//! * [`Job`] and [`run`]: the loop. It re-plans from UTC every round (at most
//!   [`MAX_SLEEP`] apart), ticks its [`Alive`], catches up a slot missed within the job's
//!   window (a restart), and marks a slot only as [`Delivery`] says: before the work (at most
//!   once: games) or after the work reported it handled (at least once: posts).
//! * [`post`]: a slot's message into rooms with transaction IDs from the slot's key and a
//!   deadline, so a retry after a crash or an outage returns the event already sent instead
//!   of posting twice.
//!
//! No cron syntax: every schedule in the fleet is daily, weekly or monthly at `HH:MM`.

use std::{
    collections::BTreeMap,
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use anyhow::Result;
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, TimeDelta, Timelike, Utc, Weekday};
use matrix_sdk::{
    ruma::{events::MessageLikeEventContent, OwnedEventId, OwnedRoomId},
    Room,
};
use tokio::{sync::watch, time::sleep};
use tracing::{info, warn};

use crate::{
    clock::{naive_time, Zone},
    config::TimeOfDay,
    persist,
    send::{classify_error, send_until, txn_for},
    supervise::Alive,
};

/// The longest a job sleeps before it plans again from the current UTC time: a clock step
/// or a DST change is noticed within this, and it is the job's tick for
/// [`crate::supervise::supervise_ticking`].
pub const MAX_SLEEP: Duration = Duration::from_secs(15 * 60);

// ── Rules ─────────────────────────────────────────────────────────────────────

/// When a job runs, in the zone's wall-clock time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    /// Every day at each of these times (none: never).
    Daily { at: Vec<TimeOfDay> },
    /// Once a week.
    Weekly { day: Weekday, at: TimeOfDay },
    /// Once a month; days 29-31 fall on the last day of shorter months.
    Monthly { day: u32, at: TimeOfDay },
}

impl Rule {
    /// How often the rule runs at least, for the status file's expected interval
    /// ([`crate::supervise::Alive::expect_every`]): a day, a week, 31 days; `None` for a daily
    /// rule without times.
    pub fn interval(&self) -> Option<Duration> {
        const DAY: Duration = Duration::from_secs(24 * 60 * 60);
        match self {
            Rule::Daily { at } if at.is_empty() => None,
            Rule::Daily { .. } => Some(DAY),
            Rule::Weekly { .. } => Some(7 * DAY),
            Rule::Monthly { .. } => Some(31 * DAY),
        }
    }

    /// Every day at `at`.
    pub fn daily(at: TimeOfDay) -> Self {
        Rule::Daily { at: vec![at] }
    }

    /// The first occurrence strictly after the wall-clock time `after`.
    pub fn next_local(&self, after: NaiveDateTime) -> Option<NaiveDateTime> {
        let date = after.date();
        match self {
            Rule::Daily { at } => at
                .iter()
                .map(|t| {
                    let today = date.and_time(naive_time(*t));
                    if today > after {
                        today
                    } else {
                        today + TimeDelta::days(1)
                    }
                })
                .min(),
            Rule::Weekly { day, at } => {
                let ahead = (i64::from(day.num_days_from_monday())
                    - i64::from(date.weekday().num_days_from_monday()))
                .rem_euclid(7);
                let this_week = (date + TimeDelta::days(ahead)).and_time(naive_time(*at));
                Some(if this_week > after {
                    this_week
                } else {
                    this_week + TimeDelta::days(7)
                })
            }
            Rule::Monthly { day, at } => {
                let this_month =
                    month_day(date.year(), date.month(), *day).and_time(naive_time(*at));
                if this_month > after {
                    return Some(this_month);
                }
                let (year, month) = next_month(date.year(), date.month());
                Some(month_day(year, month, *day).and_time(naive_time(*at)))
            }
        }
    }

    /// The most recent occurrence at or before the wall-clock time `at`.
    pub fn latest_local(&self, at: NaiveDateTime) -> Option<NaiveDateTime> {
        let date = at.date();
        match self {
            Rule::Daily { at: times } => times
                .iter()
                .map(|t| {
                    let today = date.and_time(naive_time(*t));
                    if today <= at {
                        today
                    } else {
                        today - TimeDelta::days(1)
                    }
                })
                .max(),
            Rule::Weekly { day, at: time } => {
                let back = (i64::from(date.weekday().num_days_from_monday())
                    - i64::from(day.num_days_from_monday()))
                .rem_euclid(7);
                let this_week = (date - TimeDelta::days(back)).and_time(naive_time(*time));
                Some(if this_week <= at {
                    this_week
                } else {
                    this_week - TimeDelta::days(7)
                })
            }
            Rule::Monthly { day, at: time } => {
                let this_month =
                    month_day(date.year(), date.month(), *day).and_time(naive_time(*time));
                if this_month <= at {
                    return Some(this_month);
                }
                let (year, month) = previous_month(date.year(), date.month());
                Some(month_day(year, month, *day).and_time(naive_time(*time)))
            }
        }
    }

    /// The period an occurrence at the wall-clock time `local` stands for: its day (with
    /// the time when the rule has several a day), its ISO week or its month.
    pub fn period(&self, local: NaiveDateTime) -> Period {
        let date = local.date();
        match self {
            Rule::Daily { at } if at.iter().any(|t| *t != at[0]) => {
                Period::DayAt(date, TimeOfDay::new(local.hour(), local.minute()))
            }
            Rule::Daily { .. } => Period::Day(date),
            Rule::Weekly { .. } => {
                let week = date.iso_week();
                Period::Week {
                    year: week.year(),
                    week: week.week(),
                }
            }
            Rule::Monthly { .. } => Period::Month {
                year: date.year(),
                month: date.month(),
            },
        }
    }

    fn slot(&self, zone: Zone, local: NaiveDateTime) -> Slot {
        Slot {
            local,
            at: zone.at(local),
            period: self.period(local),
        }
    }

    /// The first slot strictly after `now`.
    pub fn next(&self, zone: Zone, now: DateTime<Utc>) -> Option<Slot> {
        // Start a little before the wall clock (an autumn change repeats an hour), then
        // step forward: `Zone::at` never goes backwards, so the first later instant wins.
        let mut local = self.next_local(zone.local(now) - DST_MARGIN)?;
        while zone.at(local) <= now {
            local = self.next_local(local)?;
        }
        Some(self.slot(zone, local))
    }

    /// The most recent slot at or before `now`.
    pub fn latest(&self, zone: Zone, now: DateTime<Utc>) -> Option<Slot> {
        let mut local = self.latest_local(zone.local(now) + DST_MARGIN)?;
        while zone.at(local) > now {
            local = self.latest_local(local - TimeDelta::seconds(1))?;
        }
        Some(self.slot(zone, local))
    }

    /// The most recent slot at or before `now` if it is at most `catch_up` old.
    pub fn due(&self, zone: Zone, now: DateTime<Utc>, catch_up: Duration) -> Option<Slot> {
        let slot = self.latest(zone, now)?;
        let late = (now - slot.at).to_std().unwrap_or_default();
        (late <= catch_up).then_some(slot)
    }
}

/// More than any change of a zone's UTC offset.
const DST_MARGIN: TimeDelta = TimeDelta::hours(3);

/// Day `day` (1-31) of a month, the month's last day when it is shorter.
pub fn month_day(year: i32, month: u32, day: u32) -> NaiveDate {
    let day = day.clamp(1, 31);
    NaiveDate::from_ymd_opt(year, month, day).unwrap_or_else(|| {
        let (year, month) = next_month(year, month);
        NaiveDate::from_ymd_opt(year, month, 1).expect("valid month") - TimeDelta::days(1)
    })
}

fn next_month(year: i32, month: u32) -> (i32, u32) {
    if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    }
}

fn previous_month(year: i32, month: u32) -> (i32, u32) {
    if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

// ── Slots ─────────────────────────────────────────────────────────────────────

/// The period a slot stands for. Its [`key`](Period::key) names the slot; a later run in
/// the same period (the time moved after the post) is the same slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Period {
    /// A day: `2026-10-30`.
    Day(NaiveDate),
    /// One of several times a day: `2026-10-30 08:00`.
    DayAt(NaiveDate, TimeOfDay),
    /// An ISO week: `2026-W44`.
    Week { year: i32, week: u32 },
    /// A month: `2026-10`.
    Month { year: i32, month: u32 },
}

impl Period {
    pub fn key(&self) -> String {
        match self {
            Period::Day(date) => date.format("%Y-%m-%d").to_string(),
            Period::DayAt(date, time) => format!("{} {time}", date.format("%Y-%m-%d")),
            Period::Week { year, week } => format!("{year:04}-W{week:02}"),
            Period::Month { year, month } => format!("{year:04}-{month:02}"),
        }
    }

    /// A key as [`Period::key`] writes it.
    pub fn parse(key: &str) -> Option<Self> {
        let key = key.trim();
        if let Some((date, time)) = key.split_once(' ') {
            return Some(Period::DayAt(date.parse().ok()?, time.parse().ok()?));
        }
        if let Some((year, week)) = key.split_once("-W") {
            let (year, week) = (year.parse().ok()?, week.parse().ok()?);
            NaiveDate::from_isoywd_opt(year, week, Weekday::Mon)?;
            return Some(Period::Week { year, week });
        }
        if let Ok(date) = key.parse() {
            return Some(Period::Day(date));
        }
        let (year, month) = key.split_once('-')?;
        let (year, month) = (year.parse().ok()?, month.parse().ok()?);
        NaiveDate::from_ymd_opt(year, month, 1)?;
        Some(Period::Month { year, month })
    }

    /// The period's first day.
    pub fn start(&self) -> NaiveDate {
        match *self {
            Period::Day(date) | Period::DayAt(date, _) => date,
            Period::Week { year, week } => {
                NaiveDate::from_isoywd_opt(year, week, Weekday::Mon).expect("valid ISO week")
            }
            Period::Month { year, month } => {
                NaiveDate::from_ymd_opt(year, month, 1).expect("valid month")
            }
        }
    }

    /// Whether a ledger that recorded `self` (a slot key) has handled `slot` too: the same
    /// period or a later one (a slot older than what was done is no longer worth running). A
    /// day (the rule changed from daily) counts for the week or month it lies in, and for
    /// every time of its day. Not for dated ledgers ([`JsonLedger::open_dated`]).
    pub fn covers(&self, slot: &Period) -> bool {
        match (self, slot) {
            (Period::DayAt(d1, t1), Period::DayAt(d2, t2)) => (d1, t1) >= (d2, t2),
            _ => self.start() >= slot.start(),
        }
    }
}

impl std::fmt::Display for Period {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key())
    }
}

/// One occurrence of a rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    /// The wall-clock time the rule names (in the spring gap: the time it names, not the
    /// minute it runs at).
    pub local: NaiveDateTime,
    /// When it is due.
    pub at: DateTime<Utc>,
    pub period: Period,
}

impl Slot {
    /// The slot's name in a ledger and in transaction-ID keys.
    pub fn key(&self) -> String {
        self.period.key()
    }
}

// ── Outcomes ──────────────────────────────────────────────────────────────────

/// What a job's work did with a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Handled: recorded in the ledger.
    Done,
    /// Nothing to do this time (an empty day): recorded in the ledger as well.
    Skipped,
    /// Not handled (a source or the homeserver unreachable): tried again after this long,
    /// as long as the slot is within the job's catch-up window. Not recorded.
    RetryIn(Duration),
}

/// When a slot counts as handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Recorded before the work starts: a crash or a failure in the middle never runs it
    /// again. For things that must not happen twice (a game round).
    AtMostOnce,
    /// Recorded after the work returned [`Outcome::Done`] or [`Outcome::Skipped`]: a crash
    /// in the middle runs it again, so its sends need stable transaction IDs ([`post`]).
    /// For posts that must not get lost.
    AtLeastOnce,
}

// ── Ledgers ───────────────────────────────────────────────────────────────────

/// Where a job's handled slots are remembered, under the job's entry name.
pub trait Ledger: Send + Sync {
    /// Whether `slot` (or a later one) was handled.
    fn is_done(&self, entry: &str, slot: &Slot) -> impl Future<Output = Result<bool>> + Send;
    /// Record `slot` as handled.
    fn mark_done(&self, entry: &str, slot: &Slot) -> impl Future<Output = Result<()>> + Send;
}

/// A JSON file `{entry: slot key}` in the store. Updates are serialised and each is saved
/// before the next starts, so two jobs marking at the same minute both stay recorded.
#[derive(Clone)]
pub struct JsonLedger(Arc<LedgerFile>);

struct LedgerFile {
    path: PathBuf,
    dated: bool,
    /// `null` entries are kept as they are (calendar-bot's `posted.json` has them).
    entries: Mutex<BTreeMap<String, Option<String>>>,
    writer: tokio::sync::Mutex<()>,
}

impl JsonLedger {
    /// Open `path`; a missing file is an empty ledger, one that does not parse is moved
    /// aside ([`persist::load_json_or_quarantine`]): at worst a slot within its catch-up
    /// window runs once more.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self> {
        Self::load(path.into(), false).await
    }

    /// [`JsonLedger::open`] recording each slot as its local date (`{"daily": "2026-10-30"}`):
    /// the shape of calendar-bot's `posted.json`, which older versions of a bot still read.
    /// For rules with one slot a day at most, since it marks the whole day.
    ///
    /// A slot is handled only when the recorded date is its own date (as the versions before
    /// the shared schedule had it): moving the time within the day does not run it again,
    /// but moving a weekly or monthly day runs it on the new day, also within the same
    /// week or month. That overlaps, where counting the date for its whole week would leave
    /// days without an overview (posted Monday, moved to Sunday: nothing until the Sunday
    /// after). Transaction keys of such a job's posts name the slot's date, not its period
    /// key, or the post on the new day would return the old one.
    pub async fn open_dated(path: impl Into<PathBuf>) -> Result<Self> {
        Self::load(path.into(), true).await
    }

    async fn load(path: PathBuf, dated: bool) -> Result<Self> {
        let entries = persist::load_json_or_quarantine(&path).await?;
        Ok(Self(Arc::new(LedgerFile {
            path,
            dated,
            entries: Mutex::new(entries),
            writer: tokio::sync::Mutex::new(()),
        })))
    }

    /// What is recorded for `entry`, as written.
    pub fn recorded(&self, entry: &str) -> Option<String> {
        lock(&self.0.entries).get(entry).cloned().flatten()
    }

    /// Record `value` for `entry` and save the file. It counts in memory even when the save
    /// fails (returned as the error): this process does not run the slot again.
    pub async fn record(&self, entry: &str, value: impl Into<String>) -> Result<()> {
        let _writing = self.0.writer.lock().await;
        let snapshot = {
            let mut entries = lock(&self.0.entries);
            entries.insert(entry.to_owned(), Some(value.into()));
            entries.clone()
        };
        persist::save_json_atomic(&self.0.path, &snapshot).await
    }
}

impl Ledger for JsonLedger {
    async fn is_done(&self, entry: &str, slot: &Slot) -> Result<bool> {
        let Some(recorded) = self.recorded(entry) else {
            return Ok(false);
        };
        Ok(if self.0.dated {
            recorded
                .trim()
                .parse::<NaiveDate>()
                .is_ok_and(|date| date == slot.local.date())
        } else {
            Period::parse(&recorded).is_some_and(|recorded| recorded.covers(&slot.period))
        })
    }

    async fn mark_done(&self, entry: &str, slot: &Slot) -> Result<()> {
        let value = if self.0.dated {
            slot.local.date().format("%Y-%m-%d").to_string()
        } else {
            slot.key()
        };
        self.record(entry, value).await
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ── Jobs ──────────────────────────────────────────────────────────────────────

/// A scheduled job: its rule, zone, catch-up window and delivery.
pub struct Job {
    name: String,
    entry: String,
    zone: Zone,
    delivery: Delivery,
    catch_up: Duration,
    max_sleep: Duration,
    rule: Box<dyn Fn() -> Option<Rule> + Send + Sync>,
    changes: Option<watch::Receiver<u64>>,
}

impl Job {
    /// `name` is logged and is the ledger entry (see [`Job::entry`]). `rule` is asked every
    /// round, so runtime settings apply at once; `None` means the job is off. A slot missed
    /// (the bot was down) still runs while it is at most `catch_up` old.
    pub fn new(
        name: impl Into<String>,
        zone: Zone,
        delivery: Delivery,
        catch_up: Duration,
        rule: impl Fn() -> Option<Rule> + Send + Sync + 'static,
    ) -> Self {
        let name = name.into();
        Self {
            entry: name.clone(),
            name,
            zone,
            delivery,
            catch_up,
            max_sleep: MAX_SLEEP,
            rule: Box::new(rule),
            changes: None,
        }
    }

    /// The ledger entry, when it differs from the name (`"daily"` for "daily summary").
    pub fn entry(mut self, entry: impl Into<String>) -> Self {
        self.entry = entry.into();
        self
    }

    /// Plan again at least this often (default [`MAX_SLEEP`]).
    pub fn max_sleep(mut self, max_sleep: Duration) -> Self {
        self.max_sleep = max_sleep;
        self
    }

    /// Plan again at once when `changes` changes (a settings store's
    /// [`subscribe`](crate::settings::Settings::subscribe)).
    pub fn wake_on(mut self, changes: watch::Receiver<u64>) -> Self {
        self.changes = Some(changes);
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The job's ledger entry.
    pub fn ledger_entry(&self) -> &str {
        &self.entry
    }

    /// The most recent slot at or before `now` under the current rule; `None` while the job
    /// is off. For a bot that seeds its ledger (the version before recorded nothing).
    pub fn latest(&self, now: DateTime<Utc>) -> Option<Slot> {
        (self.rule)()?.latest(self.zone, now)
    }

    /// The `within` for [`crate::supervise::supervise_ticking`]: two pauses plus the
    /// longest a normal run of the work takes.
    pub fn stall_after(&self, longest_run: Duration) -> Duration {
        2 * self.max_sleep + longest_run
    }
}

/// A job's state between rounds. [`run`] drives it; tests can call [`Runner::round`]
/// with times of their choice.
pub struct Runner {
    job: Job,
    /// A slot whose work asked to be tried again, and not before when.
    retry: Option<(Period, DateTime<Utc>)>,
    /// The last slot this process knows to be handled (saves asking the ledger, and holds
    /// even when recording it failed). The slot, not its period: another slot in the same
    /// period (the day moved) is the ledger's to decide.
    handled: Option<Slot>,
    /// The next slot last logged.
    announced: Option<DateTime<Utc>>,
    disabled: bool,
    /// Where rounds are reported for the status file ([`run`] sets the job's [`Alive`]).
    report: Option<Alive>,
}

impl Runner {
    pub fn new(job: Job) -> Self {
        Self {
            job,
            retry: None,
            handled: None,
            announced: None,
            disabled: false,
            report: None,
        }
    }

    /// Report each slot's outcome to `alive` for the status file: done → [`Alive::ok`],
    /// skipped → [`Alive::idle`], not done → [`Alive::failed`], a slot given up →
    /// [`Alive::gave_up`]; with the rule's interval as the expected one and the next due slot.
    /// [`run`] does this with the job's own `Alive`.
    pub fn report_to(mut self, alive: Alive) -> Self {
        self.report = Some(alive);
        self
    }

    fn report(&self, with: impl FnOnce(&Alive)) {
        if let Some(alive) = &self.report {
            with(alive);
        }
    }

    fn due_at(&self, at: DateTime<Utc>) {
        self.report(|alive| {
            alive.next_due(Some(SystemTime::from(at)));
        });
    }

    /// One round at `now`: run the due slot, if there is one that is not handled. Returns
    /// when to plan again: right away after a run, the retry or the next slot otherwise,
    /// `None` while the job is off.
    pub async fn round<L, F, Fut>(
        &mut self,
        ledger: &L,
        now: DateTime<Utc>,
        work: &mut F,
    ) -> Option<DateTime<Utc>>
    where
        L: Ledger,
        F: FnMut(Slot) -> Fut,
        Fut: Future<Output = Outcome>,
    {
        let Some(rule) = (self.job.rule)() else {
            self.report(|alive| {
                alive.set_expected(None);
                alive.next_due(None);
            });
            if !self.disabled {
                info!(job = %self.job.name, "Disabled");
                self.disabled = true;
                self.announced = None;
            }
            return None;
        };
        self.disabled = false;
        self.report(|alive| alive.set_expected(rule.interval()));
        let zone = self.job.zone;
        let next = rule.next(zone, now)?;

        match rule.due(zone, now, self.job.catch_up) {
            Some(slot) if self.handled.as_ref() != Some(&slot) => {
                if let Some((period, not_before)) = self.retry {
                    if period == slot.period && not_before > now {
                        self.due_at(slot.at);
                        return Some(not_before.min(next.at));
                    }
                    if period != slot.period {
                        self.give_up(period);
                    }
                }
                match ledger.is_done(&self.job.entry, &slot).await {
                    Ok(true) => self.handled = Some(slot),
                    Ok(false) => {
                        let at = slot.at;
                        self.execute(ledger, now, slot, work).await;
                        if self.retry.is_some() {
                            self.due_at(at);
                        }
                        return Some(now);
                    }
                    Err(error) => {
                        warn!(job = %self.job.name, "Could not read the ledger: {error:#}");
                        self.report(|alive| alive.failed("could not read the ledger"));
                        self.due_at(slot.at);
                        let again = now + TimeDelta::minutes(1);
                        self.retry = Some((slot.period, again));
                        return Some(again.min(next.at));
                    }
                }
            }
            Some(_) => {}
            None => {
                if let Some((period, _)) = self.retry {
                    self.give_up(period);
                }
            }
        }

        self.due_at(next.at);
        if self.announced != Some(next.at) {
            let secs = ((next.at - now).num_milliseconds().max(0) + 999) / 1000;
            info!(
                job = %self.job.name,
                "Next {} in {secs}s ({})",
                self.job.name,
                next.local.format("%a %d %b %H:%M")
            );
            self.announced = Some(next.at);
        }
        Some(next.at)
    }

    fn give_up(&mut self, period: Period) {
        warn!(job = %self.job.name, slot = %period, "Not retried any more: its catch-up window is over");
        self.report(|alive| {
            alive.gave_up(format!("slot {period} not done in its catch-up window"))
        });
        self.retry = None;
    }

    async fn execute<L, F, Fut>(&mut self, ledger: &L, now: DateTime<Utc>, slot: Slot, work: &mut F)
    where
        L: Ledger,
        F: FnMut(Slot) -> Fut,
        Fut: Future<Output = Outcome>,
    {
        let name = self.job.name.clone();
        let period = slot.period;
        let late = now - slot.at;
        if late > TimeDelta::minutes(1) && self.retry.is_none() {
            info!(
                job = %name,
                "Catching up the {name} of {} ({}h {}m ago)",
                slot.local.format("%a %d %b %H:%M"),
                late.num_hours(),
                late.num_minutes() % 60
            );
        }
        match self.job.delivery {
            Delivery::AtMostOnce => {
                if let Err(error) = ledger.mark_done(&self.job.entry, &slot).await {
                    warn!(job = %name, slot = %period, "Not run, could not record it: {error:#}");
                    self.report(|alive| alive.failed("could not record the slot"));
                    self.retry = Some((period, now + TimeDelta::minutes(1)));
                    return;
                }
                self.handled = Some(slot.clone());
                self.retry = None;
                match work(slot).await {
                    Outcome::Done => self.report(Alive::ok),
                    Outcome::Skipped => self.report(Alive::idle),
                    Outcome::RetryIn(_) => {
                        warn!(job = %name, slot = %period, "Failed; runs at most once, not retried");
                        self.report(|alive| {
                            alive.gave_up(format!("slot {period} failed; runs at most once"));
                        });
                    }
                }
            }
            Delivery::AtLeastOnce => match work(slot.clone()).await {
                outcome @ (Outcome::Done | Outcome::Skipped) => {
                    self.report(if outcome == Outcome::Done {
                        Alive::ok
                    } else {
                        Alive::idle
                    });
                    if let Err(error) = ledger.mark_done(&self.job.entry, &slot).await {
                        warn!(job = %name, slot = %period, "Could not record the run: {error:#}");
                    }
                    self.handled = Some(slot);
                    self.retry = None;
                }
                Outcome::RetryIn(after) => {
                    let after = after.max(Duration::from_secs(1));
                    info!(job = %name, slot = %period, "Not done; trying again in {after:?}");
                    self.report(|alive| {
                        alive.failed(format!(
                            "slot {period} not done; trying again in {} s",
                            after.as_secs()
                        ));
                    });
                    let after = TimeDelta::from_std(after).unwrap_or(TimeDelta::MAX);
                    self.retry = Some((period, now.checked_add_signed(after).unwrap_or(now)));
                }
            },
        }
    }
}

/// Run `job` for the rest of the bot's life (spawn it with
/// [`crate::supervise::supervise_ticking`] and [`Job::stall_after`]): each round ticks
/// `alive`, runs `work` for a due slot that `ledger` does not have, and sleeps until the next
/// slot or retry, [`MAX_SLEEP`] at most, or until the job's settings change.
///
/// The status file shows the task as `scheduled`, each slot's outcome reported to `alive`
/// ([`Runner::report_to`]), the rule's interval as the expected one and the next due slot.
pub async fn run<L, F, Fut>(job: Job, ledger: L, alive: Alive, work: F)
where
    L: Ledger,
    F: FnMut(Slot) -> Fut,
    Fut: Future<Output = Outcome>,
{
    alive.mark_scheduled();
    run_with(job, ledger, Some(alive), work).await
}

async fn run_with<L, F, Fut>(mut job: Job, ledger: L, alive: Option<Alive>, mut work: F)
where
    L: Ledger,
    F: FnMut(Slot) -> Fut,
    Fut: Future<Output = Outcome>,
{
    let mut changes = job.changes.take();
    let max_sleep = job.max_sleep;
    let mut runner = Runner::new(job);
    if let Some(alive) = &alive {
        runner = runner.report_to(alive.clone());
    }
    loop {
        if let Some(alive) = &alive {
            alive.tick();
        }
        let wake = runner.round(&ledger, Utc::now(), &mut work).await;
        let pause = wake
            .map_or(max_sleep, |at| {
                // Rounded up to whole milliseconds: never wake just short of the slot.
                let millis = (at - Utc::now()).num_milliseconds().max(0) as u64;
                Duration::from_millis(millis + u64::from(millis > 0))
            })
            .min(max_sleep);
        if pause.is_zero() {
            continue;
        }
        let closed = match &mut changes {
            Some(changes) => tokio::select! {
                () = sleep(pause) => false,
                changed = changes.changed() => changed.is_err(),
            },
            None => {
                sleep(pause).await;
                false
            }
        };
        if closed {
            changes = None;
        }
    }
}

// ── Posting ───────────────────────────────────────────────────────────────────

/// Where a [`post`] arrived.
#[derive(Debug, Default)]
pub struct Delivered {
    pub sent: Vec<(OwnedRoomId, OwnedEventId)>,
    pub failed: Vec<(OwnedRoomId, matrix_sdk::Error)>,
}

impl Delivered {
    /// [`Outcome::Done`] when every room has the post, or the rooms that failed did so for
    /// good (forbidden: trying again does not help) while others have it. Otherwise (a
    /// transient failure, or no room got it at all) [`Outcome::RetryIn`] `retry`: the next
    /// try sends with the same transaction IDs, so rooms that have it get no second copy.
    pub fn outcome(&self, retry: Duration) -> Outcome {
        let transient = self
            .failed
            .iter()
            .filter(|(_, error)| classify_error(error).0)
            .count();
        outcome_of(
            self.sent.len(),
            transient,
            self.failed.len() - transient,
            retry,
        )
    }
}

fn outcome_of(sent: usize, transient: usize, permanent: usize, retry: Duration) -> Outcome {
    if transient > 0 || (sent == 0 && permanent > 0) {
        Outcome::RetryIn(retry)
    } else {
        Outcome::Done
    }
}

/// Send `content` into each of `rooms`, with the transaction ID [`txn_for`]`("{key}:{room}")`
/// and transient failures retried until `deadline` ([`send_until`]). `key` names the
/// occurrence (bot, job, slot key, and the part of a slot's post), never only the content:
/// the homeserver answers a known transaction ID with the event it already has.
pub async fn post<C>(
    rooms: impl IntoIterator<Item = Room>,
    content: C,
    key: &str,
    deadline: Instant,
) -> Delivered
where
    C: MessageLikeEventContent + Clone,
{
    let mut delivered = Delivered::default();
    for room in rooms {
        let room_id = room.room_id().to_owned();
        let txn = txn_for(&format!("{key}:{room_id}"));
        match send_until(&room, content.clone(), &txn, deadline).await {
            Ok(event_id) => delivered.sent.push((room_id, event_id)),
            Err(error) => delivered.failed.push((room_id, error)),
        }
    }
    delivered
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const BERLIN: Zone = Zone::DEFAULT;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn local(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        date(y, m, d).and_hms_opt(h, min, 0).unwrap()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn hm(h: u32, m: u32) -> TimeOfDay {
        TimeOfDay::new(h, m)
    }

    const HOUR: Duration = Duration::from_secs(3600);

    #[test]
    fn local_occurrences_of_each_rule() {
        let daily = Rule::daily(hm(7, 0));
        assert_eq!(
            daily.next_local(local(2026, 9, 25, 6, 59)),
            Some(local(2026, 9, 25, 7, 0))
        );
        assert_eq!(
            daily.next_local(local(2026, 9, 25, 7, 0)),
            Some(local(2026, 9, 26, 7, 0))
        );
        assert_eq!(
            daily.latest_local(local(2026, 9, 25, 7, 0)),
            Some(local(2026, 9, 25, 7, 0))
        );
        assert_eq!(
            daily.latest_local(local(2026, 9, 25, 6, 59)),
            Some(local(2026, 9, 24, 7, 0))
        );

        let twice = Rule::Daily {
            at: vec![hm(20, 0), hm(8, 0)],
        };
        assert_eq!(
            twice.next_local(local(2026, 9, 25, 9, 0)),
            Some(local(2026, 9, 25, 20, 0))
        );
        assert_eq!(
            twice.latest_local(local(2026, 9, 25, 7, 0)),
            Some(local(2026, 9, 24, 20, 0))
        );
        assert_eq!(
            Rule::Daily { at: vec![] }.next_local(local(2026, 9, 25, 9, 0)),
            None
        );

        // 2026-09-25 is a Friday.
        let monday = Rule::Weekly {
            day: Weekday::Mon,
            at: hm(7, 0),
        };
        assert_eq!(
            monday.next_local(local(2026, 9, 25, 12, 0)),
            Some(local(2026, 9, 28, 7, 0))
        );
        assert_eq!(
            monday.latest_local(local(2026, 9, 25, 12, 0)),
            Some(local(2026, 9, 21, 7, 0))
        );
        let friday = Rule::Weekly {
            day: Weekday::Fri,
            at: hm(7, 0),
        };
        assert_eq!(
            friday.next_local(local(2026, 9, 25, 6, 0)),
            Some(local(2026, 9, 25, 7, 0))
        );
        assert_eq!(
            friday.next_local(local(2026, 9, 25, 8, 0)),
            Some(local(2026, 10, 2, 7, 0))
        );
        assert_eq!(
            friday.latest_local(local(2026, 9, 25, 6, 0)),
            Some(local(2026, 9, 18, 7, 0))
        );

        let last = Rule::Monthly {
            day: 31,
            at: hm(7, 0),
        };
        assert_eq!(
            last.next_local(local(2026, 4, 2, 8, 0)),
            Some(local(2026, 4, 30, 7, 0))
        );
        assert_eq!(
            last.next_local(local(2026, 1, 31, 8, 0)),
            Some(local(2026, 2, 28, 7, 0))
        );
        assert_eq!(
            last.latest_local(local(2026, 3, 10, 6, 0)),
            Some(local(2026, 2, 28, 7, 0))
        );
        let first = Rule::Monthly {
            day: 1,
            at: hm(7, 0),
        };
        assert_eq!(
            first.next_local(local(2026, 12, 15, 8, 0)),
            Some(local(2027, 1, 1, 7, 0))
        );
        assert_eq!(
            first.latest_local(local(2026, 1, 1, 6, 0)),
            Some(local(2025, 12, 1, 7, 0))
        );
    }

    #[test]
    fn slots_are_instants_across_both_dst_changes() {
        let daily = Rule::daily(hm(7, 0));
        // Spring: from Saturday 23:00 CET the 07:00 post is 7 hours away.
        let slot = daily.next(BERLIN, utc("2026-03-28T22:00:00Z")).unwrap();
        assert_eq!(slot.at, utc("2026-03-29T05:00:00Z"));
        assert_eq!(slot.local, local(2026, 3, 29, 7, 0));
        assert_eq!(slot.key(), "2026-03-29");
        // Autumn: from Saturday midnight CEST it is 8 hours away.
        let slot = daily.next(BERLIN, utc("2026-10-24T22:00:00Z")).unwrap();
        assert_eq!(slot.at, utc("2026-10-25T06:00:00Z"));
        assert_eq!(
            daily
                .latest(BERLIN, utc("2026-10-25T05:59:00Z"))
                .unwrap()
                .at,
            utc("2026-10-24T05:00:00Z")
        );

        // A time in the gap runs at 03:00 CEST, once.
        let gap = Rule::Weekly {
            day: Weekday::Sun,
            at: hm(2, 30),
        };
        let slot = gap.next(BERLIN, utc("2026-03-28T12:00:00Z")).unwrap();
        assert_eq!(slot.at, utc("2026-03-29T01:00:00Z"));
        assert_eq!(slot.key(), "2026-W13");
        assert_eq!(
            gap.next(BERLIN, slot.at).unwrap().at,
            utc("2026-04-05T00:30:00Z")
        );

        // A time in the overlap runs at the first 02:30; at the second it is past.
        let overlap = Rule::daily(hm(2, 30));
        let first = utc("2026-10-25T00:30:00Z");
        assert_eq!(
            overlap
                .next(BERLIN, utc("2026-10-25T00:00:00Z"))
                .unwrap()
                .at,
            first
        );
        let second = utc("2026-10-25T01:10:00Z"); // 02:10 CET, after the first 02:30
        assert_eq!(overlap.latest(BERLIN, second).unwrap().at, first);
        assert_eq!(
            overlap.next(BERLIN, second).unwrap().at,
            utc("2026-10-26T01:30:00Z")
        );
    }

    #[test]
    fn a_slot_is_due_within_its_catch_up_window() {
        let daily = Rule::daily(hm(7, 0));
        let at = utc("2026-10-12T05:00:00Z");
        assert_eq!(daily.due(BERLIN, at, 12 * HOUR).unwrap().at, at);
        let restart = utc("2026-10-12T09:55:00Z");
        assert_eq!(daily.due(BERLIN, restart, 12 * HOUR).unwrap().at, at);
        assert_eq!(daily.due(BERLIN, restart, 4 * HOUR), None);
    }

    #[test]
    fn period_keys_parse_back_and_later_periods_cover_earlier_ones() {
        let periods = [
            Period::Day(date(2026, 10, 30)),
            Period::DayAt(date(2026, 10, 30), hm(8, 0)),
            Period::Week {
                year: 2026,
                week: 44,
            },
            Period::Month {
                year: 2026,
                month: 10,
            },
        ];
        let keys: Vec<_> = periods.iter().map(Period::key).collect();
        assert_eq!(
            keys,
            ["2026-10-30", "2026-10-30 08:00", "2026-W44", "2026-10"]
        );
        for period in periods {
            assert_eq!(Period::parse(&period.key()), Some(period));
        }
        assert_eq!(Period::parse("2026-W54"), None);
        assert_eq!(Period::parse("2026-13"), None);
        assert_eq!(Period::parse("soon"), None);

        let monday = Period::Day(date(2026, 10, 12));
        let week = |week| Period::Week { year: 2026, week };
        assert!(monday.covers(&week(42)), "a day in the week");
        assert!(monday.covers(&week(41)));
        assert!(!monday.covers(&week(43)));
        assert!(
            Period::Day(date(2026, 10, 18)).covers(&week(42)),
            "its Sunday"
        );
        let october = Period::Month {
            year: 2026,
            month: 10,
        };
        assert!(Period::Day(date(2026, 10, 31)).covers(&october));
        assert!(!Period::Day(date(2026, 9, 30)).covers(&october));
        let morning = Period::DayAt(date(2026, 10, 30), hm(8, 0));
        let evening = Period::DayAt(date(2026, 10, 30), hm(20, 0));
        assert!(!morning.covers(&evening));
        assert!(evening.covers(&morning));
    }

    /// A ledger file in a temporary directory, and the directory.
    async fn open_ledger(dated: bool, content: Option<&str>) -> (JsonLedger, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("posted.json");
        if let Some(content) = content {
            std::fs::write(&path, content).unwrap();
        }
        let ledger = if dated {
            JsonLedger::open_dated(&path).await.unwrap()
        } else {
            JsonLedger::open(&path).await.unwrap()
        };
        (ledger, dir)
    }

    fn job(rule: Rule, delivery: Delivery, catch_up: Duration) -> Job {
        Job::new("daily summary", BERLIN, delivery, catch_up, move || {
            Some(rule.clone())
        })
        .entry("daily")
    }

    /// Run rounds with `outcomes` given out in turn (then `Done`); the slots run.
    struct Work {
        outcomes: RefCell<Vec<Outcome>>,
        ran: RefCell<Vec<Slot>>,
    }

    impl Work {
        fn new(outcomes: &[Outcome]) -> Self {
            Self {
                outcomes: RefCell::new(outcomes.iter().rev().copied().collect()),
                ran: RefCell::new(Vec::new()),
            }
        }

        async fn round(
            &self,
            runner: &mut Runner,
            ledger: &JsonLedger,
            now: &str,
        ) -> Option<DateTime<Utc>> {
            let mut work = |slot: Slot| {
                self.ran.borrow_mut().push(slot);
                let outcome = self.outcomes.borrow_mut().pop().unwrap_or(Outcome::Done);
                async move { outcome }
            };
            runner.round(ledger, utc(now), &mut work).await
        }

        fn ran(&self) -> Vec<DateTime<Utc>> {
            self.ran.borrow().iter().map(|slot| slot.at).collect()
        }
    }

    #[tokio::test]
    async fn a_restart_catches_up_the_missed_slot_once() {
        let (ledger, _dir) =
            open_ledger(true, Some(r#"{"daily":"2026-10-11","weekly":null}"#)).await;
        let mut runner = Runner::new(job(Rule::daily(hm(7, 0)), Delivery::AtLeastOnce, 12 * HOUR));
        let work = Work::new(&[]);
        // Restarted at 11:55: the 07:00 post is caught up, then the next is tomorrow's.
        let wake = work
            .round(&mut runner, &ledger, "2026-10-12T09:55:00Z")
            .await;
        assert_eq!(wake, Some(utc("2026-10-12T09:55:00Z")));
        assert_eq!(work.ran(), [utc("2026-10-12T05:00:00Z")]);
        assert_eq!(ledger.recorded("daily").as_deref(), Some("2026-10-12"));
        let wake = work
            .round(&mut runner, &ledger, "2026-10-12T09:56:00Z")
            .await;
        assert_eq!(wake, Some(utc("2026-10-13T05:00:00Z")));
        assert_eq!(work.ran().len(), 1);
        // The file kept its shape (other entries, null included).
        let file = std::fs::read_to_string(_dir.path().join("posted.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&file).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!({"daily": "2026-10-12", "weekly": null})
        );

        // A new process with that file runs nothing more today.
        let mut fresh = Runner::new(job(Rule::daily(hm(7, 0)), Delivery::AtLeastOnce, 12 * HOUR));
        work.round(&mut fresh, &ledger, "2026-10-12T10:00:00Z")
            .await;
        assert_eq!(work.ran().len(), 1);
        // Nor one started too late for the window, even with nothing recorded.
        let (empty, _empty_dir) = open_ledger(false, None).await;
        let mut late = Runner::new(job(Rule::daily(hm(7, 0)), Delivery::AtLeastOnce, 12 * HOUR));
        work.round(&mut late, &empty, "2026-10-12T17:30:00Z").await;
        assert_eq!(work.ran().len(), 1);
    }

    #[tokio::test]
    async fn moving_the_time_after_the_run_does_not_run_the_day_again() {
        let (ledger, _dir) = open_ledger(true, Some(r#"{"daily":"2026-10-12"}"#)).await;
        let mut runner = Runner::new(job(
            Rule::daily(hm(9, 30)),
            Delivery::AtLeastOnce,
            12 * HOUR,
        ));
        let work = Work::new(&[]);
        work.round(&mut runner, &ledger, "2026-10-12T07:31:00Z")
            .await;
        assert!(work.ran().is_empty());
    }

    #[tokio::test]
    async fn a_dated_ledger_handles_a_slot_only_on_its_own_date() {
        // Weekly on Monday 07:00; posted Monday 2026-10-26 (the overview up to Sunday).
        let (ledger, _dir) = open_ledger(true, None).await;
        let rule = Arc::new(Mutex::new(Rule::Weekly {
            day: Weekday::Mon,
            at: hm(7, 0),
        }));
        let current = rule.clone();
        let mut runner = Runner::new(
            Job::new(
                "weekly",
                BERLIN,
                Delivery::AtLeastOnce,
                84 * HOUR,
                move || Some(lock(&current).clone()),
            )
            .entry("weekly"),
        );
        let work = Work::new(&[]);
        work.round(&mut runner, &ledger, "2026-10-26T06:00:00Z")
            .await;
        assert_eq!(ledger.recorded("weekly").as_deref(), Some("2026-10-26"));

        // Moved to 09:00 the same day: handled, also for a new process.
        *lock(&rule) = Rule::Weekly {
            day: Weekday::Mon,
            at: hm(9, 0),
        };
        work.round(&mut runner, &ledger, "2026-10-26T08:00:00Z")
            .await;
        let mut fresh = Runner::new(Job::new(
            "weekly",
            BERLIN,
            Delivery::AtLeastOnce,
            84 * HOUR,
            || {
                Some(Rule::Weekly {
                    day: Weekday::Mon,
                    at: hm(9, 0),
                })
            },
        ));
        work.round(&mut fresh, &ledger, "2026-10-26T08:00:00Z")
            .await;
        assert_eq!(work.ran().len(), 1);

        // Moved to Sunday on Tuesday: Sunday 2026-11-01 (same ISO week) is posted, or
        // 11-02..11-07 would get no overview. Sunday 10-25 is within the catch-up window
        // and not its own date's post, so it is caught up at once (an overlap, as the
        // versions before the shared schedule did it).
        *lock(&rule) = Rule::Weekly {
            day: Weekday::Sun,
            at: hm(7, 0),
        };
        let wake = work
            .round(&mut runner, &ledger, "2026-10-27T10:00:00Z")
            .await;
        assert_eq!(wake, Some(utc("2026-10-27T10:00:00Z")));
        assert_eq!(work.ran()[1..], [utc("2026-10-25T06:00:00Z")]);
        assert_eq!(ledger.recorded("weekly").as_deref(), Some("2026-10-25"));
        let wake = work
            .round(&mut runner, &ledger, "2026-10-27T10:00:01Z")
            .await;
        assert_eq!(wake, Some(utc("2026-11-01T06:00:00Z")));
        work.round(&mut runner, &ledger, "2026-11-01T06:00:00Z")
            .await;
        assert_eq!(work.ran()[2..], [utc("2026-11-01T06:00:00Z")]);
        assert_eq!(ledger.recorded("weekly").as_deref(), Some("2026-11-01"));
        work.round(&mut runner, &ledger, "2026-11-01T06:01:00Z")
            .await;
        assert_eq!(work.ran().len(), 3);

        // With a window too short for that catch-up, the process that posted Monday's (same
        // ISO week as Sunday 11-01) still posts on Sunday.
        let (ledger, _dir) = open_ledger(true, None).await;
        *lock(&rule) = Rule::Weekly {
            day: Weekday::Mon,
            at: hm(7, 0),
        };
        let current = rule.clone();
        let mut runner = Runner::new(Job::new(
            "weekly",
            BERLIN,
            Delivery::AtLeastOnce,
            12 * HOUR,
            move || Some(lock(&current).clone()),
        ));
        let work = Work::new(&[]);
        work.round(&mut runner, &ledger, "2026-10-26T06:00:00Z")
            .await;
        *lock(&rule) = Rule::Weekly {
            day: Weekday::Sun,
            at: hm(7, 0),
        };
        work.round(&mut runner, &ledger, "2026-10-27T10:00:00Z")
            .await;
        work.round(&mut runner, &ledger, "2026-11-01T06:00:00Z")
            .await;
        assert_eq!(
            work.ran(),
            [utc("2026-10-26T06:00:00Z"), utc("2026-11-01T06:00:00Z")]
        );
    }

    #[tokio::test]
    async fn a_failed_slot_is_retried_and_recorded_only_when_done() {
        let (ledger, _dir) = open_ledger(false, None).await;
        let mut runner = Runner::new(job(Rule::daily(hm(7, 0)), Delivery::AtLeastOnce, 12 * HOUR));
        let quarter = Duration::from_secs(15 * 60);
        let work = Work::new(&[Outcome::RetryIn(quarter), Outcome::RetryIn(quarter)]);
        work.round(&mut runner, &ledger, "2026-10-12T05:00:00Z")
            .await;
        assert_eq!(ledger.recorded("daily"), None);
        // Not before the retry time.
        let wake = work
            .round(&mut runner, &ledger, "2026-10-12T05:05:00Z")
            .await;
        assert_eq!(wake, Some(utc("2026-10-12T05:15:00Z")));
        assert_eq!(work.ran().len(), 1);
        work.round(&mut runner, &ledger, "2026-10-12T05:15:00Z")
            .await;
        assert_eq!(ledger.recorded("daily"), None);
        work.round(&mut runner, &ledger, "2026-10-12T05:30:00Z")
            .await;
        assert_eq!(ledger.recorded("daily").as_deref(), Some("2026-10-12"));
        assert_eq!(work.ran().len(), 3);
        work.round(&mut runner, &ledger, "2026-10-12T05:45:00Z")
            .await;
        assert_eq!(work.ran().len(), 3);
    }

    #[tokio::test]
    async fn retries_end_with_the_catch_up_window() {
        let (ledger, _dir) = open_ledger(false, None).await;
        let mut runner = Runner::new(job(Rule::daily(hm(7, 0)), Delivery::AtLeastOnce, HOUR));
        let work = Work::new(&[Outcome::RetryIn(2 * HOUR)]);
        work.round(&mut runner, &ledger, "2026-10-12T05:00:00Z")
            .await;
        let wake = work
            .round(&mut runner, &ledger, "2026-10-12T07:00:00Z")
            .await;
        assert_eq!(wake, Some(utc("2026-10-13T05:00:00Z")));
        assert_eq!(work.ran().len(), 1);
        assert_eq!(ledger.recorded("daily"), None);
    }

    #[tokio::test]
    async fn at_most_once_is_recorded_before_the_work_and_never_retried() {
        let (ledger, _dir) = open_ledger(false, None).await;
        let mut runner = Runner::new(job(
            Rule::Daily {
                at: vec![hm(8, 0), hm(20, 0)],
            },
            Delivery::AtMostOnce,
            Duration::from_secs(300),
        ));
        let work = Work::new(&[Outcome::RetryIn(Duration::from_secs(60))]);
        work.round(&mut runner, &ledger, "2026-10-12T06:00:00Z")
            .await;
        assert_eq!(
            ledger.recorded("daily").as_deref(),
            Some("2026-10-12 08:00")
        );
        work.round(&mut runner, &ledger, "2026-10-12T06:02:00Z")
            .await;
        assert_eq!(work.ran().len(), 1);
        // The evening slot is a slot of its own.
        work.round(&mut runner, &ledger, "2026-10-12T18:00:00Z")
            .await;
        assert_eq!(
            ledger.recorded("daily").as_deref(),
            Some("2026-10-12 20:00")
        );
        assert_eq!(work.ran().len(), 2);
    }

    #[tokio::test]
    async fn a_job_that_is_off_waits_for_its_settings() {
        let (ledger, _dir) = open_ledger(false, None).await;
        let mut runner = Runner::new(Job::new("off", BERLIN, Delivery::AtLeastOnce, HOUR, || {
            None
        }));
        let work = Work::new(&[]);
        let wake = work
            .round(&mut runner, &ledger, "2026-10-12T05:00:00Z")
            .await;
        assert_eq!(wake, None);
        assert!(work.ran().is_empty());
    }

    #[tokio::test]
    async fn two_jobs_marking_at_once_both_stay_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("posted.json");
        let ledger = JsonLedger::open_dated(&path).await.unwrap();
        let rule = Rule::daily(hm(7, 0));
        for day in 1..=28 {
            let slot = rule.slot(BERLIN, local(2026, 2, day, 7, 0));
            let (a, b) = tokio::join!(
                ledger.mark_done("daily", &slot),
                ledger.mark_done("weekly", &slot)
            );
            a.unwrap();
            b.unwrap();
            let reloaded = JsonLedger::open_dated(&path).await.unwrap();
            let expected = slot.key();
            assert_eq!(reloaded.recorded("daily"), Some(expected.clone()));
            assert_eq!(reloaded.recorded("weekly"), Some(expected));
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn a_corrupt_ledger_is_moved_aside() {
        let (ledger, dir) = open_ledger(false, Some("{ not json")).await;
        assert_eq!(ledger.recorded("daily"), None);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn each_slot_is_reported_for_the_status_file() {
        use crate::supervise::{report, snapshot, Outcome as Round};
        let status = |name: &str| {
            snapshot()
                .into_iter()
                .find(|t| t.name == name)
                .expect("listed")
        };
        let (ledger, _dir) = open_ledger(false, None).await;
        let alive = report("schedule report under test");
        let mut runner = Runner::new(job(Rule::daily(hm(7, 0)), Delivery::AtLeastOnce, 2 * HOUR))
            .report_to(alive.clone());
        let quarter = Duration::from_secs(15 * 60);
        let work = Work::new(&[Outcome::RetryIn(quarter), Outcome::Skipped]);

        // Before the slot: the next due time is today's 07:00 (05:00 UTC), a day expected.
        work.round(&mut runner, &ledger, "2026-10-12T04:00:00Z")
            .await;
        let t = status("schedule report under test");
        assert_eq!(t.expected, Some(Duration::from_secs(86_400)));
        assert_eq!(t.next_due, Some(utc("2026-10-12T05:00:00Z").into()));
        assert_eq!(t.last_outcome, None);

        // Not done: failed, and the slot stays due.
        work.round(&mut runner, &ledger, "2026-10-12T05:00:00Z")
            .await;
        let t = status("schedule report under test");
        assert_eq!(t.last_outcome, Some(Round::Failed));
        assert_eq!(t.consecutive_failures, 1);
        assert_eq!(
            t.last_err_reason.as_deref(),
            Some("slot 2026-10-12 not done; trying again in 900 s")
        );
        assert_eq!(t.next_due, Some(utc("2026-10-12T05:00:00Z").into()));
        work.round(&mut runner, &ledger, "2026-10-12T05:05:00Z")
            .await;
        assert_eq!(
            status("schedule report under test").next_due,
            Some(utc("2026-10-12T05:00:00Z").into()),
            "waiting for the retry"
        );

        // Skipped (nothing to post): idle, healthy; the next due is tomorrow.
        work.round(&mut runner, &ledger, "2026-10-12T05:15:00Z")
            .await;
        work.round(&mut runner, &ledger, "2026-10-12T05:16:00Z")
            .await;
        let t = status("schedule report under test");
        assert_eq!(t.last_outcome, Some(Round::Idle));
        assert_eq!(
            (t.idle_total, t.failed_total, t.consecutive_failures),
            (1, 1, 0)
        );
        assert_eq!(t.next_due, Some(utc("2026-10-13T05:00:00Z").into()));

        // Tomorrow's slot keeps failing past its catch-up window: given up.
        let work = Work::new(&[Outcome::RetryIn(HOUR), Outcome::RetryIn(HOUR)]);
        work.round(&mut runner, &ledger, "2026-10-13T05:00:00Z")
            .await;
        work.round(&mut runner, &ledger, "2026-10-13T06:00:00Z")
            .await;
        work.round(&mut runner, &ledger, "2026-10-13T07:30:00Z")
            .await;
        let t = status("schedule report under test");
        assert_eq!(t.gave_up_total, 1);
        assert_eq!(t.last_outcome, Some(Round::GaveUp));
        assert_eq!(t.next_due, Some(utc("2026-10-14T05:00:00Z").into()));
    }

    #[tokio::test]
    async fn a_job_turned_off_expects_nothing() {
        let (ledger, _dir) = open_ledger(false, None).await;
        let alive = crate::supervise::report("schedule off under test");
        alive.expect_every(HOUR);
        let job = Job::new("off", BERLIN, Delivery::AtLeastOnce, HOUR, || None);
        let mut runner = Runner::new(job).report_to(alive);
        let work = Work::new(&[]);
        assert_eq!(
            work.round(&mut runner, &ledger, "2026-10-12T04:00:00Z")
                .await,
            None
        );
        let t = crate::supervise::snapshot()
            .into_iter()
            .find(|t| t.name == "schedule off under test")
            .unwrap();
        assert_eq!((t.expected, t.next_due), (None, None));
    }

    #[test]
    fn rule_intervals() {
        let day = Duration::from_secs(86_400);
        assert_eq!(Rule::daily(hm(7, 0)).interval(), Some(day));
        assert_eq!(Rule::Daily { at: vec![] }.interval(), None);
        assert_eq!(
            Rule::Weekly {
                day: Weekday::Mon,
                at: hm(7, 0)
            }
            .interval(),
            Some(7 * day)
        );
        assert_eq!(
            Rule::Monthly {
                day: 1,
                at: hm(7, 0)
            }
            .interval(),
            Some(31 * day)
        );
    }

    #[tokio::test]
    async fn run_runs_a_due_slot_right_away() {
        let (ledger, _dir) = open_ledger(false, None).await;
        let now = BERLIN.now().naive_local();
        let rule = Rule::daily(TimeOfDay::new(now.hour(), now.minute()));
        let job = Job::new("now", BERLIN, Delivery::AtLeastOnce, HOUR, move || {
            Some(rule.clone())
        });
        let (sent, mut ran) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run_with(job, ledger.clone(), None, move |slot: Slot| {
            sent.send(slot).unwrap();
            async { Outcome::Done }
        }));
        let slot = tokio::time::timeout(Duration::from_secs(5), ran.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ledger.recorded("now"), Some(slot.key()));
        task.abort();
    }

    #[test]
    fn a_post_is_done_unless_a_room_may_still_get_it() {
        let retry = Duration::from_secs(900);
        assert_eq!(outcome_of(2, 0, 0, retry), Outcome::Done);
        assert_eq!(outcome_of(0, 0, 0, retry), Outcome::Done, "no rooms");
        assert_eq!(outcome_of(1, 1, 0, retry), Outcome::RetryIn(retry));
        assert_eq!(
            outcome_of(1, 0, 1, retry),
            Outcome::Done,
            "forbidden in one"
        );
        assert_eq!(
            outcome_of(0, 0, 2, retry),
            Outcome::RetryIn(retry),
            "every send failed"
        );
    }
}
