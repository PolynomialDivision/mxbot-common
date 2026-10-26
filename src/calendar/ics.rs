//! iCalendar parsing and expansion into per-day entries (moved here from calendar-bot,
//! 2026-10-22, so calendar-bot and plenum-bot read calendars the same way).
//!
//! Reading is calcard's (2026-10-26; Stalwart's iCalendar parser, liberal in what it
//! accepts): folded lines, escapes, quoted parameters, `X-` properties and broken lines, and
//! the time zones clients write: IANA names, Windows names (Outlook, Exchange), Mozilla's
//! prefixed ones and a `VTIMEZONE` that names its zone (`X-LIC-LOCATION`,
//! `X-MICROSOFT-CDO-TZID`). A zone none of them names is taken as floating.
//!
//! Recurrence follows RFC 5545 (2026-10-26): the rule (the `rrule` crate) runs on the series'
//! wall clock in its own zone; each start then becomes a moment there (a time in the spring
//! gap moves on by the gap, one in the autumn overlap is the first of the two) and only then
//! a time in the zone the calendar is read in. A series written in UTC or in another
//! country's zone keeps its hour across that zone's DST changes, not ours. DTSTART is always
//! the first occurrence and `COUNT` counts it, `UNTIL` is a moment (a day for all-day
//! series), `RDATE` adds occurrences (times, days, periods with their own length), `EXDATE`
//! removes any and a `RECURRENCE-ID` replaces one (moved, changed or `STATUS:CANCELLED`).

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};

use calcard::{
    common::{timezone::Tz as Zone, PartialDateTime},
    icalendar::{
        timezone::TzResolver, ICalendar, ICalendarComponent, ICalendarComponentType,
        ICalendarDuration, ICalendarEntry, ICalendarFrequency, ICalendarPeriod, ICalendarProperty,
        ICalendarRecurrenceRule, ICalendarStatus, ICalendarValue, ICalendarWeekday,
    },
    Entry, Parser,
};
use chrono::{
    DateTime, Datelike, Duration, Local, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, Offset,
    TimeZone, Utc, Weekday,
};
use rrule::{Frequency, NWeekday, RRule, Unvalidated};

/// Starts a rule may produce before the window ends; beyond it the series is cut (with a
/// warning): a rule every second since 2020 must not stall a bot.
const MAX_RULE_STARTS: usize = 500_000;

// ── Parsed events ─────────────────────────────────────────────────────────────

/// A start or end, converted to the zone the calendar was read in (`parse_ics_in`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IcsTime {
    Date(NaiveDate),
    DateTime(NaiveDateTime),
}

impl IcsTime {
    pub fn date(self) -> NaiveDate {
        match self {
            IcsTime::Date(d) => d,
            IcsTime::DateTime(dt) => dt.date(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct IcsEvent {
    pub uid: Option<String>,
    pub summary: String,
    pub location: Option<String>,
    /// The (first) start, in the zone the calendar was read in.
    pub start: IcsTime,
    /// DTEND, in the zone the calendar was read in.
    pub end: Option<IcsTime>,
    /// Set on a single occurrence that replaces (moves or cancels) the occurrence of its
    /// series originally due on this local date.
    pub recurrence_id: Option<NaiveDate>,
    pub cancelled: bool,
    dtstart: Stamp,
    span: Span,
    rule: Option<ICalendarRecurrenceRule>,
    /// RDATE: further starts, a period's with its own length.
    rdates: Vec<(Stamp, Option<Span>)>,
    exdates: Vec<Stamp>,
    rid: Option<Stamp>,
    reader: Reader,
}

impl IcsEvent {
    /// Whether it has more than one occurrence (its keys carry their dates).
    fn is_series(&self) -> bool {
        self.rule.is_some() || !self.rdates.is_empty()
    }
}

/// A DATE or DATE-TIME as the calendar wrote it: a day, or a wall-clock time in its zone
/// (UTC, a fixed offset, a named zone or floating).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Stamp {
    Date(NaiveDate),
    Time { wall: NaiveDateTime, zone: Zone },
}

impl Stamp {
    fn moment(self) -> Moment {
        match self {
            Stamp::Date(d) => Moment::Day(d),
            Stamp::Time { wall, zone } => moment_of(wall, zone),
        }
    }
}

/// When something happens, to compare occurrences and to read them in a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Moment {
    Day(NaiveDate),
    /// A floating time: the same wall clock wherever it is read.
    Wall(NaiveDateTime),
    At(DateTime<Utc>),
}

/// A wall-clock time in `zone` as a moment (RFC 5545 §3.3.5): in the spring gap with the
/// offset from before the gap (02:30 becomes 03:30), in the autumn overlap the first one.
fn moment_of(wall: NaiveDateTime, zone: Zone) -> Moment {
    if zone.is_floating() {
        return Moment::Wall(wall);
    }
    match zone.from_local_datetime(&wall) {
        LocalResult::Single(t) => Moment::At(t.with_timezone(&Utc)),
        LocalResult::Ambiguous(a, b) => Moment::At(a.min(b).with_timezone(&Utc)),
        LocalResult::None => match zone
            .from_local_datetime(&(wall - Duration::hours(6)))
            .earliest()
        {
            Some(before) => {
                let offset = before.offset().fix().local_minus_utc();
                Moment::At(Utc.from_utc_datetime(&(wall - Duration::seconds(offset.into()))))
            }
            None => Moment::Wall(wall),
        },
    }
}

/// The zone a calendar is read in: a moment as its wall-clock time.
#[derive(Clone)]
struct Reader(Arc<dyn Fn(DateTime<Utc>) -> NaiveDateTime + Send + Sync>);

impl fmt::Debug for Reader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Reader")
    }
}

impl Reader {
    fn local(&self, m: Moment) -> NaiveDateTime {
        match m {
            Moment::Day(d) => d.and_time(NaiveTime::MIN),
            Moment::Wall(w) => w,
            Moment::At(t) => (self.0)(t),
        }
    }

    /// Whether two moments are the same occurrence: a day matches any time on it (an
    /// all-day EXDATE of a timed series), a floating time the same local time.
    fn same(&self, a: Moment, b: Moment) -> bool {
        match (a, b) {
            (Moment::At(x), Moment::At(y)) => x == y,
            (Moment::Day(x), Moment::Day(y)) => x == y,
            (Moment::Day(d), other) | (other, Moment::Day(d)) => self.local(other).date() == d,
            _ => self.local(a) == self.local(b),
        }
    }
}

/// How long an occurrence lasts: nominal days (the same wall-clock time days later, RFC 5545
/// §3.3.6) and an exact rest.
#[derive(Debug, Clone, Copy)]
struct Span {
    days: i64,
    exact: Duration,
}

impl Span {
    const ZERO: Span = Span {
        days: 0,
        exact: Duration::zero(),
    };

    fn of(start: Stamp, end: Option<Stamp>, duration: Option<&ICalendarDuration>) -> Span {
        match (start, end) {
            (Stamp::Date(s), Some(Stamp::Date(e))) => Span {
                days: (e - s).num_days().max(1),
                exact: Duration::zero(),
            },
            // DTEND: every occurrence lasts exactly as long as the first (RFC 5545 §3.8.5.3)
            (Stamp::Time { .. }, Some(end @ Stamp::Time { .. })) => {
                let exact = match (start.moment(), end.moment()) {
                    (Moment::At(s), Moment::At(e)) => e - s,
                    (s, e) => {
                        let wall = |m| match m {
                            Moment::Wall(w) => w,
                            Moment::At(t) => t.naive_utc(),
                            Moment::Day(d) => d.and_time(NaiveTime::MIN),
                        };
                        wall(e) - wall(s)
                    }
                };
                Span {
                    days: 0,
                    exact: exact.max(Duration::zero()),
                }
            }
            _ => match duration {
                Some(d) if !d.neg => Span {
                    days: i64::from(d.weeks) * 7 + i64::from(d.days),
                    exact: Duration::seconds(
                        i64::from(d.hours) * 3600
                            + i64::from(d.minutes) * 60
                            + i64::from(d.seconds),
                    ),
                },
                _ => match start {
                    Stamp::Date(_) => Span {
                        days: 1,
                        exact: Duration::zero(),
                    },
                    Stamp::Time { .. } => Span::ZERO,
                },
            },
        }
    }

    /// The end of an occurrence starting at `start`.
    fn end(self, start: Stamp) -> Moment {
        match start {
            Stamp::Date(d) => Moment::Day(d + Duration::days(self.days.max(1))),
            Stamp::Time { wall, zone } => match moment_of(wall + Duration::days(self.days), zone) {
                Moment::At(t) => Moment::At(t + self.exact),
                Moment::Wall(w) => Moment::Wall(w + self.exact),
                day @ Moment::Day(_) => day,
            },
        }
    }
}

/// The events of `ics`, their times in the machine's local zone (`TZ`).
pub fn parse_ics(ics: &str) -> Vec<IcsEvent> {
    parse_ics_in(ics, &Local)
}

/// The events of `ics`, their times in `zone`: a bot whose config names its zone reads
/// calendars in it, whatever the machine's zone is. Several `VCALENDAR`s in one text are all
/// read; a broken line or component costs only itself.
pub fn parse_ics_in<Z>(ics: &str, zone: &Z) -> Vec<IcsEvent>
where
    Z: TimeZone + Send + Sync + 'static,
{
    let zone = zone.clone();
    let reader = Reader(Arc::new(move |t: DateTime<Utc>| {
        t.with_timezone(&zone).naive_local()
    }));
    let mut parser = Parser::new(ics);
    let mut events = Vec::new();
    loop {
        match parser.entry() {
            Entry::ICalendar(cal) => events.extend(events_of(&cal, &reader)),
            Entry::Eof | Entry::TooManyComponents => break,
            _ => {}
        }
    }
    events
}

fn events_of(cal: &ICalendar, reader: &Reader) -> Vec<IcsEvent> {
    let zones = cal.build_tz_resolver();
    cal.components
        .iter()
        .filter(|c| c.component_type == ICalendarComponentType::VEvent)
        .filter_map(|c| event_of(c, &zones, reader))
        .collect()
}

/// A DATE or DATE-TIME value of `entry`: UTC and offsets as written, otherwise in its TZID.
fn stamp(
    value: &PartialDateTime,
    entry: &ICalendarEntry,
    zones: &TzResolver<&str>,
) -> Option<Stamp> {
    let dt = value.to_date_time()?;
    if !value.has_time() {
        return Some(Stamp::Date(dt.date_time.date()));
    }
    let zone = match dt.offset {
        Some(offset) if offset.local_minus_utc() == 0 => Zone::UTC,
        Some(offset) => Zone::Fixed(offset),
        None => zones.resolve_or_default(entry.tz_id()),
    };
    Some(Stamp::Time {
        wall: dt.date_time,
        zone,
    })
}

fn text(value: Option<&ICalendarValue>) -> Option<String> {
    value
        .and_then(ICalendarValue::as_text)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn event_of(
    comp: &ICalendarComponent,
    zones: &TzResolver<&str>,
    reader: &Reader,
) -> Option<IcsEvent> {
    let mut dtstart = None;
    let mut dtend = None;
    let mut duration = None;
    let mut rule = None;
    let mut rdates = Vec::new();
    let mut exdates = Vec::new();
    let mut rid = None;
    let mut uid = None;
    let mut summary = None;
    let mut location = None;
    let mut cancelled = false;
    for entry in &comp.entries {
        let first = entry.values.first();
        let first_stamp = || match first {
            Some(ICalendarValue::PartialDateTime(t)) => stamp(t, entry, zones),
            _ => None,
        };
        match (&entry.name, first) {
            (ICalendarProperty::Dtstart, _) => dtstart = first_stamp(),
            (ICalendarProperty::Dtend, _) => dtend = first_stamp(),
            (ICalendarProperty::RecurrenceId, _) => rid = first_stamp(),
            (ICalendarProperty::Duration, Some(ICalendarValue::Duration(d))) => {
                duration = Some(d.clone())
            }
            // more than one RRULE (deprecated in RFC 5545): the first
            (ICalendarProperty::Rrule, Some(ICalendarValue::RecurrenceRule(r)))
                if rule.is_none() =>
            {
                rule = Some((**r).clone())
            }
            (ICalendarProperty::Rdate, _) => {
                rdates.extend(entry.values.iter().filter_map(|v| match v {
                    ICalendarValue::PartialDateTime(t) => Some((stamp(t, entry, zones)?, None)),
                    ICalendarValue::Period(ICalendarPeriod::Range { start, end }) => {
                        let start = stamp(start, entry, zones)?;
                        let span = Span::of(start, stamp(end, entry, zones), None);
                        Some((start, Some(span)))
                    }
                    ICalendarValue::Period(ICalendarPeriod::Duration { start, duration }) => {
                        let start = stamp(start, entry, zones)?;
                        Some((start, Some(Span::of(start, None, Some(duration)))))
                    }
                    _ => None,
                }))
            }
            (ICalendarProperty::Exdate, _) => {
                exdates.extend(entry.values.iter().filter_map(|v| match v {
                    ICalendarValue::PartialDateTime(t) => stamp(t, entry, zones),
                    _ => None,
                }))
            }
            (ICalendarProperty::Uid, _) => uid = text(first),
            (ICalendarProperty::Summary, _) => summary = text(first),
            (ICalendarProperty::Location, _) => location = text(first),
            (
                ICalendarProperty::Status,
                Some(ICalendarValue::Status(ICalendarStatus::Cancelled)),
            ) => cancelled = true,
            _ => {}
        }
    }
    // an occurrence without its own DTSTART starts where it was due
    let dtstart = dtstart.or(rid)?;
    let public = |s: Stamp| match s {
        Stamp::Date(d) => IcsTime::Date(d),
        Stamp::Time { .. } => IcsTime::DateTime(reader.local(s.moment())),
    };
    Some(IcsEvent {
        uid,
        summary: summary.unwrap_or_default(),
        location,
        start: public(dtstart),
        end: dtend.map(public),
        recurrence_id: rid.map(|r| reader.local(r.moment()).date()),
        cancelled,
        dtstart,
        span: Span::of(dtstart, dtend, duration.as_ref()),
        rule,
        rdates,
        exdates,
        rid,
        reader: reader.clone(),
    })
}

// ── Recurrence ────────────────────────────────────────────────────────────────

fn weekday(day: ICalendarWeekday) -> Weekday {
    match day {
        ICalendarWeekday::Monday => Weekday::Mon,
        ICalendarWeekday::Tuesday => Weekday::Tue,
        ICalendarWeekday::Wednesday => Weekday::Wed,
        ICalendarWeekday::Thursday => Weekday::Thu,
        ICalendarWeekday::Friday => Weekday::Fri,
        ICalendarWeekday::Saturday => Weekday::Sat,
        ICalendarWeekday::Sunday => Weekday::Sun,
    }
}

/// `rrule` runs on wall-clock times: they are carried as UTC, which has no DST.
fn carrier(wall: NaiveDateTime) -> DateTime<rrule::Tz> {
    rrule::Tz::UTC.from_utc_datetime(&wall)
}

/// The series' wall clock and zone (an all-day series: midnight, floating).
fn wall_and_zone(start: Stamp) -> (NaiveDateTime, Zone) {
    match start {
        Stamp::Date(d) => (d.and_time(NaiveTime::MIN), Zone::Floating),
        Stamp::Time { wall, zone } => (wall, zone),
    }
}

/// UNTIL on the series' wall clock: a moment (`…Z`) converted to the series' zone, a day
/// inclusive, a floating time as written (also beside a TZID start, which RFC 5545 forbids
/// but clients write).
fn until_wall(until: &PartialDateTime, start: Stamp, reader: &Reader) -> Option<NaiveDateTime> {
    let dt = until.to_date_time()?;
    if !until.has_time() {
        return dt.date_time.date().and_hms_opt(23, 59, 59);
    }
    let Some(offset) = dt.offset else {
        return Some(dt.date_time);
    };
    let at =
        Utc.from_utc_datetime(&(dt.date_time - Duration::seconds(offset.local_minus_utc().into())));
    Some(match wall_and_zone(start) {
        (_, zone) if !zone.is_floating() => zone.from_utc_datetime(&at.naive_utc()).naive_local(),
        // a floating or all-day series meets the moment in the zone it is read in
        _ => reader.local(Moment::At(at)),
    })
}

fn rrule_of(rule: &ICalendarRecurrenceRule, start: Stamp, reader: &Reader) -> RRule<Unvalidated> {
    let freq = match rule.freq {
        ICalendarFrequency::Yearly => Frequency::Yearly,
        ICalendarFrequency::Monthly => Frequency::Monthly,
        ICalendarFrequency::Weekly => Frequency::Weekly,
        ICalendarFrequency::Daily => Frequency::Daily,
        ICalendarFrequency::Hourly => Frequency::Hourly,
        ICalendarFrequency::Minutely => Frequency::Minutely,
        ICalendarFrequency::Secondly => Frequency::Secondly,
    };
    // COUNT is counted in `IcsEvent::starts`, which may start the rule earlier.
    let mut r = RRule::new(freq).interval(rule.interval.unwrap_or(1).max(1));
    if let Some(until) = rule
        .until
        .as_ref()
        .and_then(|u| until_wall(u, start, reader))
    {
        r = r.until(carrier(until));
    }
    if let Some(day) = rule.wkst {
        r = r.week_start(weekday(day));
    }
    if !rule.byday.is_empty() {
        r = r.by_weekday(
            rule.byday
                .iter()
                .map(|d| match d.ordwk {
                    Some(n) => NWeekday::Nth(n, weekday(d.weekday)),
                    None => NWeekday::Every(weekday(d.weekday)),
                })
                .collect(),
        );
    }
    if !rule.bymonth.is_empty() {
        // leap months (RSCALE, RFC 7529) have no Gregorian month
        let months: Vec<chrono::Month> = rule
            .bymonth
            .iter()
            .filter(|m| !m.is_leap())
            .filter_map(|m| chrono::Month::try_from(m.month()).ok())
            .collect();
        r = r.by_month(&months);
    }
    if !rule.bymonthday.is_empty() {
        r = r.by_month_day(rule.bymonthday.clone());
    }
    if !rule.byyearday.is_empty() {
        r = r.by_year_day(rule.byyearday.clone());
    }
    if !rule.byweekno.is_empty() {
        r = r.by_week_no(rule.byweekno.clone());
    }
    if !rule.bysetpos.is_empty() {
        r = r.by_set_pos(rule.bysetpos.clone());
    }
    if !rule.byhour.is_empty() {
        r = r.by_hour(rule.byhour.clone());
    }
    if !rule.byminute.is_empty() {
        r = r.by_minute(rule.byminute.clone());
    }
    if !rule.bysecond.is_empty() {
        r = r.by_second(rule.bysecond.clone());
    }
    r
}

/// Where the rule starts running. BYSETPOS counts in the whole first week, month or year,
/// also the days before DTSTART (RFC 5545 §3.3.10); rrule, like dateutil, begins a WEEKLY
/// rule's first week at DTSTART (`BYDAY=MO,WE,FR;BYSETPOS=2` from a Wednesday took the
/// Friday). So such a rule runs from its period's start and the dates before DTSTART are
/// dropped. Only with a BYDAY, BYMONTHDAY, BYYEARDAY or BYWEEKNO: without one the rule takes
/// its day from DTSTART, which must then stay.
fn set_pos_start(rule: &ICalendarRecurrenceRule, first: NaiveDateTime) -> NaiveDateTime {
    let has_days = !(rule.byday.is_empty()
        && rule.bymonthday.is_empty()
        && rule.byyearday.is_empty()
        && rule.byweekno.is_empty());
    if rule.bysetpos.is_empty() || !has_days {
        return first;
    }
    let date = first.date();
    let start = match rule.freq {
        ICalendarFrequency::Weekly => {
            let week_start = rule.wkst.map_or(Weekday::Mon, weekday);
            let back =
                (7 + date.weekday().num_days_from_monday() - week_start.num_days_from_monday()) % 7;
            date - Duration::days(back.into())
        }
        ICalendarFrequency::Monthly => date.with_day(1).unwrap_or(date),
        ICalendarFrequency::Yearly => date.with_ordinal(1).unwrap_or(date),
        _ => date,
    };
    start.and_time(first.time())
}

impl IcsEvent {
    /// Every start of the event with its length, before EXDATE and replaced occurrences
    /// are taken out: the rule's from DTSTART up to `up_to` (wall clock, a margin past the
    /// window), then every RDATE (COUNT does not count them) that is not already one.
    fn starts(&self, up_to: NaiveDateTime) -> Vec<(Stamp, Span)> {
        let mut starts: Vec<(Stamp, Span)> = self
            .rule_starts(up_to)
            .into_iter()
            .map(|s| (s, self.span))
            .collect();
        for &(rdate, span) in &self.rdates {
            let at = rdate.moment();
            if !starts.iter().any(|(s, _)| self.reader.same(s.moment(), at)) {
                starts.push((rdate, span.unwrap_or(self.span)));
            }
        }
        starts
    }

    fn rule_starts(&self, up_to: NaiveDateTime) -> Vec<Stamp> {
        let Some(rule) = &self.rule else {
            return vec![self.dtstart];
        };
        let (first, zone) = wall_and_zone(self.dtstart);
        let as_stamp = |wall: NaiveDateTime| match self.dtstart {
            Stamp::Date(_) => Stamp::Date(wall.date()),
            Stamp::Time { .. } => Stamp::Time { wall, zone },
        };
        let count = rule.count.map(|c| c as usize);
        let mut walls = Vec::new();
        match rrule_of(rule, self.dtstart, &self.reader).build(carrier(set_pos_start(rule, first)))
        {
            Ok(set) => {
                for t in &set {
                    let wall = t.naive_utc();
                    if wall < first {
                        continue;
                    }
                    if wall > up_to || count.is_some_and(|c| walls.len() >= c) {
                        break;
                    }
                    if walls.len() == MAX_RULE_STARTS {
                        tracing::warn!(uid = ?self.uid, "Calendar: a series cut after {MAX_RULE_STARTS} occurrences");
                        break;
                    }
                    walls.push(wall);
                }
            }
            // UNTIL before DTSTART and rules RFC 5545 does not allow: the first date stays
            Err(e) => tracing::warn!(uid = ?self.uid, "Calendar: a recurrence rule not used: {e}"),
        }
        // DTSTART is always the first occurrence, also off the rule, and COUNT counts it
        // (RFC 5545 §3.3.10); rrule, like dateutil, yields only the rule's dates.
        if walls.first() != Some(&first) {
            walls.insert(0, first);
            if let Some(count) = count {
                walls.truncate(count);
            }
        }
        walls.into_iter().map(as_stamp).collect()
    }
}

// ── Expansion into days ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryTime {
    AllDay,
    Timed {
        start: NaiveTime,
        end: Option<NaiveTime>,
    },
}

/// One event as it appears on one day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayEntry {
    pub time: EntryTime,
    pub summary: String,
    pub location: Option<String>,
    /// First and last day the occurrence covers (equal for single-day events).
    pub first_day: NaiveDate,
    pub last_day: NaiveDate,
    /// Start of the occurrence, for "what's next".
    pub starts_at: NaiveDateTime,
    /// Which occurrence this is, stable when it is moved: the UID of a single
    /// event, the UID and original date of one of a series. Without a UID
    /// the title and start, so such an event can only appear or vanish.
    pub key: String,
}

impl DayEntry {
    pub fn is_multi_day(&self) -> bool {
        self.last_day > self.first_day
    }

    fn sort_key(&self, day: NaiveDate) -> (u8, NaiveTime) {
        match self.time {
            // Events already running (all-day, or continuing from earlier
            // days) frame the day, so they come first, as in calendar apps.
            _ if self.first_day < day => (0, NaiveTime::MIN),
            EntryTime::AllDay => (0, NaiveTime::MIN),
            EntryTime::Timed { start, .. } => (1, start),
        }
    }
}

/// One occurrence before it is laid out on days.
struct Occurrence<'a> {
    /// Whose title, place and length it has.
    event: &'a IcsEvent,
    /// The series' date it stands for (its key), in the zone the calendar is read in.
    original: NaiveDate,
    start: Stamp,
    span: Span,
}

impl Occurrence<'_> {
    /// First and last day, how it shows on them, and its start, in the reading zone.
    fn layout(&self) -> (NaiveDate, NaiveDate, EntryTime, NaiveDateTime) {
        let reader = &self.event.reader;
        match self.start {
            Stamp::Date(d) => (
                d,
                d + Duration::days(self.span.days.max(1) - 1),
                EntryTime::AllDay,
                d.and_time(NaiveTime::MIN),
            ),
            Stamp::Time { .. } => {
                let starts_at = reader.local(self.start.moment());
                let ends_at = reader.local(self.span.end(self.start)).max(starts_at);
                // An event ending exactly at midnight does not occupy the next day.
                let last_day = if ends_at > starts_at && ends_at.time() == NaiveTime::MIN {
                    ends_at.date() - Duration::days(1)
                } else {
                    ends_at.date()
                };
                let end = (ends_at > starts_at).then(|| ends_at.time());
                (
                    starts_at.date(),
                    last_day.max(starts_at.date()),
                    EntryTime::Timed {
                        start: starts_at.time(),
                        end,
                    },
                    starts_at,
                )
            }
        }
    }
}

/// Every event occurring on each of the `days` days starting at `from`,
/// sorted within the day. Occurrences listed by several calendars (same UID
/// and start) appear once.
pub fn expand(events: &[IcsEvent], from: NaiveDate, days: i64) -> Vec<(NaiveDate, Vec<DayEntry>)> {
    let to = from + Duration::days(days - 1);
    // Series run on their own wall clock: two days past the window cover every zone.
    let up_to = (to + Duration::days(2)).and_time(NaiveTime::MIN);
    // Occurrences replaced by a separate RECURRENCE-ID event, per series.
    let mut replaced: HashMap<&str, Vec<Moment>> = HashMap::new();
    for ev in events {
        if let (Some(uid), Some(rid)) = (ev.uid.as_deref(), ev.rid) {
            replaced.entry(uid).or_default().push(rid.moment());
        }
    }

    let mut occurrences = Vec::new();
    for ev in events {
        if let Some(rid) = ev.rid {
            // a moved or changed occurrence is an event of its own; a cancelled one is gone
            if !ev.cancelled {
                occurrences.push(Occurrence {
                    event: ev,
                    original: ev.reader.local(rid.moment()).date(),
                    start: ev.dtstart,
                    span: ev.span,
                });
            }
            continue;
        }
        if ev.cancelled {
            continue;
        }
        let gone = ev.uid.as_deref().and_then(|uid| replaced.get(uid));
        for (start, span) in ev.starts(up_to) {
            let at = start.moment();
            if ev.exdates.iter().any(|x| ev.reader.same(x.moment(), at))
                || gone.is_some_and(|g| g.iter().any(|&m| ev.reader.same(m, at)))
            {
                continue;
            }
            occurrences.push(Occurrence {
                event: ev,
                original: ev.reader.local(at).date(),
                start,
                span,
            });
        }
    }

    let mut seen: HashSet<(String, NaiveDateTime)> = HashSet::new();
    let mut by_day: Vec<(NaiveDate, Vec<DayEntry>)> = (0..days)
        .map(|i| (from + Duration::days(i), Vec::new()))
        .collect();
    for occ in occurrences {
        let (first_day, last_day, time, starts_at) = occ.layout();
        if last_day < from || first_day > to {
            continue;
        }
        let ev = occ.event;
        if !seen.insert((
            ev.uid.clone().unwrap_or_else(|| ev.summary.clone()),
            starts_at,
        )) {
            continue;
        }
        let key = match ev.uid.as_deref() {
            Some(uid) if ev.rid.is_some() || ev.is_series() => format!("{uid}#{}", occ.original),
            Some(uid) => uid.to_owned(),
            None => format!("{}#{starts_at}", ev.summary),
        };
        let entry = DayEntry {
            time,
            summary: ev.summary.clone(),
            location: ev.location.clone(),
            first_day,
            last_day,
            starts_at,
            key,
        };
        for (day, entries) in &mut by_day {
            if *day >= first_day && *day <= last_day {
                entries.push(entry.clone());
            }
        }
    }

    for (day, entries) in &mut by_day {
        let day = *day;
        entries.sort_by(|a, b| {
            a.sort_key(day)
                .cmp(&b.sort_key(day))
                .then_with(|| a.summary.cmp(&b.summary))
        });
    }
    by_day
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn vevent(lines: &str) -> String {
        format!("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\n{lines}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n")
    }

    fn days_with(events: &[IcsEvent], from: NaiveDate, days: i64) -> Vec<NaiveDate> {
        expand(events, from, days)
            .into_iter()
            .filter(|(_, e)| !e.is_empty())
            .map(|(d, _)| d)
            .collect()
    }

    fn rule_days(dtstart: &str, rrule: &str, from: NaiveDate, days: i64) -> Vec<NaiveDate> {
        let ics = vevent(&format!(
            "UID:x\r\nSUMMARY:S\r\nDTSTART:{dtstart}\r\nRRULE:{rrule}"
        ));
        days_with(&parse_ics(&ics), from, days)
    }

    #[test]
    fn bysetpos_picks_the_third_thursday_not_every_thursday() {
        // From the live Nextcloud data: FREQ=MONTHLY;BYDAY=TH;BYSETPOS=3.
        let days = rule_days(
            "20260917T190000",
            "FREQ=MONTHLY;BYDAY=TH;BYSETPOS=3",
            date(2026, 10, 1),
            61,
        );
        assert_eq!(days, vec![date(2026, 10, 15), date(2026, 11, 19)]);
    }

    #[test]
    fn bysetpos_counts_the_days_before_dtstart_in_the_first_month() {
        // The finance plenum (2026-10-26): from its first date, the 3rd Thursday, not the
        // 29 October (the 3rd Thursday from DTSTART on).
        let days = rule_days(
            "20261015T200000",
            "FREQ=MONTHLY;BYDAY=TH;BYSETPOS=3",
            date(2026, 10, 1),
            61,
        );
        assert_eq!(days, vec![date(2026, 10, 15), date(2026, 11, 19)]);
        let weekly = rule_days(
            "20261028T090000",
            "FREQ=WEEKLY;BYDAY=MO,WE,FR;BYSETPOS=2",
            date(2026, 10, 26),
            14,
        );
        assert_eq!(weekly, vec![date(2026, 10, 28), date(2026, 11, 4)]);
    }

    #[test]
    fn dtstart_is_the_first_occurrence_also_off_the_rule() {
        // A Monday event repeated on Tuesdays and Thursdays: Nextcloud shows the Monday, and
        // COUNT counts it.
        let days = rule_days(
            "20261026T190000",
            "FREQ=WEEKLY;BYDAY=TU,TH;COUNT=3",
            date(2026, 10, 26),
            14,
        );
        assert_eq!(
            days,
            vec![date(2026, 10, 26), date(2026, 10, 27), date(2026, 10, 29)]
        );
    }

    #[test]
    fn wkst_sets_the_weeks_of_an_interval() {
        // Every other week on Sunday and Tuesday, weeks from Sunday (as Apple writes it): the
        // Tuesday after the first Sunday is in the same week.
        let days = rule_days(
            "20261025T100000",
            "FREQ=WEEKLY;INTERVAL=2;WKST=SU;BYDAY=SU,TU",
            date(2026, 10, 25),
            21,
        );
        assert_eq!(
            days,
            vec![
                date(2026, 10, 25),
                date(2026, 10, 27),
                date(2026, 11, 8),
                date(2026, 11, 10)
            ]
        );
        // Weeks from Monday (the default): that Tuesday is in the next week, one not taken.
        let days = rule_days(
            "20261025T100000",
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=SU,TU",
            date(2026, 10, 25),
            21,
        );
        assert_eq!(
            days,
            vec![date(2026, 10, 25), date(2026, 11, 3), date(2026, 11, 8)]
        );
    }

    #[test]
    fn an_escaped_backslash_before_n_is_no_newline() {
        let ev = &parse_ics(&vevent(
            "UID:x\r\nSUMMARY:C:\\\\new\\, now\\nnext\r\nDTSTART:20261027T100000",
        ))[0];
        assert_eq!(ev.summary, "C:\\new, now\nnext");
    }

    #[test]
    fn ordinal_byday_and_fifth_sunday() {
        let first_sunday = rule_days(
            "20231029T190000",
            "FREQ=MONTHLY;BYDAY=1SU",
            date(2026, 10, 1),
            61,
        );
        assert_eq!(first_sunday, vec![date(2026, 10, 4), date(2026, 11, 1)]);

        // Only months that have a fifth Sunday: Nov 2026 (29th), not Oct.
        let fifth = rule_days(
            "20231224T190000",
            "FREQ=MONTHLY;BYDAY=5SU",
            date(2026, 10, 1),
            61,
        );
        assert_eq!(fifth, vec![date(2026, 11, 29)]);

        let last_friday = rule_days(
            "20260101T100000",
            "FREQ=MONTHLY;BYDAY=-1FR",
            date(2026, 9, 1),
            30,
        );
        assert_eq!(last_friday, vec![date(2026, 9, 25)]);
    }

    #[test]
    fn interval_count_and_until_are_honoured() {
        // Every other Monday.
        let biweekly = rule_days(
            "20260907T180000",
            "FREQ=WEEKLY;INTERVAL=2",
            date(2026, 9, 1),
            30,
        );
        assert_eq!(biweekly, vec![date(2026, 9, 7), date(2026, 9, 21)]);

        // Tue+Thu, every other week, starting on a Tuesday.
        let pair = rule_days(
            "20260908T180000",
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU,TH",
            date(2026, 9, 7),
            21,
        );
        assert_eq!(
            pair,
            vec![
                date(2026, 9, 8),
                date(2026, 9, 10),
                date(2026, 9, 22),
                date(2026, 9, 24)
            ]
        );

        let three = rule_days(
            "20260901T090000",
            "FREQ=DAILY;COUNT=3",
            date(2026, 9, 1),
            10,
        );
        assert_eq!(
            three,
            vec![date(2026, 9, 1), date(2026, 9, 2), date(2026, 9, 3)]
        );

        let until = rule_days(
            "20260901T090000",
            "FREQ=DAILY;UNTIL=20260902T235959Z",
            date(2026, 9, 1),
            10,
        );
        // UNTIL is a moment: in a zone east of UTC "23:59:59Z" is already
        // the next local day, yet before that day's 09:00 occurrence.
        let until_local = Utc
            .with_ymd_and_hms(2026, 9, 2, 23, 59, 59)
            .unwrap()
            .with_timezone(&Local)
            .naive_local();
        let expected: Vec<NaiveDate> = (1..=3)
            .map(|d| date(2026, 9, d))
            .filter(|d| d.and_hms_opt(9, 0, 0).unwrap() <= until_local)
            .collect();
        assert_eq!(until, expected);
        let until_date = rule_days(
            "20260901",
            "FREQ=DAILY;UNTIL=20260902",
            date(2026, 9, 1),
            10,
        );
        assert_eq!(until_date, vec![date(2026, 9, 1), date(2026, 9, 2)]);

        let monthday = rule_days(
            "20260131T090000",
            "FREQ=MONTHLY;BYMONTHDAY=-1",
            date(2026, 2, 1),
            60,
        );
        assert_eq!(monthday, vec![date(2026, 2, 28), date(2026, 3, 31)]);

        let yearly = rule_days("20200229", "FREQ=YEARLY", date(2026, 1, 1), 365);
        assert!(yearly.is_empty(), "no 29 Feb in 2026");
    }

    #[test]
    fn moved_and_cancelled_occurrences_replace_the_series_date() {
        // Series every Sunday; the 11 Oct occurrence moved to Monday 12 Oct,
        // the 18 Oct occurrence cancelled.
        let ics = "BEGIN:VCALENDAR\r\n\
            BEGIN:VEVENT\r\nUID:s\r\nSUMMARY:Plenum\r\nDTSTART;TZID=Europe/Berlin:20261004T190000\r\nRRULE:FREQ=WEEKLY\r\nEND:VEVENT\r\n\
            BEGIN:VEVENT\r\nUID:s\r\nSUMMARY:Plenum\r\nRECURRENCE-ID;TZID=Europe/Berlin:20261011T190000\r\nDTSTART;TZID=Europe/Berlin:20261012T190000\r\nEND:VEVENT\r\n\
            BEGIN:VEVENT\r\nUID:s\r\nSUMMARY:Plenum\r\nRECURRENCE-ID;TZID=Europe/Berlin:20261018T190000\r\nDTSTART;TZID=Europe/Berlin:20261018T190000\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\n\
            END:VCALENDAR\r\n";

        // read in the series' own zone: in a zone far east, 19:00 Berlin is the next day
        let berlin: chrono_tz::Tz = "Europe/Berlin".parse().unwrap();
        let events = parse_ics_in(ics, &berlin);
        let days = days_with(&events, date(2026, 10, 4), 21);

        assert_eq!(days, vec![date(2026, 10, 4), date(2026, 10, 12)]);

        // The moved occurrence keeps its key (the series date it replaces),
        // so the week update reports it as moved, not as new and cancelled.
        let keys: Vec<String> = expand(&events, date(2026, 10, 4), 21)
            .into_iter()
            .flat_map(|(_, entries)| entries)
            .map(|e| e.key)
            .collect();
        assert_eq!(keys, ["s#2026-10-04", "s#2026-10-11"]);
        let single = vevent("UID:one\r\nSUMMARY:Party\r\nDTSTART:20261012T200000");
        assert_eq!(
            expand(&parse_ics(&single), date(2026, 10, 12), 1)[0].1[0].key,
            "one"
        );
    }

    #[test]
    fn exdate_lists_and_cancelled_events() {
        let ics = vevent(
            "UID:d\r\nSUMMARY:Standup\r\nDTSTART:20260901T090000\r\nRRULE:FREQ=DAILY;COUNT=5\r\n\
             EXDATE:20260902T090000,20260904T090000",
        );
        assert_eq!(
            days_with(&parse_ics(&ics), date(2026, 9, 1), 7),
            vec![date(2026, 9, 1), date(2026, 9, 3), date(2026, 9, 5)]
        );

        let cancelled =
            vevent("UID:c\r\nSUMMARY:Off\r\nDTSTART:20260901T090000\r\nSTATUS:CANCELLED");
        assert!(days_with(&parse_ics(&cancelled), date(2026, 9, 1), 3).is_empty());
    }

    #[test]
    fn multi_day_events_cover_every_day_they_span() {
        // All-day DTEND is exclusive: 25–27 Sep.
        let trip = vevent(
            "UID:t\r\nSUMMARY:Trip\r\nDTSTART;VALUE=DATE:20260925\r\nDTEND;VALUE=DATE:20260928",
        );
        assert_eq!(
            days_with(&parse_ics(&trip), date(2026, 9, 26), 5),
            vec![date(2026, 9, 26), date(2026, 9, 27)]
        );

        // A party from 22:00 to 02:00 touches two days; one ending at
        // midnight only one.
        let night =
            vevent("UID:n\r\nSUMMARY:Party\r\nDTSTART:20260925T220000\r\nDTEND:20260926T020000");
        assert_eq!(days_with(&parse_ics(&night), date(2026, 9, 25), 3).len(), 2);
        let midnight = vevent("UID:m\r\nSUMMARY:Talk\r\nDTSTART:20260925T220000\r\nDURATION:PT2H");
        assert_eq!(
            days_with(&parse_ics(&midnight), date(2026, 9, 25), 3),
            vec![date(2026, 9, 25)]
        );
    }

    #[test]
    fn times_are_read_in_the_given_zone_whatever_the_machine_is_in() {
        let berlin: chrono_tz::Tz = "Europe/Berlin".parse().unwrap();
        let ics = vevent(
            "UID:s\r\nSUMMARY:Plenum\r\nDTSTART;TZID=Europe/Berlin:20261101T190000\r\n\
             RRULE:FREQ=WEEKLY;UNTIL=20261115T180000Z",
        );
        let events = parse_ics_in(&ics, &berlin);
        let start = NaiveDate::from_ymd_opt(2026, 11, 1)
            .unwrap()
            .and_hms_opt(19, 0, 0)
            .unwrap();
        assert_eq!(events[0].start, IcsTime::DateTime(start));
        // UNTIL 18:00Z is 19:00 in Berlin (winter): the 15th is the last one
        let days: Vec<NaiveDate> = expand(&events, start.date(), 30)
            .into_iter()
            .filter(|(_, e)| !e.is_empty())
            .map(|(d, _)| d)
            .collect();
        assert_eq!(days, [1, 8, 15].map(|d| date(2026, 11, d)));
        // the same in UTC: 18:00
        let utc = parse_ics_in(&ics, &Utc);
        assert_eq!(utc[0].start, IcsTime::DateTime(start - Duration::hours(1)));
    }

    #[test]
    fn times_are_converted_to_local_and_end_times_kept() {
        let ics =
            vevent("UID:u\r\nSUMMARY:Call\r\nDTSTART:20260925T170000Z\r\nDTEND:20260925T180000Z");
        let events = parse_ics(&ics);
        let expected = Utc
            .with_ymd_and_hms(2026, 9, 25, 17, 0, 0)
            .unwrap()
            .with_timezone(&Local)
            .naive_local();
        assert_eq!(events[0].start, IcsTime::DateTime(expected));

        let day = expand(&events, expected.date(), 1);
        match &day[0].1[0].time {
            EntryTime::Timed { start, end } => {
                assert_eq!(*start, expected.time());
                assert_eq!(*end, Some((expected + Duration::hours(1)).time()));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn alarms_do_not_overwrite_event_properties_and_duplicates_collapse() {
        let ics = "BEGIN:VCALENDAR\r\n\
            BEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Real\r\nDTSTART:20260925T100000\r\n\
            BEGIN:VALARM\r\nDESCRIPTION:Reminder\r\nSUMMARY:Alarm text\r\nEND:VALARM\r\nEND:VEVENT\r\n\
            BEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Real\r\nDTSTART:20260925T100000\r\nEND:VEVENT\r\n\
            END:VCALENDAR\r\n";
        let events = parse_ics(ics);
        assert_eq!(events[0].summary, "Real");

        let day = expand(&events, date(2026, 9, 25), 1);
        assert_eq!(day[0].1.len(), 1, "same UID and start from two calendars");
    }

    #[test]
    fn all_day_and_running_events_sort_before_timed_ones() {
        let ics = "BEGIN:VCALENDAR\r\n\
            BEGIN:VEVENT\r\nUID:1\r\nSUMMARY:Breakfast\r\nDTSTART:20260925T080000\r\nEND:VEVENT\r\n\
            BEGIN:VEVENT\r\nUID:2\r\nSUMMARY:Holiday\r\nDTSTART;VALUE=DATE:20260925\r\nEND:VEVENT\r\n\
            BEGIN:VEVENT\r\nUID:3\r\nSUMMARY:Plenum\r\nDTSTART:20260925T190000\r\nEND:VEVENT\r\n\
            END:VCALENDAR\r\n";
        let day = expand(&parse_ics(ics), date(2026, 9, 25), 1);
        let order: Vec<&str> = day[0].1.iter().map(|e| e.summary.as_str()).collect();
        assert_eq!(order, ["Holiday", "Breakfast", "Plenum"]);
    }

    fn berlin() -> chrono_tz::Tz {
        chrono_tz::Europe::Berlin
    }

    /// The local starts of `ics` read in `zone` over `days` days from `from`.
    fn starts_in<Z: TimeZone + Send + Sync + 'static>(
        ics: &str,
        zone: &Z,
        from: NaiveDate,
        days: i64,
    ) -> Vec<NaiveDateTime> {
        let mut out: Vec<NaiveDateTime> = expand(&parse_ics_in(ics, zone), from, days)
            .into_iter()
            .flat_map(|(_, entries)| entries)
            .map(|e| e.starts_at)
            .collect();
        out.dedup();
        out
    }

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        date(y, m, d).and_hms_opt(h, min, 0).unwrap()
    }

    #[test]
    fn a_utc_series_keeps_its_utc_hour_across_our_dst_change() {
        // 18:00Z is 20:00 in Berlin in summer time, 19:00 after 25 October.
        let ics = vevent(
            "UID:u\r\nSUMMARY:Call\r\nDTSTART:20261022T180000Z\r\nRRULE:FREQ=WEEKLY;COUNT=3",
        );
        assert_eq!(
            starts_in(&ics, &berlin(), date(2026, 10, 20), 21),
            [
                at(2026, 10, 22, 20, 0),
                at(2026, 10, 29, 19, 0),
                at(2026, 11, 5, 19, 0)
            ]
        );
    }

    #[test]
    fn a_series_in_another_zone_keeps_its_hour_there() {
        // New York changes a week after Berlin (1 November): 14:00 there is 20:00 here,
        // 19:00 in the week between, then 20:00 again.
        let ics = vevent(
            "UID:n\r\nSUMMARY:Call\r\nDTSTART;TZID=America/New_York:20261022T140000\r\n\
             RRULE:FREQ=WEEKLY;COUNT=3",
        );
        assert_eq!(
            starts_in(&ics, &berlin(), date(2026, 10, 20), 21),
            [
                at(2026, 10, 22, 20, 0),
                at(2026, 10, 29, 19, 0),
                at(2026, 11, 5, 20, 0)
            ]
        );
        // and a Berlin series read in UTC: 18:00, then 19:00
        let ics = vevent(
            "UID:b\r\nSUMMARY:Plenum\r\nDTSTART;TZID=Europe/Berlin:20261022T200000\r\n\
             RRULE:FREQ=WEEKLY;COUNT=2",
        );
        assert_eq!(
            starts_in(&ics, &Utc, date(2026, 10, 20), 14),
            [at(2026, 10, 22, 18, 0), at(2026, 10, 29, 19, 0)]
        );
    }

    #[test]
    fn times_in_the_dst_gap_move_on_and_in_the_overlap_take_the_first() {
        // 02:30 does not exist on 28 March 2027: 03:30 summer time (RFC 5545 §3.3.5).
        let gap = vevent(
            "UID:g\r\nSUMMARY:Early\r\nDTSTART;TZID=Europe/Berlin:20270321T023000\r\n\
             RRULE:FREQ=WEEKLY;COUNT=2",
        );
        assert_eq!(
            starts_in(&gap, &berlin(), date(2027, 3, 21), 14),
            [at(2027, 3, 21, 2, 30), at(2027, 3, 28, 3, 30)]
        );
        // 02:30 is there twice on 25 October 2026: the first (summer time), 00:30 UTC.
        let overlap = vevent(
            "UID:o\r\nSUMMARY:Early\r\nDTSTART;TZID=Europe/Berlin:20261018T023000\r\n\
             RRULE:FREQ=WEEKLY;COUNT=2",
        );
        assert_eq!(
            starts_in(&overlap, &Utc, date(2026, 10, 17), 14),
            [at(2026, 10, 18, 0, 30), at(2026, 10, 25, 0, 30)]
        );
    }

    #[test]
    fn windows_and_mozilla_zone_names_are_their_zones() {
        // Outlook: a Windows name with its VTIMEZONE; Thunderbird (older): a prefixed one.
        for tzid in [
            "W. Europe Standard Time",
            "/mozilla.org/20070129_1/Europe/Berlin",
        ] {
            let ics = format!(
                "BEGIN:VCALENDAR\r\nBEGIN:VTIMEZONE\r\nTZID:{tzid}\r\nEND:VTIMEZONE\r\n\
                 BEGIN:VEVENT\r\nUID:w\r\nSUMMARY:Team\r\nDTSTART;TZID=\"{tzid}\":20261022T200000\r\n\
                 RRULE:FREQ=WEEKLY;COUNT=2\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
            );
            assert_eq!(
                starts_in(&ics, &Utc, date(2026, 10, 20), 14),
                [at(2026, 10, 22, 18, 0), at(2026, 10, 29, 19, 0)],
                "{tzid}"
            );
        }
        // An unknown zone is floating: the wall clock wherever it is read.
        let ics = vevent("UID:x\r\nSUMMARY:S\r\nDTSTART;TZID=Mars/Olympus:20261022T200000");
        assert_eq!(
            starts_in(&ics, &Utc, date(2026, 10, 22), 1),
            [at(2026, 10, 22, 20, 0)]
        );
    }

    #[test]
    fn until_forms_clients_write() {
        // a floating UNTIL beside a TZID start (not RFC 5545, written anyway): in that zone
        let floating = vevent(
            "UID:f\r\nSUMMARY:S\r\nDTSTART;TZID=Europe/Berlin:20261022T200000\r\n\
             RRULE:FREQ=WEEKLY;UNTIL=20261105T200000",
        );
        assert_eq!(
            days_with(&parse_ics_in(&floating, &berlin()), date(2026, 10, 20), 30).len(),
            3
        );
        // a day UNTIL of a timed series: that day included
        let day = vevent(
            "UID:d\r\nSUMMARY:S\r\nDTSTART;TZID=Europe/Berlin:20261022T200000\r\n\
             RRULE:FREQ=WEEKLY;UNTIL=20261105",
        );
        assert_eq!(
            days_with(&parse_ics_in(&day, &berlin()), date(2026, 10, 20), 30).len(),
            3
        );
        // UNTIL before DTSTART: the first date only
        let before = vevent(
            "UID:b\r\nSUMMARY:S\r\nDTSTART;TZID=Europe/Berlin:20261022T200000\r\n\
             RRULE:FREQ=WEEKLY;UNTIL=20261001T000000Z",
        );
        assert_eq!(
            days_with(&parse_ics_in(&before, &berlin()), date(2026, 10, 1), 60),
            [date(2026, 10, 22)]
        );
    }

    #[test]
    fn a_nominal_day_keeps_the_wall_clock_across_dst() {
        // DURATION:P1D from 20:00 before the change ends at 20:00 the next day (25 hours),
        // PT24H at 19:00.
        let nominal = vevent(
            "UID:p\r\nSUMMARY:S\r\nDTSTART;TZID=Europe/Berlin:20261024T200000\r\nDURATION:P1D",
        );
        let exact = vevent(
            "UID:e\r\nSUMMARY:S\r\nDTSTART;TZID=Europe/Berlin:20261024T200000\r\nDURATION:PT24H",
        );
        let end = |ics: &str| match &expand(&parse_ics_in(ics, &berlin()), date(2026, 10, 24), 1)[0]
            .1[0]
            .time
        {
            EntryTime::Timed { end, .. } => *end,
            other => panic!("{other:?}"),
        };
        assert_eq!(end(&nominal), NaiveTime::from_hms_opt(20, 0, 0));
        assert_eq!(end(&exact), NaiveTime::from_hms_opt(19, 0, 0));
    }

    #[test]
    fn a_broken_line_costs_only_itself() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Kept\r\n\
            this is no property\r\nDTSTART:20261022T200000\r\nX-WR-ANYTHING;X-P=1:v\r\n\
            END:VEVENT\r\nEND:VCALENDAR\r\nBEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:b\r\n\
            SUMMARY:Second\r\nDTSTART:20261022T210000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_ics(ics);
        let names: Vec<&str> = events.iter().map(|e| e.summary.as_str()).collect();
        assert_eq!(names, ["Kept", "Second"]);
    }

    /// (start, end, key) of every occurrence, read in Berlin.
    fn occurrences_of(
        ics: &str,
        from: NaiveDate,
        days: i64,
    ) -> Vec<(NaiveDateTime, Option<NaiveTime>, String)> {
        let mut out: Vec<_> = expand(&parse_ics_in(ics, &berlin()), from, days)
            .into_iter()
            .flat_map(|(_, entries)| entries)
            .map(|e| {
                let end = match e.time {
                    EntryTime::Timed { end, .. } => end,
                    EntryTime::AllDay => None,
                };
                (e.starts_at, end, e.key)
            })
            .collect();
        out.dedup();
        out
    }

    #[test]
    fn rdates_add_times_periods_and_days() {
        // Two Tuesdays by the rule (COUNT=2 does not count RDATEs), a time, a period with its
        // own length, a day (all-day), one RDATE that is a rule date anyway.
        let ics = vevent(
            "UID:r\r\nSUMMARY:Café\r\nDTSTART;TZID=Europe/Berlin:20261006T190000\r\n\
             DTEND;TZID=Europe/Berlin:20261006T210000\r\nRRULE:FREQ=MONTHLY;COUNT=2;BYDAY=1TU\r\n\
             RDATE;TZID=Europe/Berlin:20261020T190000,20261103T190000\r\n\
             RDATE;VALUE=PERIOD:20261124T170000Z/PT3H\r\nRDATE;VALUE=DATE:20261128",
        );
        let t = |h, m| NaiveTime::from_hms_opt(h, m, 0);
        assert_eq!(
            occurrences_of(&ics, date(2026, 10, 1), 90),
            [
                (at(2026, 10, 6, 19, 0), t(21, 0), "r#2026-10-06".to_owned()),
                (at(2026, 10, 20, 19, 0), t(21, 0), "r#2026-10-20".to_owned()),
                (at(2026, 11, 3, 19, 0), t(21, 0), "r#2026-11-03".to_owned()),
                (at(2026, 11, 24, 18, 0), t(21, 0), "r#2026-11-24".to_owned()),
                (at(2026, 11, 28, 0, 0), None, "r#2026-11-28".to_owned()),
            ]
        );
        // an event with RDATEs alone is a series too (its keys carry the date)
        let only = vevent(
            "UID:o\r\nSUMMARY:Extra\r\nDTSTART;TZID=Europe/Berlin:20261006T190000\r\n\
             RDATE;TZID=Europe/Berlin:20261008T190000",
        );
        let keys: Vec<String> = occurrences_of(&only, date(2026, 10, 1), 30)
            .into_iter()
            .map(|o| o.2)
            .collect();
        assert_eq!(keys, ["o#2026-10-06", "o#2026-10-08"]);
    }

    #[test]
    fn exdate_and_replaced_occurrences_apply_to_rdates() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:r\r\nSUMMARY:Café\r\n\
            DTSTART;TZID=Europe/Berlin:20261006T190000\r\n\
            RDATE;TZID=Europe/Berlin:20261013T190000,20261020T190000\r\n\
            EXDATE;TZID=Europe/Berlin:20261013T190000\r\nEND:VEVENT\r\n\
            BEGIN:VEVENT\r\nUID:r\r\nSUMMARY:Café (yard)\r\n\
            RECURRENCE-ID;TZID=Europe/Berlin:20261020T190000\r\n\
            DTSTART;TZID=Europe/Berlin:20261020T180000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let starts: Vec<(NaiveDateTime, String)> = occurrences_of(ics, date(2026, 10, 1), 30)
            .into_iter()
            .map(|(s, _, k)| (s, k))
            .collect();
        assert_eq!(
            starts,
            [
                (at(2026, 10, 6, 19, 0), "r#2026-10-06".to_owned()),
                (at(2026, 10, 20, 18, 0), "r#2026-10-20".to_owned()),
            ]
        );
    }
}
