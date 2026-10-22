//! Calendars (feature `calendar`): iCalendar parsing and recurrence expansion ([`ics`]),
//! fetching from ICS URLs and CalDAV ([`caldav`]), shared by calendar-bot and plenum-bot, and a
//! Nextcloud account's own calendars ([`nextcloud`]: make, share, write events), for
//! plenum-bot and cleaning-bot.

pub mod caldav;
pub mod ics;
pub mod nextcloud;
