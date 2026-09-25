//! Runtime settings that admins can change over chat.
//!
//! The config file provides the baseline; admin overrides are persisted as a
//! small JSON object (by default `store/settings.json`) and survive
//! restarts. `!unset` drops an override, falling back to the config file.
//!
//! Only put non-secret, low-risk knobs into a settings struct. Credentials
//! stay in the config file or environment and can never be changed here.

use std::{
    marker::PhantomData,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use anyhow::{anyhow, bail, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{Map, Value};
use tokio::sync::{watch, Mutex};
use tracing::warn;

use crate::persist::{load_json_or_default, save_json_atomic};

type Validator = Arc<dyn Fn(&Value) -> Result<()> + Send + Sync>;

struct StoreInner {
    path: PathBuf,
    defaults: Value,
    overrides: RwLock<Map<String, Value>>,
    validate: Validator,
    /// Serializes writers so the file and memory never diverge.
    write_lock: Mutex<()>,
    changed: watch::Sender<u64>,
}

/// Type-erased settings store used by the admin console.
#[derive(Clone)]
pub struct SettingsStore {
    inner: Arc<StoreInner>,
}

/// Typed view of a [`SettingsStore`].
pub struct Settings<T> {
    store: SettingsStore,
    _type: PhantomData<fn() -> T>,
}

impl<T> Clone for Settings<T> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            _type: PhantomData,
        }
    }
}

impl<T> Settings<T>
where
    T: Serialize + DeserializeOwned + 'static,
{
    /// Load persisted overrides from `path` on top of `defaults` (normally
    /// taken from the config file). Overrides that no longer validate —
    /// e.g. after a field was renamed — are dropped with a warning.
    pub async fn load(path: PathBuf, defaults: T) -> Result<Self> {
        let defaults = serde_json::to_value(&defaults).context("Serializing default settings")?;
        if !defaults.is_object() {
            bail!("Settings must serialize to a JSON object");
        }
        let validate: Validator = Arc::new(|value: &Value| {
            serde_json::from_value::<T>(value.clone())
                .map(drop)
                .map_err(|error| anyhow!("{error}"))
        });
        let stored: Map<String, Value> = load_json_or_default(&path).await?;
        let mut overrides = Map::new();
        for (key, value) in stored {
            let mut candidate = overrides.clone();
            candidate.insert(key.clone(), value.clone());
            match validate(&merge(&defaults, &candidate)) {
                Ok(()) => {
                    overrides.insert(key, value);
                }
                Err(error) => warn!(key, %error, "Dropping invalid persisted setting"),
            }
        }
        let (changed, _) = watch::channel(0);
        Ok(Self {
            store: SettingsStore {
                inner: Arc::new(StoreInner {
                    path,
                    defaults,
                    overrides: RwLock::new(overrides),
                    validate,
                    write_lock: Mutex::new(()),
                    changed,
                }),
            },
            _type: PhantomData,
        })
    }

    /// The effective settings (config defaults + overrides).
    pub fn get(&self) -> T {
        serde_json::from_value(self.store.effective())
            .expect("settings are validated before they are stored")
    }

    pub fn store(&self) -> &SettingsStore {
        &self.store
    }
}

impl SettingsStore {
    fn effective(&self) -> Value {
        let overrides = self.inner.overrides.read().expect("settings lock poisoned");
        merge(&self.inner.defaults, &overrides)
    }

    /// Notified (with a new version number) whenever a setting changes.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.changed.subscribe()
    }

    /// `key = value` lines, marking overridden keys.
    pub fn describe(&self) -> String {
        let effective = self.effective();
        let overrides = self.inner.overrides.read().expect("settings lock poisoned");
        let Value::Object(map) = effective else {
            return String::new();
        };
        let mut lines: Vec<String> = map
            .iter()
            .map(|(key, value)| {
                let marker = if overrides.contains_key(key) {
                    " (changed)"
                } else {
                    ""
                };
                format!("{key} = {}{marker}", render(value))
            })
            .collect();
        lines.sort();
        lines.join("\n")
    }

    /// Set `key` from user input. Input that parses as JSON is used as such
    /// (numbers, booleans, lists, quoted strings), anything else is taken as
    /// a string. Returns the new value.
    pub async fn set(&self, key: &str, raw: &str) -> Result<String> {
        let key = key.trim();
        if !self
            .inner
            .defaults
            .as_object()
            .is_some_and(|d| d.contains_key(key))
        {
            bail!("unknown setting {key:?}");
        }
        let value = parse_input(raw);
        self.update(|overrides| {
            overrides.insert(key.to_owned(), value.clone());
        })
        .await?;
        Ok(render(&value))
    }

    /// Remove the override for `key`; returns the value it falls back to.
    pub async fn unset(&self, key: &str) -> Result<String> {
        let key = key.trim();
        let Some(default) = self.inner.defaults.get(key).cloned() else {
            bail!("unknown setting {key:?}");
        };
        self.update(|overrides| {
            overrides.remove(key);
        })
        .await?;
        Ok(render(&default))
    }

    async fn update(&self, change: impl FnOnce(&mut Map<String, Value>)) -> Result<()> {
        let _guard = self.inner.write_lock.lock().await;
        let mut candidate = self
            .inner
            .overrides
            .read()
            .expect("settings lock poisoned")
            .clone();
        change(&mut candidate);
        (self.inner.validate)(&merge(&self.inner.defaults, &candidate))
            .map_err(|error| anyhow!("invalid value: {error}"))?;
        save_json_atomic(&self.inner.path, &candidate).await?;
        *self
            .inner
            .overrides
            .write()
            .expect("settings lock poisoned") = candidate;
        self.inner.changed.send_modify(|version| *version += 1);
        Ok(())
    }
}

fn merge(defaults: &Value, overrides: &Map<String, Value>) -> Value {
    let mut merged = defaults.clone();
    if let Value::Object(map) = &mut merged {
        for (key, value) in overrides {
            map.insert(key.clone(), value.clone());
        }
    }
    merged
}

fn parse_input(raw: &str) -> Value {
    let raw = raw.trim();
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
}

fn render(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "(unset)".to_owned(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
    struct Knobs {
        enabled: bool,
        time: String,
        count: u32,
        language: Option<String>,
    }

    fn defaults() -> Knobs {
        Knobs {
            enabled: true,
            time: "07:00".into(),
            count: 3,
            language: None,
        }
    }

    #[tokio::test]
    async fn overrides_persist_validate_and_unset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let settings = Settings::load(path.clone(), defaults()).await.unwrap();
        let changes = settings.store().subscribe();

        assert_eq!(settings.store().set("count", "5").await.unwrap(), "5");
        assert_eq!(
            settings.store().set("time", "08:30").await.unwrap(),
            "08:30"
        );
        assert_eq!(settings.store().set("language", "de").await.unwrap(), "de");
        assert!(settings.store().set("count", "many").await.is_err());
        assert!(settings.store().set("password", "x").await.is_err());
        assert!(changes.has_changed().unwrap());
        assert_eq!(settings.get().count, 5);

        let reloaded = Settings::load(path.clone(), defaults()).await.unwrap();
        assert_eq!(reloaded.get().time, "08:30");
        assert!(reloaded.store().describe().contains("count = 5 (changed)"));

        assert_eq!(reloaded.store().unset("count").await.unwrap(), "3");
        assert_eq!(reloaded.get().count, 3);
        assert_eq!(reloaded.store().unset("language").await.unwrap(), "(unset)");
        assert_eq!(reloaded.get().language, None);
    }

    #[tokio::test]
    async fn invalid_persisted_overrides_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"count": "nope", "enabled": false}"#).unwrap();
        let settings = Settings::load(path, defaults()).await.unwrap();
        assert_eq!(settings.get().count, 3);
        assert!(!settings.get().enabled);
    }
}
