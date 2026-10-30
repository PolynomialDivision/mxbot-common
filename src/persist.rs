//! Small JSON files in the store directory, written atomically.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use tracing::error;

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

/// Load `path` as [`load_json_or_default`] does, but a file that does not
/// parse is moved aside (`<name>.corrupt-<unix time>`, logged as an error) and
/// `T::default()` returned, so a damaged file costs its contents instead of
/// stopping the bot at every start. For state that can be rebuilt or lost
/// (markers, caches); keep [`load_json_or_default`] where losing it is worse
/// than not starting.
pub async fn load_json_or_quarantine<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    let raw = match tokio::fs::read_to_string(path).await {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(error) => return Err(error).with_context(|| format!("Reading {}", path.display())),
    };
    match serde_json::from_str(&raw) {
        Ok(value) => Ok(value),
        Err(parse_error) => {
            let aside = sidecar(path, &format!("corrupt-{}", unix_now()));
            tokio::fs::rename(path, &aside)
                .await
                .with_context(|| format!("Moving {} aside", path.display()))?;
            error!(path = %path.display(), moved_to = %aside.display(), %parse_error,
                   "Store file did not parse; moved aside, starting empty");
            Ok(T::default())
        }
    }
}

/// Write `value` as pretty JSON via a temporary file and rename, so a crash
/// never leaves a truncated file behind. The temporary file's name is unique
/// (two writers of one file at the same moment do not share it) and its data
/// reach the disk before the rename (fsync), so a power cut leaves the old or
/// the new file, never an empty one.
pub async fn save_json_atomic<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent).await?;
    }
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let tmp = sidecar(
        path,
        &format!(
            "tmp-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ),
    );
    let data = serde_json::to_string_pretty(value)?;
    let written = async {
        let mut file = tokio::fs::File::create(&tmp).await?;
        tokio::io::AsyncWriteExt::write_all(&mut file, data.as_bytes()).await?;
        // tokio writes in the background: only flush reports a failed write (a full disk);
        // sync_all would keep it to itself and the rename would put a short file in place.
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        file.sync_all().await
    }
    .await;
    if let Err(error) = written {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(error).with_context(|| format!("Writing {}", tmp.display()));
    }
    if let Err(error) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(error).with_context(|| format!("Replacing {}", path.display()));
    }
    // The rename itself on disk too (best effort: not every file system syncs a directory).
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Ok(dir) = tokio::fs::File::open(parent).await {
            let _ = dir.sync_all().await;
        }
    }
    Ok(())
}

/// `path` with `.suffix` appended to its file name (`state.json.tmp-1-0`).
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
        let loaded: HashMap<String, u32> = load_json_or_default(&path).await.unwrap();
        assert_eq!(loaded.get("a"), Some(&1));
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            names,
            vec![std::ffi::OsString::from("state.json")],
            "no temporary file left"
        );
    }

    #[tokio::test]
    async fn concurrent_writers_of_one_file_all_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("posted.json");
        let writes = (0..20u32).map(|i| {
            let path = path.clone();
            tokio::spawn(async move { save_json_atomic(&path, &i).await })
        });
        for write in writes {
            write.await.unwrap().unwrap();
        }
        let last: u32 = load_json_or_default(&path).await.unwrap();
        assert!(last < 20);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn a_corrupt_file_is_moved_aside_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load_json_or_default::<HashMap<String, u32>>(&path)
            .await
            .is_err());
        let empty: HashMap<String, u32> = load_json_or_quarantine(&path).await.unwrap();
        assert!(empty.is_empty());
        assert!(!path.exists());
        let aside: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(
            aside.len() == 1 && aside[0].starts_with("state.json.corrupt-"),
            "{aside:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(&aside[0])).unwrap(),
            "{ not json"
        );
    }
}
