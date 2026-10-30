//! Wall-clock time in the house's zone, with one rule for the DST changes.
//!
//! A bot plans in local time ("07:00") but sleeps and compares in UTC instants: a sleep
//! computed from two naive local times is an hour off across a DST change. [`Zone::at`] is
//! the one place a local time becomes an instant:
//!
//! * a time in the spring gap (02:30 on the last Sunday of March in Berlin) moves to the
//!   first valid minute after it (03:00);
//! * an ambiguous autumn time (02:30 on the last Sunday of October) is the earlier of its
//!   two instants (still summer time).
//!
//! Never `chrono::Local`: it reads `/etc/localtime`, which containers often keep at UTC even
//! when `TZ` is set.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike, Utc};
use serde::{Deserialize, Deserializer};

pub use chrono_tz::Tz;

use crate::config::TimeOfDay;

/// A bot's time zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Zone(pub Tz);

impl Default for Zone {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Zone {
    /// Without a configured zone and `TZ`: the house is in Berlin.
    pub const DEFAULT: Zone = Zone(chrono_tz::Europe::Berlin);

    /// `configured` (a `timezone` key of the bot's config), else the `TZ` environment
    /// variable (an IANA name, as compose sets it), else [`Zone::DEFAULT`]. A name that is no
    /// known zone is an error, never a silent fallback.
    pub fn resolve(configured: Option<&str>) -> Result<Self> {
        Self::resolve_from(configured, std::env::var("TZ").ok().as_deref())
    }

    /// [`Zone::resolve`] with the value of `TZ` given (for tests).
    pub fn resolve_from(configured: Option<&str>, tz_env: Option<&str>) -> Result<Self> {
        if let Some(name) = configured.map(str::trim).filter(|n| !n.is_empty()) {
            return name
                .parse()
                .map(Zone)
                .map_err(|_| anyhow!("timezone {name:?} is not an IANA time zone"));
        }
        // POSIX allows a leading ':' ("TZ=:Europe/Berlin").
        match tz_env.map(|v| v.trim().trim_start_matches(':')) {
            None | Some("") => Ok(Self::DEFAULT),
            Some(name) => name.parse().map(Zone).map_err(|_| {
                anyhow!("TZ={name:?} is not an IANA time zone; set the bot's timezone instead")
            }),
        }
    }

    /// The current time in this zone.
    pub fn now(&self) -> DateTime<Tz> {
        Utc::now().with_timezone(&self.0)
    }

    /// The wall-clock time of `at` in this zone.
    pub fn local(&self, at: DateTime<Utc>) -> NaiveDateTime {
        at.with_timezone(&self.0).naive_local()
    }

    /// The instant of the wall-clock time `local`: in the spring gap the first valid minute
    /// after it, in the autumn overlap the earlier instant.
    pub fn at(&self, local: NaiveDateTime) -> DateTime<Utc> {
        if let Some(t) = self.0.from_local_datetime(&local).earliest() {
            return t.to_utc();
        }
        let minute = local.with_second(0).and_then(|t| t.with_nanosecond(0));
        let start = minute.unwrap_or(local);
        // A gap is an hour (at most a few in history); a day bounds the search.
        (1..=24 * 60)
            .find_map(|m| {
                self.0
                    .from_local_datetime(&(start + Duration::minutes(m)))
                    .earliest()
            })
            .map(|t| t.to_utc())
            .unwrap_or_else(|| Utc.from_utc_datetime(&local))
    }

    /// The instant of `time` on `date` (see [`Zone::at`]).
    pub fn at_time(&self, date: NaiveDate, time: TimeOfDay) -> DateTime<Utc> {
        self.at(date.and_time(naive_time(time)))
    }

    /// The first instant strictly after `after` at which it is `time` here (or the first
    /// valid minute after `time`, on the day it falls into the spring gap).
    pub fn next(&self, time: TimeOfDay, after: DateTime<Utc>) -> DateTime<Utc> {
        let today = self.local(after).date();
        // `at` never goes backwards in local time, so the first candidate that is later wins.
        (-1..=2)
            .map(|days| self.at_time(today + Duration::days(days), time))
            .find(|t| *t > after)
            .expect("a wall-clock time occurs within two days")
    }
}

impl<'de> Deserialize<'de> for Zone {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        name.trim()
            .parse()
            .map(Zone)
            .map_err(|_| serde::de::Error::custom(format!("{name:?} is not an IANA time zone")))
    }
}

/// `time` as a chrono time of day.
pub fn naive_time(time: TimeOfDay) -> NaiveTime {
    NaiveTime::from_hms_opt(time.hour, time.minute, 0).expect("TimeOfDay is validated")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BERLIN: Zone = Zone::DEFAULT;

    fn local(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn the_zone_comes_from_the_config_then_tz_then_berlin() {
        let zone = |c, e| Zone::resolve_from(c, e).map(|z| z.0);
        assert_eq!(
            zone(Some("Europe/Lisbon"), Some("Asia/Tokyo")).unwrap(),
            Tz::Europe__Lisbon
        );
        assert_eq!(zone(None, Some("Asia/Tokyo")).unwrap(), Tz::Asia__Tokyo);
        assert_eq!(zone(None, Some(":Asia/Tokyo")).unwrap(), Tz::Asia__Tokyo);
        assert_eq!(zone(None, Some("")).unwrap(), Tz::Europe__Berlin);
        assert_eq!(zone(None, None).unwrap(), Tz::Europe__Berlin);
        assert!(zone(Some("Mars/Base"), None).is_err());
        assert!(zone(None, Some("Mars/Base")).is_err());
        let parsed: Zone = serde_json::from_str("\"Europe/Berlin\"").unwrap();
        assert_eq!(parsed, BERLIN);
        assert!(serde_json::from_str::<Zone>("\"nowhere\"").is_err());
    }

    #[test]
    fn a_time_in_the_spring_gap_moves_to_the_first_valid_minute() {
        // 2026-03-29: 02:00 CET jumps to 03:00 CEST.
        assert_eq!(
            BERLIN.at(local(2026, 3, 29, 1, 59)),
            utc("2026-03-29T00:59:00Z")
        );
        assert_eq!(
            BERLIN.at(local(2026, 3, 29, 2, 0)),
            utc("2026-03-29T01:00:00Z")
        );
        assert_eq!(
            BERLIN.at(local(2026, 3, 29, 2, 30)),
            utc("2026-03-29T01:00:00Z")
        );
        assert_eq!(
            BERLIN.at(local(2026, 3, 29, 3, 0)),
            utc("2026-03-29T01:00:00Z")
        );
        assert_eq!(
            BERLIN.at(local(2026, 3, 29, 7, 0)),
            utc("2026-03-29T05:00:00Z")
        );
    }

    #[test]
    fn an_ambiguous_autumn_time_is_the_earlier_instant() {
        // 2026-10-25: 03:00 CEST goes back to 02:00 CET; 02:00-02:59 happen twice.
        assert_eq!(
            BERLIN.at(local(2026, 10, 25, 2, 30)),
            utc("2026-10-25T00:30:00Z")
        );
        assert_eq!(
            BERLIN.at(local(2026, 10, 25, 3, 0)),
            utc("2026-10-25T02:00:00Z")
        );
        assert_eq!(
            BERLIN.at(local(2026, 10, 25, 7, 0)),
            utc("2026-10-25T06:00:00Z")
        );
    }

    #[test]
    fn the_next_occurrence_is_an_instant_across_both_changes() {
        let seven = TimeOfDay::new(7, 0);
        // Saturday 23:00 CET before the spring change: 07:00 CEST is 7 hours away, not 8.
        let before_spring = utc("2026-03-28T22:00:00Z");
        assert_eq!(
            BERLIN.next(seven, before_spring),
            utc("2026-03-29T05:00:00Z")
        );
        // Saturday midnight CEST before the autumn change: 07:00 CET is 8 hours away, not 7.
        let before_autumn = utc("2026-10-24T22:00:00Z");
        assert_eq!(
            BERLIN.next(seven, before_autumn),
            utc("2026-10-25T06:00:00Z")
        );
        // Strictly after: at 07:00 itself the next one is tomorrow's.
        assert_eq!(
            BERLIN.next(seven, utc("2026-10-25T06:00:00Z")),
            utc("2026-10-26T06:00:00Z")
        );
        // In the gap: 02:30 on the change day is 03:00 CEST, the day after at 02:30 again.
        let half_two = TimeOfDay::new(2, 30);
        assert_eq!(
            BERLIN.next(half_two, before_spring),
            utc("2026-03-29T01:00:00Z")
        );
        assert_eq!(
            BERLIN.next(half_two, utc("2026-03-29T01:00:00Z")),
            utc("2026-03-30T00:30:00Z")
        );
        // In the overlap: once, at the first of the two 02:30s.
        assert_eq!(
            BERLIN.next(half_two, before_autumn),
            utc("2026-10-25T00:30:00Z")
        );
        assert_eq!(
            BERLIN.next(half_two, utc("2026-10-25T00:30:00Z")),
            utc("2026-10-26T01:30:00Z")
        );
    }

    #[test]
    fn local_is_the_wall_clock() {
        assert_eq!(
            BERLIN.local(utc("2026-10-25T00:30:00Z")),
            local(2026, 10, 25, 2, 30)
        );
        assert_eq!(
            BERLIN.local(utc("2026-10-25T01:30:00Z")),
            local(2026, 10, 25, 2, 30)
        );
    }
}
