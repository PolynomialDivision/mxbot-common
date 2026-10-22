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
    /// Calendar slugs to skip when using caldav_account_url (e.g. ["contacts", "birthdays"]).
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// The events of one CalDAV collection between `start` and `end`. Unlike an account's
/// calendars, a failure is an error: the caller must not take "nothing" for "no events".
pub async fn fetch_calendar(
    http: &reqwest::Client,
    url: &str,
    username: &str,
    password: &str,
    start: NaiveDate,
    end: NaiveDate,
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
        .flat_map(|b| super::ics::parse_ics(b))
        .collect())
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

    let xml = xml_unescape(&req.send().await?.error_for_status()?.text().await?);

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

    let xml = req.send().await?.error_for_status()?.text().await?;

    // Extract href values from responses that contain <C:calendar/> resourcetype.
    // We split by <D:response> blocks and keep those that declare a calendar collection.
    let mut urls = Vec::new();
    let mut pos = 0;
    while let Some(rel) = xml[pos..]
        .find("<D:response>")
        .or_else(|| xml[pos..].find("<d:response>"))
    {
        let block_start = pos + rel;
        let close_tag = if xml[block_start..].starts_with("<D:") {
            "</D:response>"
        } else {
            "</d:response>"
        };
        let block_end = xml[block_start..]
            .find(close_tag)
            .map(|i| block_start + i + close_tag.len())
            .unwrap_or(xml.len());
        let block = &xml[block_start..block_end];
        pos = block_end;

        let is_calendar = block.contains(":calendar/>")
            || block.contains(":calendar />")
            || block.contains(":calendar>");
        if !is_calendar {
            continue;
        }

        // Extract the href from this response block.
        let href = block
            .find("<D:href>")
            .or_else(|| block.find("<d:href>"))
            .and_then(|s| {
                let after = &block[s..];
                let val_start = after.find('>').map(|i| i + 1)?;
                let val_end = after[val_start..].find('<').map(|i| val_start + i)?;
                Some(after[val_start..val_end].trim().to_owned())
            });

        let Some(href) = href else { continue };

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
                caldav_account_url: None,
                ics_url: None,
                username: src.username.clone(),
                password: src.password.clone(),
                exclude: vec![],
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
    fn caldav_xml_is_unescaped() {
        assert_eq!(
            xml_unescape("SUMMARY:Tom &amp; Jerry &lt;3&#13;\nLOCATION:&#x4B;üche"),
            "SUMMARY:Tom & Jerry <3\r\nLOCATION:Küche"
        );
        assert_eq!(xml_unescape("a & b; c"), "a & b; c");
    }
}
