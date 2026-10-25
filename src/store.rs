//! Atomic JSON store read/write with lock-protected updates.
//!
//! Provides generic utilities for reading, writing, and updating JSON files
//! with atomic write semantics (write to `.tmp`, then rename) and optional
//! lock protection for concurrent access.

use crate::display::escape_path;
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::Path;

/// Read a JSON file, returning `T::default()` if the file doesn't exist.
///
/// Returns an error if the file exists but can't be parsed.
pub fn read<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    if !path.exists() {
        return Ok(T::default());
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read store file: {}", escape_path(path)))?;

    if content.trim().is_empty() {
        return Ok(T::default());
    }

    serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse store file: {}", escape_path(path)))
}

/// Write data to a JSON file atomically.
///
/// Writes to a temporary file (`.tmp` suffix) then renames to the target path.
/// This ensures readers never see a partially-written file.
pub fn write_atomic<T: Serialize>(path: &Path, data: &T) -> Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            crate::data_dir::create_private_dir(parent)?;
        }
    }

    let tmp_path = path.with_extension("tmp");

    let json =
        serde_json::to_string_pretty(data).with_context(|| "Failed to serialize store data")?;

    std::fs::write(&tmp_path, &json)
        .with_context(|| format!("Failed to write temp file: {}", escape_path(&tmp_path)))?;

    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("Failed to rename temp file to: {}", escape_path(path)))?;

    Ok(())
}

/// Read-modify-write with lock protection.
///
/// 1. Acquires the lock at `lock_path`
/// 2. Reads the current data from `data_path` (or `T::default()` if missing)
/// 3. Applies the mutation function `f`
/// 4. Writes the modified data atomically
/// 5. Releases the lock (via RAII guard drop)
///
/// This is the primary API for concurrent store access.
pub fn update<T, F>(data_path: &Path, lock_path: &Path, f: F) -> Result<()>
where
    T: DeserializeOwned + Default + Serialize,
    F: FnOnce(&mut T),
{
    let _guard = crate::lock::acquire(lock_path, std::time::Duration::from_secs(5))?;

    let mut data: T = read(data_path)?;
    f(&mut data);
    write_atomic(data_path, &data)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::collections::HashMap;

    #[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
    struct TestStore {
        items: HashMap<String, String>,
    }

    #[test]
    fn test_read_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nonexistent.json");

        let store: TestStore = read(&path).unwrap();
        assert_eq!(store, TestStore::default());
    }

    #[test]
    fn test_write_atomic_and_read() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.json");

        let mut store = TestStore::default();
        store.items.insert("key".to_string(), "value".to_string());

        write_atomic(&path, &store).unwrap();

        let loaded: TestStore = read(&path).unwrap();
        assert_eq!(loaded.items.get("key").unwrap(), "value");

        // Ensure tmp file was cleaned up
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn test_write_atomic_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("dir").join("test.json");

        let store = TestStore::default();
        write_atomic(&path, &store).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn test_update_with_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let data_path = tmp.path().join("store.json");
        let lock_path = tmp.path().join("store.lock");

        // First update — creates the file
        update::<TestStore, _>(&data_path, &lock_path, |store| {
            store.items.insert("first".to_string(), "1".to_string());
        })
        .unwrap();

        // Second update — modifies existing
        update::<TestStore, _>(&data_path, &lock_path, |store| {
            store.items.insert("second".to_string(), "2".to_string());
        })
        .unwrap();

        // Verify both entries exist
        let store: TestStore = read(&data_path).unwrap();
        assert_eq!(store.items.get("first").unwrap(), "1");
        assert_eq!(store.items.get("second").unwrap(), "2");

        // Lock should be released
        assert!(crate::lock::acquire(&lock_path, std::time::Duration::ZERO).is_ok());
    }

    #[test]
    fn test_read_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("empty.json");
        std::fs::write(&path, "").unwrap();

        let store: TestStore = read(&path).unwrap();
        assert_eq!(store, TestStore::default());
    }
}
