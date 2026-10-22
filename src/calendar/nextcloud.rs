//! A Nextcloud account's own calendars over CalDAV: make one with a display name, share it
//! (Nextcloud's `oc:share` POST, read back with `oc:invite`), read its events, and write single
//! events by UID. Moved here from plenum-bot (2026-10-22), which owns the plenums' calendars;
//! cleaning-bot keeps every resident's turns in a calendar of their own the same way.
//!
//! Events written here are named `<uid>.ics` in their calendar, so a listing
//! ([`Nextcloud::list_events`]) gives back each one's UID without reading its data.

use anyhow::{bail, Context, Result};
use chrono::NaiveDate;

use super::{caldav, ics::IcsEvent};

/// Someone a calendar is shared with: `principal:principals/groups/<gid>` or
/// `principal:principals/users/<uid>`, and whether they may edit it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Share {
    pub principal: String,
    pub write: bool,
}

/// The sharing principal of a Nextcloud group.
pub fn group(gid: &str) -> String {
    format!("principal:principals/groups/{gid}")
}

/// The sharing principal of a Nextcloud user.
pub fn user(uid: &str) -> String {
    format!("principal:principals/users/{uid}")
}

/// `(set, remove)`: shares to make or change, principals to take away.
pub fn plan_shares(current: &[Share], wanted: &[Share]) -> (Vec<Share>, Vec<String>) {
    let set = wanted
        .iter()
        .filter(|w| !current.contains(w))
        .cloned()
        .collect();
    let remove = current
        .iter()
        .filter(|c| !wanted.iter().any(|w| w.principal == c.principal))
        .map(|c| c.principal.clone())
        .collect();
    (set, remove)
}

/// The shares in a PROPFIND answer with `oc:invite` (Nextcloud: one `user` element per
/// sharee, `read-write` among its access when it may edit).
pub fn parse_shares(xml: &str) -> Vec<Share> {
    let mut out: Vec<Share> = caldav::xml_elements(xml, "invite")
        .into_iter()
        .flat_map(|invite| caldav::xml_elements(invite, "user"))
        .filter_map(|u| {
            let href = caldav::xml_elements(u, "href").first()?.trim().to_owned();
            let write = !caldav::xml_elements(u, "read-write").is_empty();
            Some(Share {
                principal: caldav::xml_unescape(&href),
                write,
            })
        })
        .collect();
    out.sort();
    out
}

/// One event of a calendar, as a listing has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventRef {
    /// The path the server gave (`/remote.php/dav/calendars/<user>/<slug>/<name>.ics`).
    pub href: String,
    /// The server's ETag, quotes included (as `If-Match` wants it).
    pub etag: String,
    /// The file name without `.ics`: the event's UID for every event written by
    /// [`Nextcloud::put_event`].
    pub uid: String,
}

/// When a [`Nextcloud::put_event`] may write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Precondition {
    /// Only if there is no such event yet (`If-None-Match: *`).
    Create,
    /// Only if the event is still this version (`If-Match`).
    Match(String),
    /// Whatever is there.
    Overwrite,
}

/// What a [`Nextcloud::put_event`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Written {
    /// Stored; the new ETag when the server sent one.
    Stored { etag: Option<String> },
    /// The precondition failed (412): the event exists (`Create`) or changed (`Match`).
    Conflict,
}

/// What a [`Nextcloud::delete_event`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deleted {
    Deleted,
    /// There was no such event (404): nothing to do.
    Missing,
    /// It changed since the ETag given (412): left as it is.
    Conflict,
}

/// One of the account's own calendars (not one shared with it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnCalendar {
    pub slug: String,
    pub name: Option<String>,
}

/// A Nextcloud account over CalDAV: `account` is its calendar home
/// (`https://cloud.example.org/remote.php/dav/calendars/plenum-bot/`).
pub struct Nextcloud {
    http: reqwest::Client,
    account: String,
    username: String,
    password: String,
}

fn method(name: &str) -> reqwest::Method {
    reqwest::Method::from_bytes(name.as_bytes()).expect("a valid method")
}

fn excerpt(body: &str) -> String {
    body.chars().take(200).collect()
}

/// A UID as a file name: letters, digits and `-_.@` as they are, everything else
/// percent-encoded (so a UID never leaves its calendar or clashes with another).
fn file_name(uid: &str) -> String {
    let mut out = String::with_capacity(uid.len() + 4);
    for b in uid.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out.push_str(".ics");
    out
}

/// `%XX` back to bytes (a file name from a listing).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Nextcloud {
    /// With its own HTTP client (30 s timeout).
    pub fn new(account: &str, username: &str, password: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("mxbot-common/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self::with_client(http, account, username, password))
    }

    /// With the bot's HTTP client (its user agent and timeouts).
    pub fn with_client(
        http: reqwest::Client,
        account: &str,
        username: &str,
        password: &str,
    ) -> Self {
        Self {
            http,
            account: format!("{}/", account.trim_end_matches('/')),
            username: username.to_owned(),
            password: password.to_owned(),
        }
    }

    /// The calendar home, with a trailing slash.
    pub fn account(&self) -> &str {
        &self.account
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn calendar_url(&self, slug: &str) -> String {
        format!("{}{slug}/", self.account)
    }

    /// The URL of the event `uid` in `slug`.
    pub fn event_url(&self, slug: &str, uid: &str) -> String {
        format!("{}{}", self.calendar_url(slug), file_name(uid))
    }

    async fn request(
        &self,
        m: &str,
        url: &str,
        headers: &[(&str, &str)],
        content_type: Option<&str>,
        body: String,
    ) -> Result<(u16, reqwest::header::HeaderMap, String)> {
        let mut req = self
            .http
            .request(method(m), url)
            .basic_auth(&self.username, Some(&self.password));
        if let Some(ct) = content_type {
            req = req.header("Content-Type", ct);
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req
            .body(body)
            .send()
            .await
            .with_context(|| format!("{m} {url}"))?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        Ok((status, headers, resp.text().await.unwrap_or_default()))
    }

    async fn dav(
        &self,
        m: &str,
        url: &str,
        depth: Option<&str>,
        body: String,
    ) -> Result<(u16, String)> {
        let depth: Vec<(&str, &str)> = depth.map(|d| ("Depth", d)).into_iter().collect();
        let (status, _, body) = self
            .request(m, url, &depth, Some("application/xml; charset=utf-8"), body)
            .await?;
        Ok((status, body))
    }

    /// The calendar `slug` with this display name; made when missing. True when it was made.
    pub async fn ensure_calendar(&self, slug: &str, name: &str) -> Result<bool> {
        let url = self.calendar_url(slug);
        let (status, body) = self
            .dav(
                "PROPFIND",
                &url,
                Some("0"),
                r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:displayname/></d:prop></d:propfind>"#.to_owned(),
            )
            .await?;
        match status {
            207 => {
                let current = caldav::xml_elements(&body, "displayname")
                    .first()
                    .map(|s| caldav::xml_unescape(s.trim()));
                if current.as_deref() != Some(name) {
                    let (status, _) = self
                        .dav(
                            "PROPPATCH",
                            &url,
                            None,
                            format!(r#"<?xml version="1.0" encoding="utf-8"?><d:propertyupdate xmlns:d="DAV:"><d:set><d:prop><d:displayname>{}</d:displayname></d:prop></d:set></d:propertyupdate>"#, caldav::xml_escape(name)),
                        )
                        .await?;
                    if status != 207 {
                        bail!("renaming the calendar {slug}: {status}");
                    }
                }
                Ok(false)
            }
            404 => {
                let (status, body) = self
                    .dav(
                        "MKCALENDAR",
                        &url,
                        None,
                        format!(r#"<?xml version="1.0" encoding="utf-8"?><c:mkcalendar xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:set><d:prop><d:displayname>{}</d:displayname><c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set></d:prop></d:set></c:mkcalendar>"#, caldav::xml_escape(name)),
                    )
                    .await?;
                if status != 201 {
                    bail!("making the calendar {slug}: {status} {}", excerpt(&body));
                }
                Ok(true)
            }
            other => bail!("the calendar {slug}: {other} {}", excerpt(&body)),
        }
    }

    /// The account's own calendars (calendars shared with it are left out).
    pub async fn calendars(&self) -> Result<Vec<OwnCalendar>> {
        let (status, body) = self
            .dav(
                "PROPFIND",
                &self.account,
                Some("1"),
                r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><d:resourcetype/><d:displayname/><oc:owner-principal/></d:prop></d:propfind>"#.to_owned(),
            )
            .await?;
        if status != 207 || caldav::xml_elements(&body, "multistatus").is_empty() {
            bail!(
                "the calendars of {}: {status} {}",
                self.username,
                excerpt(&body)
            );
        }
        let own = format!("principals/users/{}", self.username);
        let home = reqwest::Url::parse(&self.account)
            .map(|u| u.path().trim_end_matches('/').to_owned())
            .unwrap_or_default();
        let mut out = Vec::new();
        for block in caldav::xml_elements(&body, "response") {
            let is_calendar = caldav::xml_elements(block, "resourcetype")
                .first()
                .is_some_and(|t| !caldav::xml_elements(t, "calendar").is_empty());
            let Some(href) = caldav::xml_elements(block, "href")
                .first()
                .map(|h| caldav::xml_unescape(h.trim()))
            else {
                continue;
            };
            let path = reqwest::Url::parse(&href)
                .map(|u| u.path().to_owned())
                .unwrap_or(href);
            let path = path.trim_end_matches('/');
            if !is_calendar || path == home {
                continue;
            }
            // A calendar shared with the account: someone else's, named <slug>_shared_by_<owner>.
            let owner = caldav::xml_elements(block, "owner-principal")
                .first()
                .map(|o| caldav::xml_unescape(o.trim()))
                .filter(|o| !o.is_empty());
            if owner.is_some_and(|o| !o.trim_end_matches('/').ends_with(&own)) {
                continue;
            }
            let slug = percent_decode(path.rsplit('/').next().unwrap_or(path));
            if slug.contains("_shared_by_") {
                continue;
            }
            let name = caldav::xml_elements(block, "displayname")
                .first()
                .map(|n| caldav::xml_unescape(n.trim()))
                .filter(|n| !n.is_empty());
            out.push(OwnCalendar { slug, name });
        }
        Ok(out)
    }

    /// Delete the calendar `slug` (Nextcloud keeps it in the account's trash bin for a while).
    /// False when there was none.
    pub async fn delete_calendar(&self, slug: &str) -> Result<bool> {
        let (status, _, body) = self
            .request("DELETE", &self.calendar_url(slug), &[], None, String::new())
            .await?;
        match status {
            200 | 204 => Ok(true),
            404 => Ok(false),
            other => bail!("deleting the calendar {slug}: {other} {}", excerpt(&body)),
        }
    }

    pub async fn shares(&self, slug: &str) -> Result<Vec<Share>> {
        let (status, body) = self
            .dav(
                "PROPFIND",
                &self.calendar_url(slug),
                Some("0"),
                r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><oc:invite/></d:prop></d:propfind>"#.to_owned(),
            )
            .await?;
        if status != 207 {
            bail!("the shares of {slug}: {status}");
        }
        Ok(parse_shares(&body))
    }

    async fn share_request(&self, slug: &str, inner: String) -> Result<()> {
        let (status, body) = self
            .dav(
                "POST",
                &self.calendar_url(slug),
                None,
                format!(r#"<?xml version="1.0" encoding="utf-8"?><oc:share xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">{inner}</oc:share>"#),
            )
            .await?;
        if !(200..300).contains(&status) {
            bail!("sharing {slug}: {status} {}", excerpt(&body));
        }
        Ok(())
    }

    /// Share `slug` (Nextcloud answers 200 also for a principal that does not exist and
    /// shares nothing then: [`Nextcloud::shares`] tells).
    pub async fn share(&self, slug: &str, share: &Share) -> Result<()> {
        let write = if share.write { "<oc:read-write/>" } else { "" };
        self.share_request(
            slug,
            format!(
                "<oc:set><d:href>{}</d:href>{write}</oc:set>",
                caldav::xml_escape(&share.principal)
            ),
        )
        .await
    }

    pub async fn unshare(&self, slug: &str, principal: &str) -> Result<()> {
        self.share_request(
            slug,
            format!(
                "<oc:remove><d:href>{}</d:href></oc:remove>",
                caldav::xml_escape(principal)
            ),
        )
        .await
    }

    /// The calendar's events touching `from`..`to`, their times in `zone` (a failure is an
    /// error, never "none").
    pub async fn events(
        &self,
        slug: &str,
        from: NaiveDate,
        to: NaiveDate,
        zone: &chrono_tz::Tz,
    ) -> Result<Vec<IcsEvent>> {
        caldav::fetch_calendar(
            &self.http,
            &self.calendar_url(slug),
            &self.username,
            &self.password,
            from,
            to,
            zone,
        )
        .await
        .with_context(|| format!("reading the calendar {slug}"))
    }

    /// Every event of `slug` with its ETag (one PROPFIND, no event data): what is there to
    /// compare before writing.
    pub async fn list_events(&self, slug: &str) -> Result<Vec<EventRef>> {
        let url = self.calendar_url(slug);
        let (status, body) = self
            .dav(
                "PROPFIND",
                &url,
                Some("1"),
                r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:getetag/><d:resourcetype/></d:prop></d:propfind>"#.to_owned(),
            )
            .await?;
        if status != 207 || caldav::xml_elements(&body, "multistatus").is_empty() {
            bail!("the events of {slug}: {status} {}", excerpt(&body));
        }
        let mut out = Vec::new();
        for block in caldav::xml_elements(&body, "response") {
            let Some(href) = caldav::xml_elements(block, "href")
                .first()
                .map(|h| caldav::xml_unescape(h.trim()))
            else {
                continue;
            };
            let Some(name) = href.rsplit('/').next().and_then(|n| n.strip_suffix(".ics")) else {
                continue; // the collection itself
            };
            let etag = caldav::xml_elements(block, "getetag")
                .first()
                .map(|e| caldav::xml_unescape(e.trim()))
                .unwrap_or_default();
            out.push(EventRef {
                uid: percent_decode(name),
                href,
                etag,
            });
        }
        out.sort_by(|a, b| a.uid.cmp(&b.uid));
        Ok(out)
    }

    /// Write the event `uid` (`ics`: a whole VCALENDAR with that one UID) into `slug`, as
    /// `<uid>.ics`.
    pub async fn put_event(
        &self,
        slug: &str,
        uid: &str,
        ics: &str,
        when: &Precondition,
    ) -> Result<Written> {
        let condition: Vec<(&str, &str)> = match when {
            Precondition::Create => vec![("If-None-Match", "*")],
            Precondition::Match(etag) => vec![("If-Match", etag.as_str())],
            Precondition::Overwrite => vec![],
        };
        let (status, headers, body) = self
            .request(
                "PUT",
                &self.event_url(slug, uid),
                &condition,
                Some("text/calendar; charset=utf-8"),
                ics.to_owned(),
            )
            .await?;
        match status {
            200 | 201 | 204 => Ok(Written::Stored {
                etag: headers
                    .get("etag")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
            }),
            412 => Ok(Written::Conflict),
            other => bail!("writing {uid} into {slug}: {other} {}", excerpt(&body)),
        }
    }

    /// Delete the event `uid` from `slug`; with `etag` only if it is still that version. An
    /// event that is not there is no error.
    pub async fn delete_event(&self, slug: &str, uid: &str, etag: Option<&str>) -> Result<Deleted> {
        let condition: Vec<(&str, &str)> = etag.map(|e| ("If-Match", e)).into_iter().collect();
        let (status, _, body) = self
            .request(
                "DELETE",
                &self.event_url(slug, uid),
                &condition,
                None,
                String::new(),
            )
            .await?;
        match status {
            200 | 204 => Ok(Deleted::Deleted),
            404 | 410 => Ok(Deleted::Missing),
            412 => Ok(Deleted::Conflict),
            other => bail!("deleting {uid} from {slug}: {other} {}", excerpt(&body)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, header, header_exists, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn nc(server: &MockServer) -> Nextcloud {
        Nextcloud::new(
            &format!("{}/remote.php/dav/calendars/bot", server.uri()),
            "bot",
            "app-password",
        )
        .unwrap()
    }

    const HOME: &str = "/remote.php/dav/calendars/bot/";
    const AUTH: &str = "Basic Ym90OmFwcC1wYXNzd29yZA=="; // bot:app-password

    fn multistatus(inner: &str) -> ResponseTemplate {
        ResponseTemplate::new(207).set_body_string(format!(
            r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:cal="urn:ietf:params:xml:ns:caldav">{inner}</d:multistatus>"#
        ))
    }

    #[test]
    fn a_share_plan_changes_only_what_differs() {
        let w = |p: &str, write| Share {
            principal: p.to_owned(),
            write,
        };
        let current = [
            w("principal:principals/groups/finance", false),
            w("principal:principals/users/eve", true),
        ];
        let wanted = [w("principal:principals/groups/finance", true)];
        let (set, remove) = plan_shares(&current, &wanted);
        assert_eq!(set, vec![w("principal:principals/groups/finance", true)]);
        assert_eq!(remove, vec!["principal:principals/users/eve".to_owned()]);
        assert_eq!(plan_shares(&wanted, &wanted), (vec![], vec![]));
        assert_eq!(group("finance"), "principal:principals/groups/finance");
        assert_eq!(user("anna"), "principal:principals/users/anna");
    }

    #[test]
    fn shares_are_read_whatever_the_prefix() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:x1="http://owncloud.org/ns"><d:response><d:href>/remote.php/dav/calendars/plenum-bot/plenum-house/</d:href>
<d:propstat><d:prop><x1:invite>
<x1:user><d:href>principal:principals/groups/residents</d:href><x1:common-name>Bewohner</x1:common-name><x1:invite-accepted/><x1:access><x1:read-write/></x1:access></x1:user>
<x1:user><d:href>principal:principals/users/calendar-bot</d:href><x1:invite-accepted/><x1:access><x1:read/></x1:access></x1:user>
</x1:invite></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
        assert_eq!(
            parse_shares(xml),
            vec![
                Share {
                    principal: "principal:principals/groups/residents".into(),
                    write: true
                },
                Share {
                    principal: "principal:principals/users/calendar-bot".into(),
                    write: false
                },
            ]
        );
        assert!(parse_shares(
            "<d:multistatus xmlns:d=\"DAV:\"><oc:invite xmlns:oc=\"x\"/></d:multistatus>"
        )
        .is_empty());
    }

    #[test]
    fn a_uid_is_a_safe_file_name_and_comes_back() {
        assert_eq!(file_name("3f2a-b@example.org"), "3f2a-b@example.org.ics");
        assert_eq!(file_name("a/b c"), "a%2Fb%20c.ics");
        assert_eq!(percent_decode("a%2Fb%20c"), "a/b c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("x%zz"), "x%zz");
    }

    #[tokio::test]
    async fn a_missing_calendar_is_made_and_a_renamed_one_renamed() {
        let server = MockServer::start().await;
        Mock::given(method("PROPFIND"))
            .and(path(format!("{HOME}new/")))
            .and(header("authorization", AUTH))
            .and(header("depth", "0"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("MKCALENDAR"))
            .and(path(format!("{HOME}new/")))
            .and(body_string_contains(
                "<d:displayname>Tom &amp; Jerry</d:displayname>",
            ))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PROPFIND"))
            .and(path(format!("{HOME}old/")))
            .respond_with(multistatus(
                "<d:response><d:href>/x/</d:href><d:propstat><d:prop><d:displayname>Old</d:displayname></d:prop></d:propstat></d:response>",
            ))
            .mount(&server)
            .await;
        Mock::given(method("PROPPATCH"))
            .and(path(format!("{HOME}old/")))
            .and(body_string_contains("<d:displayname>New</d:displayname>"))
            .respond_with(multistatus(""))
            .expect(1)
            .mount(&server)
            .await;
        let nc = nc(&server);
        assert!(nc.ensure_calendar("new", "Tom & Jerry").await.unwrap());
        assert!(!nc.ensure_calendar("old", "New").await.unwrap());
        assert!(!nc.ensure_calendar("old", "Old").await.unwrap()); // no PROPPATCH
    }

    #[tokio::test]
    async fn shares_are_set_and_removed_with_the_oc_share_post() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("{HOME}cal/")))
            .and(body_string_contains(
                "<oc:set><d:href>principal:principals/users/anna</d:href></oc:set>",
            ))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{HOME}cal/")))
            .and(body_string_contains(
                "<oc:set><d:href>principal:principals/groups/residents</d:href><oc:read-write/></oc:set>",
            ))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{HOME}cal/")))
            .and(body_string_contains(
                "<oc:remove><d:href>principal:principals/users/eve</d:href></oc:remove>",
            ))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let nc = nc(&server);
        nc.share(
            "cal",
            &Share {
                principal: user("anna"),
                write: false,
            },
        )
        .await
        .unwrap();
        nc.share(
            "cal",
            &Share {
                principal: group("residents"),
                write: true,
            },
        )
        .await
        .unwrap();
        assert!(nc.unshare("cal", &user("eve")).await.is_err());
    }

    #[tokio::test]
    async fn events_are_listed_written_and_deleted_by_uid() {
        let server = MockServer::start().await;
        Mock::given(method("PROPFIND"))
            .and(path(format!("{HOME}cal/")))
            .and(header("depth", "1"))
            .respond_with(multistatus(&format!(
                "<d:response><d:href>{HOME}cal/</d:href><d:propstat><d:prop><d:getetag/><d:resourcetype><d:collection/><cal:calendar/></d:resourcetype></d:prop></d:propstat></d:response>\
                 <d:response><d:href>{HOME}cal/turn-2.ics</d:href><d:propstat><d:prop><d:getetag>&quot;e2&quot;</d:getetag></d:prop></d:propstat></d:response>\
                 <d:response><d:href>{HOME}cal/turn%201.ics</d:href><d:propstat><d:prop><d:getetag>&quot;e1&quot;</d:getetag></d:prop></d:propstat></d:response>"
            )))
            .mount(&server)
            .await;
        // a new event: only if it is not there yet
        Mock::given(method("PUT"))
            .and(path(format!("{HOME}cal/new.ics")))
            .and(header("if-none-match", "*"))
            .and(header("content-type", "text/calendar; charset=utf-8"))
            .respond_with(ResponseTemplate::new(201).insert_header("ETag", "\"n1\""))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(format!("{HOME}cal/turn-2.ics")))
            .and(header("if-none-match", "*"))
            .respond_with(ResponseTemplate::new(412))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(format!("{HOME}cal/turn-2.ics")))
            .and(header("if-match", "\"e2\""))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(format!("{HOME}cal/plain.ics")))
            .respond_with(move |req: &Request| {
                let conditional = req.headers.contains_key("if-match")
                    || req.headers.contains_key("if-none-match");
                ResponseTemplate::new(if conditional { 400 } else { 204 })
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path(format!("{HOME}cal/turn-2.ics")))
            .and(header_exists("if-match"))
            .respond_with(ResponseTemplate::new(412))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path(format!("{HOME}cal/turn%201.ics")))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path(format!("{HOME}cal/gone.ics")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let nc = nc(&server);
        let list = nc.list_events("cal").await.unwrap();
        assert_eq!(
            list,
            vec![
                EventRef {
                    href: format!("{HOME}cal/turn%201.ics"),
                    etag: "\"e1\"".into(),
                    uid: "turn 1".into()
                },
                EventRef {
                    href: format!("{HOME}cal/turn-2.ics"),
                    etag: "\"e2\"".into(),
                    uid: "turn-2".into()
                },
            ]
        );
        let ics = "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n";
        assert_eq!(
            nc.put_event("cal", "new", ics, &Precondition::Create)
                .await
                .unwrap(),
            Written::Stored {
                etag: Some("\"n1\"".into())
            }
        );
        assert_eq!(
            nc.put_event("cal", "turn-2", ics, &Precondition::Create)
                .await
                .unwrap(),
            Written::Conflict
        );
        assert_eq!(
            nc.put_event("cal", "turn-2", ics, &Precondition::Match("\"e2\"".into()))
                .await
                .unwrap(),
            Written::Stored { etag: None }
        );
        assert_eq!(
            nc.put_event("cal", "plain", ics, &Precondition::Overwrite)
                .await
                .unwrap(),
            Written::Stored { etag: None }
        );
        assert_eq!(
            nc.delete_event("cal", "turn-2", Some("\"old\""))
                .await
                .unwrap(),
            Deleted::Conflict
        );
        assert_eq!(
            nc.delete_event("cal", "turn 1", None).await.unwrap(),
            Deleted::Deleted
        );
        assert_eq!(
            nc.delete_event("cal", "gone", None).await.unwrap(),
            Deleted::Missing
        );
    }

    #[tokio::test]
    async fn only_the_accounts_own_calendars_are_listed_and_one_is_deleted() {
        let server = MockServer::start().await;
        let response = |href: &str, extra: &str| {
            format!("<d:response><d:href>{href}</d:href><d:propstat><d:prop>{extra}</d:prop></d:propstat></d:response>")
        };
        let calendar = "<d:resourcetype><d:collection/><cal:calendar/></d:resourcetype>";
        Mock::given(method("PROPFIND"))
            .and(path(HOME))
            .and(header("depth", "1"))
            .respond_with(multistatus(&[
                response(HOME, "<d:resourcetype><d:collection/></d:resourcetype>"),
                response(&format!("{HOME}cleaning-anna/"), &format!("{calendar}<d:displayname>Cleaning: Anna</d:displayname><oc:owner-principal>principals/users/bot</oc:owner-principal>")),
                response(&format!("{HOME}cleaning-ben/"), calendar),
                response(&format!("{HOME}cleaning-x_shared_by_eve/"), calendar),
                response(&format!("{HOME}theirs/"), &format!("{calendar}<oc:owner-principal>principals/users/eve</oc:owner-principal>")),
                response(&format!("{HOME}inbox/"), "<d:resourcetype><d:collection/><cal:schedule-inbox/></d:resourcetype>"),
            ].concat()))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path(format!("{HOME}cleaning-ben/")))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path(format!("{HOME}cleaning-gone/")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let nc = nc(&server);
        assert_eq!(
            nc.calendars().await.unwrap(),
            vec![
                OwnCalendar {
                    slug: "cleaning-anna".into(),
                    name: Some("Cleaning: Anna".into())
                },
                OwnCalendar {
                    slug: "cleaning-ben".into(),
                    name: None
                },
            ]
        );
        assert!(nc.delete_calendar("cleaning-ben").await.unwrap());
        assert!(!nc.delete_calendar("cleaning-gone").await.unwrap());
    }

    #[tokio::test]
    async fn a_login_page_is_no_listing() {
        let server = MockServer::start().await;
        Mock::given(method("PROPFIND"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>login</html>"))
            .mount(&server)
            .await;
        let nc = nc(&server);
        assert!(nc.list_events("cal").await.is_err());
        assert!(nc.calendars().await.is_err());
    }
}
