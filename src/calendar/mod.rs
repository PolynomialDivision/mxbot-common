//! Calendars (feature `calendar`): iCalendar parsing and recurrence expansion ([`ics`]) and
//! fetching from ICS URLs and CalDAV ([`caldav`]), shared by calendar-bot and plenum-bot.

pub mod caldav;
pub mod ics;
