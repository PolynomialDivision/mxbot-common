//! Small JSON files in the store directory, written atomically.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};

/// Load `path`, or `T::default()` when the file does not exist.
pub async fn load_json_or_default<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match tokio::fs::read_to_string(path).await {
        Ok(raw) => {
            serde_json::from_str(&raw).with_context(|| format!("Parsing {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("Reading {}", path.display())),
    }
}

/// Write `value` as pretty JSON via a temporary file and rename, so a crash
/// never leaves a truncated file behind.
pub async fn save_json_atomic<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, serde_json::to_string_pretty(value)?)
        .await
        .with_context(|| format!("Writing {}", tmp.display()))?;
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("Replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn round_trip_is_atomic_and_missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let empty: HashMap<String, u32> = load_json_or_default(&path).await.unwrap();
        assert!(empty.is_empty());

        save_json_atomic(&path, &HashMap::from([("a".to_owned(), 1u32)]))
            .await
            .unwrap();
        assert!(!path.with_extension("tmp").exists());
        let loaded: HashMap<String, u32> = load_json_or_default(&path).await.unwrap();
        assert_eq!(loaded.get("a"), Some(&1));
    }
}
