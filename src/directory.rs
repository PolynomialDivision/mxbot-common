//! The house's directory (its identity provider, `[directory]`): who lives here, their
//! Matrix accounts and their groups.
//!
//! A person's accounts are their house account (@<login>:<server>, or the localpart
//! attribute) and the other accounts an admin entered for them (the `matrix_ids`
//! attribute: a list of full Matrix IDs, or one string separated by commas or spaces).
//! Only active people count, never service accounts. The API token comes from the
//! environment (`token_env`, `DIRECTORY_TOKEN` by default), never from the config; it needs
//! to view users and groups.
//!
//! A group nobody is in is missing from [`groups`]; a bot that must tell "exists, but has
//! nobody" from "unknown" reads [`Directory::existing_groups`] too and builds the answer
//! with [`groups_with_empty`].

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use matrix_sdk::ruma::{OwnedUserId, UserId};
use serde::Deserialize;
use tracing::warn;

/// The `[directory]` table.
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct DirectoryConfig {
    pub provider: DirectoryProvider,
    /// The REST API base, e.g. "https://login.example.org/api/v3".
    pub api_url: String,
    /// The house's homeserver, for the house accounts. Default: the bot's own server.
    #[serde(default)]
    pub server_name: Option<String>,
    /// User attribute that overrides the login as the house account's localpart.
    #[serde(default)]
    pub localpart_attribute: Option<String>,
    /// User attribute with a person's other accounts.
    #[serde(default = "default_matrix_ids_attribute")]
    pub matrix_ids_attribute: String,
    /// Environment variable with the read-only API token.
    #[serde(default = "default_token_env")]
    pub token_env: String,
    /// How often a bot reads it again.
    #[serde(default = "default_refresh_minutes")]
    pub refresh_minutes: u64,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DirectoryProvider {
    Authentik,
}

fn default_matrix_ids_attribute() -> String {
    "matrix_ids".to_owned()
}

fn default_token_env() -> String {
    "DIRECTORY_TOKEN".to_owned()
}

fn default_refresh_minutes() -> u64 {
    15
}

impl DirectoryConfig {
    pub fn refresh(&self) -> Duration {
        Duration::from_secs(self.refresh_minutes.max(1) * 60)
    }
}

/// One person in the directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    pub username: String,
    /// The house account first, then the listed ones in their order.
    pub accounts: Vec<OwnedUserId>,
    pub groups: BTreeSet<String>,
}

/// The members' accounts of some groups.
pub type Groups = BTreeMap<String, BTreeSet<OwnedUserId>>;

/// The accounts of each of `wanted` that anyone is in; a group nobody is in (or that does
/// not exist) is missing from the answer. [`groups_with_empty`] tells the two apart.
pub fn groups(people: &[Person], wanted: &BTreeSet<String>) -> Groups {
    let mut out = Groups::new();
    for person in people {
        for group in person.groups.iter().filter(|g| wanted.contains(*g)) {
            out.entry(group.clone())
                .or_default()
                .extend(person.accounts.iter().cloned());
        }
    }
    out
}

/// [`groups`], plus an empty entry for each of `wanted` that `existing` names (from
/// [`Directory::existing_groups`]) and nobody is in: a group that exists but has nobody is
/// in the answer with no accounts, only an unknown group is missing from it.
pub fn groups_with_empty(
    people: &[Person],
    wanted: &BTreeSet<String>,
    existing: &BTreeSet<String>,
) -> Groups {
    let mut out = groups(people, wanted);
    for group in wanted.iter().filter(|g| existing.contains(*g)) {
        out.entry(group.clone()).or_default();
    }
    out
}

/// A reader of the directory.
pub struct Directory {
    cfg: DirectoryConfig,
    server_name: String,
    token: String,
    http: reqwest::Client,
}

impl Directory {
    /// The reader for `cfg`, with `own` (the bot's account) giving the default server. Fails
    /// without a token in `token_env`.
    pub fn new(cfg: &DirectoryConfig, own: &UserId) -> Result<Self> {
        let token = std::env::var(&cfg.token_env)
            .ok()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .with_context(|| format!("no directory token in {}", cfg.token_env))?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("the directory's HTTP client")?;
        Ok(Self {
            server_name: cfg
                .server_name
                .clone()
                .unwrap_or_else(|| own.server_name().to_string()),
            cfg: cfg.clone(),
            token,
            http,
        })
    }

    pub fn config(&self) -> &DirectoryConfig {
        &self.cfg
    }

    /// Every active person. An answer without anyone is an error: a directory that names
    /// nobody is broken, and a bot acting on it would remove everyone.
    pub async fn people(&self) -> Result<Vec<Person>> {
        let DirectoryProvider::Authentik = self.cfg.provider;
        let users: Vec<User> = self
            .all_pages("core/users/?is_active=true&include_groups=true", "users")
            .await?;
        let people = people_of(&users, &self.cfg, &self.server_name);
        if people.is_empty() {
            bail!("the directory names nobody");
        }
        Ok(people)
    }

    /// Which of `wanted` exist in the directory, whether anyone is in them or not (a group
    /// that is missing from [`groups`] exists with nobody in it when it is in this answer).
    /// An answer without any group is an error: every house has groups, and a token that may
    /// not see them gets an empty list from Authentik, not a refusal.
    pub async fn existing_groups(&self, wanted: &BTreeSet<String>) -> Result<BTreeSet<String>> {
        let DirectoryProvider::Authentik = self.cfg.provider;
        if wanted.is_empty() {
            return Ok(BTreeSet::new());
        }
        let all: Vec<Group> = self
            .all_pages("core/groups/?include_users=false", "groups")
            .await?;
        if all.is_empty() {
            bail!("the directory names no group");
        }
        Ok(all
            .into_iter()
            .map(|g| g.name)
            .filter(|g| wanted.contains(g))
            .collect())
    }

    /// Every page of a list (`path` with its query, below the API base).
    async fn all_pages<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        what: &str,
    ) -> Result<Vec<T>> {
        let base = self.cfg.api_url.trim_end_matches('/');
        let mut out: Vec<T> = Vec::new();
        let mut page = 1u64;
        // At most 50 pages of 200: far more than a house.
        for _ in 0..50 {
            let url = format!("{base}/{path}&page_size=200&page={page}");
            let body = self
                .http
                .get(&url)
                .bearer_auth(&self.token)
                .send()
                .await
                .context("reading the directory")?
                .error_for_status()
                .context("reading the directory")?
                .bytes()
                .await
                .context("reading the directory")?;
            let answer: Page<T> =
                serde_json::from_slice(&body).context("the directory's answer")?;
            out.extend(answer.results);
            match answer.pagination.and_then(|p| p.next.as_u64()) {
                Some(next) if next > page => page = next,
                _ => return Ok(out),
            }
        }
        bail!("the directory has more than 50 pages of {what}")
    }
}

#[derive(Deserialize, Debug)]
struct User {
    username: String,
    #[serde(default = "yes")]
    is_active: bool,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    attributes: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    groups_obj: Vec<Group>,
}

#[derive(Deserialize, Debug)]
struct Group {
    name: String,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct Page<T> {
    results: Vec<T>,
    #[serde(default)]
    pagination: Option<Pagination>,
}

#[derive(Deserialize)]
struct Pagination {
    /// The next page's number, 0 on the last one.
    #[serde(default)]
    next: serde_json::Value,
}

fn people_of(users: &[User], cfg: &DirectoryConfig, server_name: &str) -> Vec<Person> {
    users
        .iter()
        .filter(|u| u.is_active && !u.kind.contains("service_account"))
        .map(|u| Person {
            username: u.username.clone(),
            accounts: accounts_of(u, cfg, server_name),
            groups: u.groups_obj.iter().map(|g| g.name.clone()).collect(),
        })
        .collect()
}

fn accounts_of(user: &User, cfg: &DirectoryConfig, server_name: &str) -> Vec<OwnedUserId> {
    let localpart = cfg
        .localpart_attribute
        .as_ref()
        .and_then(|a| user.attributes.get(a))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .unwrap_or(&user.username)
        .to_lowercase();
    let mut out: Vec<OwnedUserId> = Vec::new();
    match UserId::parse(format!("@{localpart}:{server_name}")) {
        Ok(id) => out.push(id),
        Err(_) => warn!(user = %user.username, "Directory: no valid house account"),
    }
    let listed: Vec<&str> = match user.attributes.get(&cfg.matrix_ids_attribute) {
        Some(serde_json::Value::Array(items)) => items.iter().filter_map(|v| v.as_str()).collect(),
        Some(serde_json::Value::String(s)) => s.split([',', ' ', '\n', '\t']).collect(),
        _ => Vec::new(),
    };
    for id in listed.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        match UserId::parse(id) {
            Ok(id) if !out.contains(&id) => out.push(id),
            Ok(_) => {}
            Err(_) => warn!(user = %user.username, "Directory: not a Matrix ID, left out"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DirectoryConfig {
        toml::from_str(
            "provider = 'authentik'\napi_url = 'https://login.example.org/api/v3'\n\
             localpart_attribute = 'matrix_localpart'",
        )
        .unwrap()
    }

    fn people() -> Vec<Person> {
        let users: Vec<User> = serde_json::from_value(serde_json::json!([
            {"username": "anna", "groups_obj": [{"name": "residents"}, {"name": "kitchen-floor-1"}],
             "attributes": {"matrix_ids": ["@anna:matrix.org", "not an id", "@anna:house.org"]}},
            {"username": "Ben", "groups_obj": [{"name": "kitchen-floor-1"}, {"name": "finance"}],
             "attributes": {"matrix_localpart": "benny", "matrix_ids": "@ben:matrix.org, @ben:example.org"}},
            {"username": "old", "is_active": false, "groups_obj": [{"name": "kitchen-floor-1"}]},
            {"username": "svc", "type": "service_account", "groups_obj": [{"name": "kitchen-floor-1"}]},
            {"username": "cara", "groups_obj": [{"name": "kitchen-floor-3"}]}
        ]))
        .unwrap();
        people_of(&users, &cfg(), "house.org")
    }

    fn ids(list: &[&str]) -> Vec<OwnedUserId> {
        list.iter().map(|s| UserId::parse(*s).unwrap()).collect()
    }

    #[test]
    fn defaults() {
        let c = cfg();
        assert_eq!(c.token_env, "DIRECTORY_TOKEN");
        assert_eq!(c.matrix_ids_attribute, "matrix_ids");
        assert_eq!(c.refresh(), Duration::from_secs(900));
        assert!(c.server_name.is_none());
    }

    #[test]
    fn active_people_with_their_accounts() {
        let p = people();
        assert_eq!(
            p.iter().map(|p| p.username.as_str()).collect::<Vec<_>>(),
            ["anna", "Ben", "cara"]
        );
        // the house account first, no duplicates, invalid IDs left out
        assert_eq!(p[0].accounts, ids(&["@anna:house.org", "@anna:matrix.org"]));
        assert_eq!(
            p[1].accounts,
            ids(&["@benny:house.org", "@ben:matrix.org", "@ben:example.org"])
        );
    }

    #[test]
    fn each_wanted_group_with_its_members_accounts() {
        let wanted: BTreeSet<String> = ["kitchen-floor-1", "finance", "kitchen-floor-4"]
            .map(String::from)
            .into();
        let g = groups(&people(), &wanted);
        assert_eq!(
            g["kitchen-floor-1"],
            ids(&[
                "@anna:house.org",
                "@anna:matrix.org",
                "@benny:house.org",
                "@ben:matrix.org",
                "@ben:example.org"
            ])
            .into_iter()
            .collect()
        );
        assert_eq!(g["finance"].len(), 3);
        assert!(!g.contains_key("kitchen-floor-4"));
        assert!(!g.contains_key("kitchen-floor-3"));
    }

    #[test]
    fn an_existing_group_without_anyone_is_empty_an_unknown_one_missing() {
        let wanted: BTreeSet<String> = ["kitchen-floor-1", "finance", "kitchen-floor-4", "gone"]
            .map(String::from)
            .into();
        let existing: BTreeSet<String> = ["kitchen-floor-1", "finance", "kitchen-floor-4"]
            .map(String::from)
            .into();
        let g = groups_with_empty(&people(), &wanted, &existing);
        assert_eq!(g["kitchen-floor-1"].len(), 5);
        assert_eq!(g["finance"].len(), 3);
        assert!(g["kitchen-floor-4"].is_empty());
        assert!(!g.contains_key("gone"));
        // a group not wanted stays out, even if it exists
        assert!(!g.contains_key("kitchen-floor-3"));
        // a group someone is in counts even when the group list missed it (read in between)
        let g = groups_with_empty(&people(), &wanted, &BTreeSet::new());
        assert_eq!(g, groups(&people(), &wanted));
    }

    fn reader(url: String, token_env: &str) -> Directory {
        let mut c = cfg();
        c.api_url = url;
        c.token_env = token_env.into();
        Directory::new(&c, &UserId::parse("@bot:house.org").unwrap()).unwrap()
    }

    #[tokio::test]
    async fn existing_groups_reads_every_page_and_no_group_at_all_is_an_error() {
        use wiremock::matchers::{header, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        std::env::set_var("MXBOT_TEST_GROUPS_TOKEN", "secret");
        let wanted: BTreeSet<String> = ["finance", "kitchen-floor-4", "gone"]
            .map(String::from)
            .into();
        let server = MockServer::start().await;
        for (page, next, names) in [
            ("1", 2, ["residents", "finance"]),
            ("2", 0, ["kitchen-floor-4", "it-admins"]),
        ] {
            Mock::given(method("GET"))
                .and(path("/api/v3/core/groups/"))
                .and(query_param("page", page))
                .and(query_param("page_size", "200"))
                .and(query_param("include_users", "false"))
                .and(header("authorization", "Bearer secret"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "pagination": {"next": next},
                    "results": names.map(|n| serde_json::json!({"pk": n, "name": n, "users": []})),
                })))
                .expect(1)
                .mount(&server)
                .await;
        }
        let dir = reader(
            format!("{}/api/v3", server.uri()),
            "MXBOT_TEST_GROUPS_TOKEN",
        );
        let found = dir.existing_groups(&wanted).await.unwrap();
        assert_eq!(
            found,
            ["finance", "kitchen-floor-4"].map(String::from).into()
        );
        // nothing wanted: nothing asked
        assert!(dir
            .existing_groups(&BTreeSet::new())
            .await
            .unwrap()
            .is_empty());

        // a token that may not see groups gets an empty list: an error, not "none exist"
        let empty = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&empty)
            .await;
        let dir = reader(empty.uri(), "MXBOT_TEST_GROUPS_TOKEN");
        assert!(dir.existing_groups(&wanted).await.is_err());
        let refused = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&refused)
            .await;
        let dir = reader(refused.uri(), "MXBOT_TEST_GROUPS_TOKEN");
        assert!(dir.existing_groups(&wanted).await.is_err());
        let garbled = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
            .mount(&garbled)
            .await;
        let dir = reader(garbled.uri(), "MXBOT_TEST_GROUPS_TOKEN");
        assert!(dir.existing_groups(&wanted).await.is_err());
    }

    #[tokio::test]
    async fn reads_every_page_with_the_token_and_an_empty_answer_is_an_error() {
        use wiremock::matchers::{header, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        for (page, next, name) in [("1", 2, "one"), ("2", 0, "two")] {
            Mock::given(method("GET"))
                .and(path("/api/v3/core/users/"))
                .and(query_param("page", page))
                .and(query_param("is_active", "true"))
                .and(query_param("include_groups", "true"))
                .and(header("authorization", "Bearer secret"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "pagination": {"next": next},
                    "results": [{"username": name, "type": "internal", "attributes": {},
                                 "groups_obj": [{"name": "residents"}]}],
                })))
                .expect(1)
                .mount(&server)
                .await;
        }
        std::env::set_var("MXBOT_TEST_DIRECTORY_TOKEN", " secret\n");
        let reader = |url: String| {
            let mut c = cfg();
            c.api_url = url;
            c.token_env = "MXBOT_TEST_DIRECTORY_TOKEN".into();
            Directory::new(&c, &UserId::parse("@bot:house.org").unwrap()).unwrap()
        };
        let people = reader(format!("{}/api/v3/", server.uri()))
            .people()
            .await
            .unwrap();
        let names: Vec<&str> = people.iter().map(|p| p.username.as_str()).collect();
        assert_eq!(names, ["one", "two"]);
        // the bot's own server for the house accounts
        assert_eq!(people[0].accounts, ids(&["@one:house.org"]));

        let empty = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&empty)
            .await;
        assert!(reader(empty.uri()).people().await.is_err());
        let refused = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&refused)
            .await;
        assert!(reader(refused.uri()).people().await.is_err());

        let mut c = cfg();
        c.token_env = "MXBOT_TEST_NO_SUCH_TOKEN".into();
        assert!(Directory::new(&c, &UserId::parse("@bot:house.org").unwrap()).is_err());
    }
}
