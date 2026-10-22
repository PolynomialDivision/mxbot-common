//! Fetching calendars: a plain ICS URL, one CalDAV collection (REPORT with a time range), or
//! every calendar of a CalDAV account (PROPFIND, then each). Moved here from calendar-bot
//! (2026-10-22); `fetch_calendar` is the strict read of one collection plenum-bot uses.

use anyhow::{Context, Result};
use chrono::NaiveDate;
use serde::Deserialize;
use tracing::{info, warn};

/// One `[[calendar.sources]]` entry (calendar-bot's config).
#[derive(Deserialize, Clone, Debug, Default)]
pub struct Source {
    /// Direct ICS URL (GET request).
    pub ics_url: Option<String>,
    /// CalDAV collection URL (REPORT request, server-side date filtering, handles recurrence).
    pub caldav_url: Option<String>,
    /// CalDAV account root URL: discovers and fetches all calendars automatically.
    pub caldav_account_url: Option<String>,
    /// HTTP Basic Auth username (optional).
    pub username: Option<String>,
    /// HTTP Basic Auth password (optional).
    pub password: Option<String>,
    /// The environment variable with the password, instead of `password` (an app password
    /// the deployment writes next to the bot, never into its config). Read by
    /// [`Source::resolve_password_env`].
    #[serde(default)]
    pub password_env: Option<String>,
    /// Calendar slugs to skip when using caldav_account_url (e.g. ["contacts", "birthdays"]).
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl Source {
    /// `password` from the variable `password_env` names, when there is no `password`. A
    /// variable that is missing or empty is an error: the bot must not run without it.
    pub fn resolve_password_env(&mut self) -> Result<()> {
        if self.password.is_some() {
            return Ok(());
        }
        let Some(var) = &self.password_env else {
            return Ok(());
        };
        let value = std::env::var(var)
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .with_context(|| format!("password_env: {var} is not set"))?;
        self.password = Some(value);
        Ok(())
    }
}

/// The events of one CalDAV collection between `start` and `end`, their times in `zone`.
/// Unlike an account's calendars, a failure is an error: the caller must not take "nothing"
/// for "no events".
pub async fn fetch_calendar(
    http: &reqwest::Client,
    url: &str,
    username: &str,
    password: &str,
    start: NaiveDate,
    end: NaiveDate,
    zone: &chrono_tz::Tz,
) -> Result<Vec<super::ics::IcsEvent>> {
    let src = Source {
        caldav_url: Some(url.to_owned()),
        username: Some(username.to_owned()),
        password: Some(password.to_owned()),
        ..Default::default()
    };
    let blocks = fetch_ics_caldav(http, &src, start, end).await?;
    Ok(blocks
        .iter()
        .flat_map(|b| super::ics::parse_ics_in(b, zone))
        .collect())
}

/// The text inside every element with the local name `name`, whatever namespace prefix the
/// server chose (`<d:href>`, `<D:href>`, `<href xmlns="DAV:">`); an empty element is "".
pub fn xml_elements<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(open) = find_tag(rest, name) {
        let after = &rest[open..];
        let Some(gt) = after.find('>') else {
            break;
        };
        if after[..gt].ends_with('/') {
            out.push("");
            rest = &after[gt + 1..];
            continue;
        }
        let body = &after[gt + 1..];
        let Some(close) = find_close(body, name) else {
            break;
        };
        out.push(&body[..close]);
        rest = &body[close..];
    }
    out
}

/// The position of the next `<name` or `<prefix:name` start tag.
fn find_tag(xml: &str, name: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = xml[from..].find('<') {
        let at = from + i;
        let tag = &xml[at + 1..];
        let local = tag
            .split(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .next()
            .unwrap_or("");
        let local = local.rsplit(':').next().unwrap_or(local);
        if !tag.starts_with('/') && local == name {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

fn find_close(xml: &str, name: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = xml[from..].find("</") {
        let at = from + i;
        let tag = &xml[at + 2..];
        let local = tag.split('>').next().unwrap_or("").trim();
        if local.rsplit(':').next() == Some(name) {
            return Some(at);
        }
        from = at + 2;
    }
    None
}

/// Escape text for an XML element.
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A WebDAV multistatus body, or an error: a login page or a proxy's answer with status 200
/// must never read as "no events".
fn multistatus(what: &str, status: reqwest::StatusCode, body: &str) -> Result<()> {
    if status.as_u16() != 207 || find_tag(body, "multistatus").is_none() {
        anyhow::bail!(
            "{what}: no WebDAV multistatus (HTTP {status}): {}",
            body.chars().take(200).collect::<String>()
        );
    }
    Ok(())
}

fn add_auth(req: reqwest::RequestBuilder, src: &Source) -> reqwest::RequestBuilder {
    if let (Some(u), Some(p)) = (&src.username, &src.password) {
        req.basic_auth(u, Some(p))
    } else {
        req
    }
}

pub async fn fetch_ics_direct(http: &reqwest::Client, src: &Source) -> Result<String> {
    let url = src.ics_url.as_deref().context("No ics_url configured")?;
    let req = add_auth(http.get(url), src);
    Ok(req.send().await?.error_for_status()?.text().await?)
}

/// Undo XML escaping of `calendar-data` inside a CalDAV response, so "&amp;"
/// in a title reads "&" (and "&#13;" line endings become real ones).
pub fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest.find(';').filter(|&end| end <= 10).and_then(|end| {
            let entity = &rest[1..end];
            let ch = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => entity
                    .strip_prefix("#x")
                    .or_else(|| entity.strip_prefix("#X"))
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            };
            ch.map(|c| (c, end))
        });
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

pub async fn fetch_ics_caldav(
    http: &reqwest::Client,
    src: &Source,
    start: NaiveDate,
    end: NaiveDate,
) -> Result<Vec<String>> {
    let url = src
        .caldav_url
        .as_deref()
        .context("No caldav_url configured")?;
    let start = start.format("%Y%m%dT000000Z").to_string();
    let end = end.format("%Y%m%dT000000Z").to_string();

    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8" ?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><C:calendar-data/></D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VEVENT">
        <C:time-range start="{start}" end="{end}"/>
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#
    );

    let method = reqwest::Method::from_bytes(b"REPORT").expect("valid method");
    let req = add_auth(
        http.request(method, url)
            .header("Content-Type", "application/xml; charset=utf-8")
            .header("Depth", "1")
            .body(body),
        src,
    );

    let resp = req.send().await?.error_for_status()?;
    let status = resp.status();
    let body = resp.text().await?;
    multistatus("the calendar query", status, &body)?;
    let xml = xml_unescape(&body);

    let mut blocks = Vec::new();
    let mut pos = 0;
    while let Some(rel) = xml[pos..].find("BEGIN:VCALENDAR") {
        let start_idx = pos + rel;
        if let Some(rel_end) = xml[start_idx..].find("END:VCALENDAR") {
            let end_idx = start_idx + rel_end + "END:VCALENDAR".len();
            blocks.push(xml[start_idx..end_idx].to_owned());
            pos = end_idx;
        } else {
            break;
        }
    }

    Ok(blocks)
}

/// PROPFIND the account root and return all calendar collection URLs found.
pub async fn discover_calendars(http: &reqwest::Client, src: &Source) -> Result<Vec<String>> {
    let account_url = src
        .caldav_account_url
        .as_deref()
        .context("No caldav_account_url")?;

    let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:resourcetype/>
    <D:displayname/>
  </D:prop>
</D:propfind>"#;

    let method = reqwest::Method::from_bytes(b"PROPFIND").expect("valid method");
    let req = add_auth(
        http.request(method, account_url)
            .header("Content-Type", "application/xml; charset=utf-8")
            .header("Depth", "1")
            .body(body),
        src,
    );

    let resp = req.send().await?.error_for_status()?;
    let status = resp.status();
    let xml = resp.text().await?;
    multistatus("the calendar list", status, &xml)?;

    // The responses whose resource type is a calendar collection.
    let mut urls = Vec::new();
    for block in xml_elements(&xml, "response") {
        let is_calendar = xml_elements(block, "resourcetype")
            .first()
            .is_some_and(|t| !xml_elements(t, "calendar").is_empty());
        if !is_calendar {
            continue;
        }
        let Some(href) = xml_elements(block, "href")
            .first()
            .map(|h| xml_unescape(h.trim()))
        else {
            continue;
        };

        // Skip the account root itself (returned as first entry).
        let account_path = reqwest::Url::parse(account_url)
            .ok()
            .map(|u| u.path().trim_end_matches('/').to_owned())
            .unwrap_or_default();
        let href_path = href.trim_end_matches('/');
        if href_path == account_path {
            continue;
        }

        // Check exclude list against the last path segment (calendar slug).
        let slug = href_path.rsplit('/').next().unwrap_or(href_path);
        if src.exclude.iter().any(|ex| ex == slug) {
            info!("Skipping excluded calendar: {slug}");
            continue;
        }

        // Build absolute URL if href is a path.
        let absolute = if href.starts_with("http://") || href.starts_with("https://") {
            href
        } else {
            let base = reqwest::Url::parse(account_url)?;
            base.join(&href)?.to_string()
        };

        urls.push(absolute);
    }

    Ok(urls)
}

/// Fetch raw ICS events from a single source for a date range (no day filtering).
pub async fn fetch_source_ics_events(
    http: &reqwest::Client,
    src: &Source,
    start: NaiveDate,
    end: NaiveDate,
) -> Result<Vec<super::ics::IcsEvent>> {
    if src.caldav_account_url.is_some() {
        let calendar_urls = discover_calendars(http, src).await?;
        info!(
            "Discovered {} calendar(s) from account",
            calendar_urls.len()
        );
        let mut all: Vec<super::ics::IcsEvent> = Vec::new();
        for url in &calendar_urls {
            let cal_src = Source {
                caldav_url: Some(url.clone()),
                username: src.username.clone(),
                password: src.password.clone(),
                ..Default::default()
            };
            match fetch_ics_caldav(http, &cal_src, start, end).await {
                Ok(blocks) => blocks
                    .iter()
                    .for_each(|b| all.extend(super::ics::parse_ics(b))),
                Err(e) => warn!("Failed to fetch calendar {url}: {e}"),
            }
        }
        Ok(all)
    } else if src.caldav_url.is_some() {
        let blocks = fetch_ics_caldav(http, src, start, end).await?;
        Ok(blocks
            .iter()
            .flat_map(|b| super::ics::parse_ics(b))
            .collect())
    } else {
        let ics = fetch_ics_direct(http, src).await?;
        Ok(super::ics::parse_ics(&ics))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elements_by_local_name_whatever_the_prefix() {
        let xml = r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/a/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/><cal:calendar xmlns:cal="urn:ietf:params:xml:ns:caldav"/></d:resourcetype></d:prop></d:propstat></d:response><D:response><D:href>/b/</D:href><D:resourcetype/></D:response><response><href>/c/</href></response></d:multistatus>"#;
        let responses = xml_elements(xml, "response");
        assert_eq!(responses.len(), 3);
        assert_eq!(xml_elements(responses[0], "href"), ["/a/"]);
        assert_eq!(xml_elements(responses[0], "calendar"), [""]);
        assert!(xml_elements(responses[0], "calendar-data").is_empty());
        assert_eq!(xml_elements(responses[1], "resourcetype"), [""]);
        assert_eq!(xml_elements(responses[2], "href"), ["/c/"]);
    }

    #[test]
    fn only_a_multistatus_is_an_answer() {
        let ok = reqwest::StatusCode::from_u16(207).unwrap();
        let fine = reqwest::StatusCode::OK;
        assert!(multistatus("q", ok, "<d:multistatus xmlns:d='DAV:'/>").is_ok());
        assert!(multistatus("q", fine, "<d:multistatus xmlns:d='DAV:'/>").is_err());
        assert!(multistatus("q", ok, "<html>login</html>").is_err());
    }

    #[test]
    fn the_password_can_come_from_the_environment() {
        let mut src: Source = toml::from_str(
            "caldav_account_url = 'https://cloud.example.org/remote.php/dav/calendars/bot/'\n\
             username = 'bot'\npassword_env = 'MXBOT_TEST_CALDAV_PASSWORD'",
        )
        .unwrap();
        assert!(src.resolve_password_env().is_err(), "unset is an error");
        std::env::set_var("MXBOT_TEST_CALDAV_PASSWORD", " app-password\n");
        src.resolve_password_env().unwrap();
        assert_eq!(src.password.as_deref(), Some("app-password"));
        // a password in the config wins; no variable, nothing to do
        let mut fixed = Source {
            password: Some("fixed".into()),
            password_env: Some("MXBOT_TEST_CALDAV_UNSET".into()),
            ..Default::default()
        };
        fixed.resolve_password_env().unwrap();
        assert_eq!(fixed.password.as_deref(), Some("fixed"));
        let mut none = Source::default();
        none.resolve_password_env().unwrap();
        assert_eq!(none.password, None);
    }

    #[test]
    fn caldav_xml_is_unescaped() {
        assert_eq!(
            xml_unescape("SUMMARY:Tom &amp; Jerry &lt;3&#13;\nLOCATION:&#x4B;üche"),
            "SUMMARY:Tom & Jerry <3\r\nLOCATION:Küche"
        );
        assert_eq!(xml_unescape("a & b; c"), "a & b; c");
    }
}
