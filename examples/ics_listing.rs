//! Every occurrence of an .ics file as mxbot-common expands it, one "local start|title" line
//! each, sorted: the counterpart of tools/ics-oracle/oracle.py for comparing.
//!
//! cargo run --features calendar --example ics_listing -- <file.ics> <from YYYY-MM-DD> <days> [zone]

use std::collections::BTreeSet;

use chrono::NaiveDate;
use mxbot_common::calendar::ics::{expand, parse_ics_in, EntryTime};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, path, from, days, rest @ ..] = args.as_slice() else {
        eprintln!(
            "usage: ics_listing <file.ics> <from YYYY-MM-DD> <days> [zone, default Europe/Berlin]"
        );
        std::process::exit(2);
    };
    let ics = std::fs::read_to_string(path).expect("readable file");
    let from = NaiveDate::parse_from_str(from, "%Y-%m-%d").expect("from as YYYY-MM-DD");
    let days: i64 = days.parse().expect("days as a number");
    let zone: chrono_tz::Tz = rest
        .first()
        .map_or("Europe/Berlin", String::as_str)
        .parse()
        .expect("an IANA zone");
    let mut lines = BTreeSet::new();
    for (_, entries) in expand(&parse_ics_in(&ics, &zone), from, days) {
        for e in entries.into_iter().filter(|e| e.first_day >= from) {
            let when = match e.time {
                EntryTime::AllDay => format!("{} all-day", e.first_day),
                EntryTime::Timed { .. } => e.starts_at.format("%Y-%m-%d %H:%M").to_string(),
            };
            lines.insert(format!("{when}|{}", e.summary));
        }
    }
    for line in lines {
        println!("{line}");
    }
}
