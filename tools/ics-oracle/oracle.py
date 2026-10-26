"""Every occurrence of an .ics file as recurring-ical-events expands it: an independent
implementation (Python, icalendar) to compare mxbot-common's with (compare.sh).

usage: oracle.py <file.ics> <from YYYY-MM-DD> <days> [zone, default Europe/Berlin]
prints one "<local start>|<summary>" line per occurrence, sorted, like
examples/ics_listing.rs; all-day occurrences as "<date> all-day|<summary>".
"""
import datetime as dt
import sys
import zoneinfo

import icalendar
import recurring_ical_events

path, start, days = sys.argv[1], dt.date.fromisoformat(sys.argv[2]), int(sys.argv[3])
zone = zoneinfo.ZoneInfo(sys.argv[4] if len(sys.argv) > 4 else "Europe/Berlin")
with open(path, "rb") as f:
    # several VCALENDARs, as a CalDAV query returns them (one per event and its changes)
    cals = icalendar.Calendar.from_ical(f.read(), multiple=True)
end = start + dt.timedelta(days=days)
lines = set()
events = [ev for cal in cals for ev in recurring_ical_events.of(cal).between(start, end)]
for ev in events:
    # recurring-ical-events returns cancelled occurrences too; calendars do not show them
    if str(ev.get("STATUS", "")).upper() == "CANCELLED":
        continue
    s = ev["DTSTART"].dt
    summary = str(ev.get("SUMMARY", ""))
    if isinstance(s, dt.datetime):
        if s.tzinfo is not None:
            s = s.astimezone(zone).replace(tzinfo=None)
        if start <= s.date() < end:
            lines.add(f"{s:%Y-%m-%d %H:%M}|{summary}")
    elif start <= s < end:
        lines.add(f"{s:%Y-%m-%d} all-day|{summary}")
if lines:
    print("\n".join(sorted(lines)))
