//! Who asks a bot's website, and in which language ([`lang`]).
//!
//! The house's reverse proxy asks the identity provider before a request (forward auth) and
//! passes the person on as the neutral X-House-* headers, after dropping a client's own
//! copies. On a shared Docker network other containers could send those headers too, so a
//! request must also carry the proxy's secret in the bot's own header
//! (X-Cleaning-Proxy-Secret, …), which only the proxy knows: [`ProxySecret`], required, at
//! least 32 bytes, compared in constant time.
//!
//! [`ForwardAuth::identify`] turns a request's headers into a [`Visitor`]: a guest (no login,
//! where the website lets guests in) or a [`WebPerson`] with their login, name, e-mail,
//! groups and Matrix accounts. Groups fail closed like [`AllowList`]: an empty list lets
//! nobody in, `"all"` (the string, not a list containing it) everybody with a login, and members of `admin_groups` always get in.
//!
//! A person's house account is [`house_account`] of their login. The headers carry no
//! `matrix_localpart` (which the directory honours for people renamed at the login), so on
//! a website a renamed person is their new login's account: a known gap until the proxy
//! passes the house account itself. Logins that are no plain localpart have no house
//! account here (the directory's `UserId::parse` takes some of them); each website decides
//! what such a login is.
//!
//! Framework-free: everything takes an `http::HeaderMap` (axum's is the same type).

use std::fmt;

use anyhow::{bail, Context, Result};
use http::{HeaderMap, HeaderName};
use matrix_sdk::ruma::{OwnedUserId, UserId};
use subtle::ConstantTimeEq;
use tracing::warn;

use crate::config::AllowList;

pub mod lang;

pub const USER_HEADER: &str = "x-house-user";
pub const GROUPS_HEADER: &str = "x-house-groups";
pub const NAME_HEADER: &str = "x-house-name";
pub const EMAIL_HEADER: &str = "x-house-email";
/// A person's other Matrix accounts, comma-separated (set by an admin in the directory).
pub const MATRIX_IDS_HEADER: &str = "x-house-matrix-ids";

/// A person's accounts on a website at most: the house account and ten others.
pub const MAX_ACCOUNTS: usize = 11;

// ── The proxy's secret ──────────────────────────────────────────────────────

/// The secret the proxy sends in `header` with every request it passes on.
pub struct ProxySecret {
    header: HeaderName,
    value: Vec<u8>,
}

impl ProxySecret {
    /// The shortest secret accepted, in bytes.
    pub const MIN_LEN: usize = 32;

    /// `value`, sent in `header`; refused when shorter than [`Self::MIN_LEN`] bytes.
    pub fn new(header: &str, value: impl Into<Vec<u8>>) -> Result<ProxySecret> {
        let header = HeaderName::from_bytes(header.as_bytes())
            .with_context(|| format!("{header:?} is not a header name"))?;
        let value = value.into();
        if value.len() < Self::MIN_LEN {
            bail!(
                "the proxy's secret ({header}): at least {} characters",
                Self::MIN_LEN
            );
        }
        Ok(ProxySecret { header, value })
    }

    /// From the environment variable `env` ([`secret_from_env`]).
    pub fn from_env(env: &str, header: &str) -> Result<ProxySecret> {
        ProxySecret::new(header, secret_from_env(env)?)
    }

    pub fn header(&self) -> &HeaderName {
        &self.header
    }

    /// The request carries the secret: compared in constant time.
    pub fn verify(&self, headers: &HeaderMap) -> bool {
        headers
            .get(&self.header)
            .is_some_and(|sent| secret_eq(sent.as_bytes(), &self.value))
    }
}

impl fmt::Debug for ProxySecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProxySecret({}: <redacted>)", self.header)
    }
}

/// The proxy's secret from the environment variable `env`: required, so that a website
/// without it stops the bot at its start, not at the first visit, and at least
/// [`ProxySecret::MIN_LEN`] bytes.
pub fn secret_from_env(env: &str) -> Result<String> {
    let value = std::env::var(env).with_context(|| format!("[web] is on but {env} is not set"))?;
    if value.len() < ProxySecret::MIN_LEN {
        bail!("{env}: at least {} characters", ProxySecret::MIN_LEN);
    }
    Ok(value)
}

/// `sent == secret` in constant time (the length aside, which is no secret).
pub fn secret_eq(sent: &[u8], secret: &[u8]) -> bool {
    sent.ct_eq(secret).into()
}

// ── Who asks ────────────────────────────────────────────────────────────────

/// Who may use a website, besides the proxy's own rule.
#[derive(Clone, Debug, Default)]
pub struct Access {
    /// Groups (X-House-Groups) whose members may: `"all"`, a list (empty: nobody), or not
    /// set, which [`ForwardAuth::new`] refuses.
    pub allowed_groups: AllowList<String>,
    /// Groups whose members may always, and count as the website's admins
    /// ([`WebPerson::admin_group`]).
    pub admin_groups: Vec<String>,
    /// Without a login (a proxy that asks for it only from outside the house): a guest.
    pub guests: bool,
}

/// A website's check of the proxy's headers.
#[derive(Debug)]
pub struct ForwardAuth {
    secret: ProxySecret,
    access: Access,
    /// The server part of the house accounts (the bot's own).
    server_name: String,
}

/// Someone the proxy let through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Visitor {
    /// No login, and the website lets guests in ([`Access::guests`]).
    Guest,
    Person(WebPerson),
}

/// A person with a login.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebPerson {
    /// The login name (X-House-User), as sent.
    pub login: String,
    /// X-House-Name, else the login: without control characters, 64 characters at most.
    pub name: String,
    /// X-House-Email; empty without one.
    pub email: String,
    /// X-House-Groups, `|`-separated.
    pub groups: Vec<String>,
    /// [`house_account`] of the login.
    pub house_account: Option<OwnedUserId>,
    /// The house account first, then their other accounts (X-House-Matrix-Ids), each once,
    /// [`MAX_ACCOUNTS`] at most.
    pub accounts: Vec<OwnedUserId>,
    /// In one of [`Access::admin_groups`].
    pub admin_group: bool,
}

/// Why a request is turned away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not through the house's proxy (no or a wrong secret).
    Proxy,
    /// No login (or not one a person can have).
    NoLogin,
    /// In none of the allowed groups.
    NotAllowed,
}

impl ForwardAuth {
    /// `server_name`: the server part of the bot's own Matrix ID, where the house accounts
    /// are. Refuses `allowed_groups` not set; warns when nobody can get in.
    pub fn new(secret: ProxySecret, access: Access, server_name: &str) -> Result<ForwardAuth> {
        if !access.allowed_groups.is_set() {
            bail!(
                "[web] allowed_groups is not set: the groups that may use the website, or \"all\""
            );
        }
        if access.allowed_groups.is_none() && access.admin_groups.is_empty() {
            warn!("[web] allowed_groups is empty: nobody with a login may use the website");
        }
        Ok(ForwardAuth {
            secret,
            access,
            server_name: server_name.to_owned(),
        })
    }

    pub fn access(&self) -> &Access {
        &self.access
    }

    /// Who sent `headers`: the proxy's secret first, then the login and the groups.
    pub fn identify(&self, headers: &HeaderMap) -> Result<Visitor, Refusal> {
        if !self.secret.verify(headers) {
            return Err(Refusal::Proxy);
        }
        let login = header_text(headers, USER_HEADER);
        if login.is_empty() && self.access.guests {
            // The proxy drops a client's own X-House-* headers and lets requests without a
            // login through only where it is set up to (from inside the house).
            return Ok(Visitor::Guest);
        }
        if login.is_empty() || login.len() > 128 || login.chars().any(char::is_control) {
            return Err(Refusal::NoLogin);
        }
        let groups: Vec<String> = header_text(headers, GROUPS_HEADER)
            .split('|')
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .map(str::to_owned)
            .collect();
        let admin_group = self.access.admin_groups.iter().any(|g| groups.contains(g));
        if !admin_group && !allows(&self.access.allowed_groups, &groups) {
            return Err(Refusal::NotAllowed);
        }
        let house = house_account(login, &self.server_name);
        let accounts = with_linked(house.clone(), header_text(headers, MATRIX_IDS_HEADER));
        let email = Some(header_text(headers, EMAIL_HEADER))
            .filter(|e| e.len() <= 254 && !e.chars().any(char::is_control))
            .unwrap_or("")
            .to_owned();
        Ok(Visitor::Person(WebPerson {
            login: login.to_owned(),
            name: display_name(headers, login),
            email,
            groups,
            house_account: house,
            accounts,
            admin_group,
        }))
    }
}

/// Fails closed: not set or an empty list, nobody.
fn allows(allowed: &AllowList<String>, groups: &[String]) -> bool {
    match allowed {
        AllowList::Unset => false,
        AllowList::All => true,
        AllowList::Only(set) => groups.iter().any(|g| set.contains(g)),
    }
}

fn display_name(headers: &HeaderMap, login: &str) -> String {
    Some(header_text(headers, NAME_HEADER))
        .filter(|n| !n.is_empty())
        .unwrap_or(login)
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect()
}

/// A header as UTF-8 (names and groups may be "Jürgen" or Cyrillic, which
/// `HeaderValue::to_str` refuses), trimmed; "" when absent or not UTF-8.
pub fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
        .map(str::trim)
        .unwrap_or("")
}

/// The house account of a login on `server`: `@<login>:<server>`, lowercased, where the
/// login is a plain localpart (a–z, 0–9 and `._=-/`, 1–64 characters); `None` otherwise.
/// The one rule of both websites; it ignores `matrix_localpart` (see the module's docs).
pub fn house_account(login: &str, server: &str) -> Option<OwnedUserId> {
    // ASCII before lowercasing: Unicode lowercasing maps the Kelvin sign (U+212A) to `k`.
    let login = login.trim();
    if !login.is_ascii() {
        return None;
    }
    let lower = login.to_ascii_lowercase();
    let valid = (1..=64).contains(&lower.len())
        && lower
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._=-/".contains(c));
    valid
        .then(|| UserId::parse(format!("@{lower}:{server}")).ok())
        .flatten()
}

/// `first` (when there is one), then the Matrix IDs in `linked` (an X-House-Matrix-Ids
/// value: comma-separated, anything else left out), each once, [`MAX_ACCOUNTS`] at most.
pub fn with_linked(first: Option<OwnedUserId>, linked: &str) -> Vec<OwnedUserId> {
    let mut accounts: Vec<OwnedUserId> = first.into_iter().collect();
    for id in linked
        .split(',')
        .filter_map(|s| UserId::parse(s.trim()).ok())
    {
        if accounts.len() < MAX_ACCOUNTS && !accounts.contains(&id) {
            accounts.push(id);
        }
    }
    accounts
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_login_with_a_unicode_lookalike_has_no_house_account() {
        assert_eq!(house_account("\u{212a}im", "house.org"), None); // Kelvin sign, not K
        assert_eq!(
            house_account(" Kim ", "house.org").map(|id| id.to_string()),
            Some("@kim:house.org".to_owned())
        );
    }

    use std::collections::HashSet;

    use http::HeaderValue;

    use super::*;

    const HEADER: &str = "x-test-proxy-secret";
    const SECRET: &str = "a-test-proxy-secret-of-at-least-32-chars";

    fn auth(allowed: AllowList<String>, admin: &[&str], guests: bool) -> ForwardAuth {
        let access = Access {
            allowed_groups: allowed,
            admin_groups: admin.iter().map(|g| g.to_string()).collect(),
            guests,
        };
        ForwardAuth::new(
            ProxySecret::new(HEADER, SECRET).unwrap(),
            access,
            "example.org",
        )
        .unwrap()
    }

    fn only(groups: &[&str]) -> AllowList<String> {
        AllowList::Only(groups.iter().map(|g| g.to_string()).collect::<HashSet<_>>())
    }

    fn request(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (name, value) in pairs {
            h.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_bytes(value.as_bytes()).unwrap(),
            );
        }
        h
    }

    fn person(visitor: Result<Visitor, Refusal>) -> WebPerson {
        match visitor {
            Ok(Visitor::Person(p)) => p,
            other => panic!("not a person: {other:?}"),
        }
    }

    #[test]
    fn the_secret_is_required_and_at_least_32_bytes() {
        let env = "MXBOT_COMMON_TEST_WEB_SECRET";
        std::env::remove_var(env);
        let missing = ProxySecret::from_env(env, HEADER).unwrap_err();
        assert_eq!(
            missing.to_string(),
            format!("[web] is on but {env} is not set")
        );
        std::env::set_var(env, "x".repeat(31));
        let short = ProxySecret::from_env(env, HEADER).unwrap_err();
        assert_eq!(short.to_string(), format!("{env}: at least 32 characters"));
        std::env::set_var(env, "x".repeat(32));
        assert!(ProxySecret::from_env(env, HEADER).is_ok());
        std::env::remove_var(env);

        assert!(ProxySecret::new(HEADER, "").is_err());
        assert!(ProxySecret::new(HEADER, &b"0123456789abcdef0123456789abcde"[..]).is_err());
        assert!(ProxySecret::new("not a header", SECRET).is_err());
        let secret = ProxySecret::new(HEADER, SECRET).unwrap();
        assert!(!format!("{secret:?}").contains(SECRET));
    }

    #[test]
    fn only_the_exact_secret_in_its_own_header_passes() {
        let secret = ProxySecret::new(HEADER, SECRET).unwrap();
        assert!(secret.verify(&request(&[(HEADER, SECRET)])));
        assert!(!secret.verify(&HeaderMap::new()));
        // Elsewhere, or with a different case in the value.
        assert!(!secret.verify(&request(&[("x-other-proxy-secret", SECRET)])));
        assert!(!secret.verify(&request(&[(HEADER, &SECRET.to_uppercase())])));
        // Wrong in the last byte, a prefix, one byte more, the same length but different.
        let mut last = SECRET.to_owned();
        last.pop();
        assert!(!secret.verify(&request(&[(HEADER, &format!("{last}t"))])));
        assert!(!secret.verify(&request(&[(HEADER, &last)])));
        assert!(!secret.verify(&request(&[(HEADER, &format!("{SECRET}x"))])));
        assert!(!secret.verify(&request(&[(HEADER, &"b".repeat(SECRET.len()))])));
    }

    #[test]
    fn secrets_compare_in_constant_time_through_subtle() {
        // `subtle`'s comparison: every byte is looked at, whichever differs.
        assert!(secret_eq(b"same-secret", b"same-secret"));
        assert!(!secret_eq(b"Xame-secret", b"same-secret"));
        assert!(!secret_eq(b"same-secreX", b"same-secret"));
        assert!(!secret_eq(b"same", b"same-secret"));
        assert!(!secret_eq(b"", b"same-secret"));
        assert!(secret_eq(b"", b""));
    }

    #[test]
    fn a_wrong_secret_is_refused_before_anything_else() {
        let site = auth(AllowList::All, &[], true);
        let wrong = "b".repeat(SECRET.len());
        for headers in [
            request(&[(USER_HEADER, "alice"), (GROUPS_HEADER, "residents")]),
            request(&[(HEADER, &wrong), (USER_HEADER, "alice")]),
            request(&[(HEADER, "short")]),
        ] {
            assert_eq!(site.identify(&headers), Err(Refusal::Proxy));
        }
    }

    #[test]
    fn groups_fail_closed() {
        let alice = |groups: &str| {
            request(&[
                (HEADER, SECRET),
                (USER_HEADER, "alice"),
                (GROUPS_HEADER, groups),
            ])
        };
        // An empty list: nobody, also not someone in every group.
        let nobody = auth(only(&[]), &[], false);
        assert_eq!(
            nobody.identify(&alice("residents")),
            Err(Refusal::NotAllowed)
        );
        assert_eq!(nobody.identify(&alice("")), Err(Refusal::NotAllowed));
        // A list: one of its groups, exactly.
        let residents = auth(only(&["residents"]), &[], false);
        assert!(residents.identify(&alice("other| residents ")).is_ok());
        assert_eq!(
            residents.identify(&alice("guests|residentsx")),
            Err(Refusal::NotAllowed)
        );
        assert_eq!(residents.identify(&alice("")), Err(Refusal::NotAllowed));
        // "all": everybody with a login.
        assert!(auth(AllowList::All, &[], false)
            .identify(&alice(""))
            .is_ok());
        // Admin groups always get in, and are told apart.
        let admins = auth(only(&[]), &["it-admins"], false);
        assert!(person(admins.identify(&alice("it-admins"))).admin_group);
        assert_eq!(
            admins.identify(&alice("residents")),
            Err(Refusal::NotAllowed)
        );
        let both = auth(only(&["residents"]), &["it-admins"], false);
        assert!(!person(both.identify(&alice("residents"))).admin_group);
        // Not set at all: refused at the start.
        let unset = ForwardAuth::new(
            ProxySecret::new(HEADER, SECRET).unwrap(),
            Access::default(),
            "example.org",
        );
        assert!(unset.is_err());
    }

    #[test]
    fn without_a_login_a_guest_where_guests_are_let_in() {
        let no_login = request(&[(HEADER, SECRET), (GROUPS_HEADER, "residents")]);
        let blank = request(&[(HEADER, SECRET), (USER_HEADER, " ")]);
        let closed = auth(only(&["residents"]), &[], false);
        assert_eq!(closed.identify(&no_login), Err(Refusal::NoLogin));
        assert_eq!(closed.identify(&blank), Err(Refusal::NoLogin));
        // Guests are not checked for groups: they have none.
        let open = auth(only(&[]), &[], true);
        assert_eq!(open.identify(&no_login), Ok(Visitor::Guest));
        assert_eq!(open.identify(&blank), Ok(Visitor::Guest));
        // A login that no person has (a tab is the one control character a header takes).
        for login in ["a\tb".to_owned(), "x".repeat(129)] {
            let odd = request(&[(HEADER, SECRET), (USER_HEADER, &login)]);
            assert_eq!(open.identify(&odd), Err(Refusal::NoLogin));
        }
    }

    #[test]
    fn a_person_with_their_name_email_groups_and_accounts() {
        let site = auth(AllowList::All, &[], false);
        let kim = person(site.identify(&request(&[
            (HEADER, SECRET),
            (USER_HEADER, " Kim "),
            (GROUPS_HEADER, "residents|kitchen-1"),
            (NAME_HEADER, "Kim K."),
            (EMAIL_HEADER, "kim@example.org"),
            (
                MATRIX_IDS_HEADER,
                "@kim:matrix.org, not an id, @kim:example.org, @kim:matrix.org",
            ),
        ])));
        assert_eq!(kim.login, "Kim");
        assert_eq!(kim.name, "Kim K.");
        assert_eq!(kim.email, "kim@example.org");
        assert_eq!(kim.groups, ["residents", "kitchen-1"]);
        let id = |s: &str| UserId::parse(s).unwrap();
        assert_eq!(kim.house_account, Some(id("@kim:example.org")));
        assert_eq!(
            kim.accounts,
            [id("@kim:example.org"), id("@kim:matrix.org")]
        );
        assert!(!kim.admin_group);

        // No name: the login. Names are UTF-8, not just what `to_str` takes.
        let jurgen = person(site.identify(&request(&[
            (HEADER, SECRET),
            (USER_HEADER, "Jürgen Ö"),
            (EMAIL_HEADER, "bad\tmail"),
        ])));
        assert_eq!(jurgen.name, "Jürgen Ö");
        assert_eq!(jurgen.email, "");
        assert_eq!(jurgen.house_account, None);
        assert!(jurgen.accounts.is_empty());
        let named = person(site.identify(&request(&[
            (HEADER, SECRET),
            (USER_HEADER, "olha"),
            (NAME_HEADER, "Ольга"),
        ])));
        assert_eq!(named.name, "Ольга");
    }

    #[test]
    fn logins_become_house_accounts_and_linked_accounts_are_capped() {
        assert_eq!(
            house_account(" Alice ", "example.org").unwrap().as_str(),
            "@alice:example.org"
        );
        assert_eq!(
            house_account("ann.b-c", "example.org").unwrap().as_str(),
            "@ann.b-c:example.org"
        );
        for odd in ["a b", "", "jürgen", "kim+1", &"x".repeat(65)] {
            assert!(house_account(odd, "example.org").is_none(), "{odd}");
        }

        let many: Vec<String> = (0..20).map(|i| format!("@u{i}:example.org")).collect();
        let linked = many.join(",");
        let house = house_account("kim", "example.org");
        let with_house = with_linked(house.clone(), &linked);
        assert_eq!(with_house.len(), MAX_ACCOUNTS);
        assert_eq!(with_house.first(), house.as_ref());
        // Without a house account, the linked ones fill every place.
        assert_eq!(with_linked(None, &linked).len(), MAX_ACCOUNTS);
        assert!(with_linked(None, "").is_empty());
    }
}
