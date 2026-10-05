//! Shared-history files for provider switches: the immutable Markdown
//! snapshot a provider can read, durable copies of historical attachments,
//! and their cleanup after a session is deleted. Ported from the
//! `session_context_snapshot` half of src-tauri/src/session_store.rs and
//! from src-tauri/src/context_assets.rs.
//!
//! Files live under `<data dir>/context-history/<session id>`. A
//! per-session lifecycle lock orders writes and deletion, and file work
//! holds only that lock, so unrelated sessions can keep saving while one
//! session copies or removes files. Writers and cleanup take the lifecycle
//! lock before the database lock.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::session_store::{SessionStore, ensure_context_writable, validate_id};

const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_SESSION_BYTES: u64 = 64 * 1024 * 1024;
const MAX_INDEX_BYTES: usize = 4 * 1024 * 1024;

type ContextLifecycleLocks = HashMap<(PathBuf, String), Weak<Mutex<()>>>;
static CONTEXT_LIFECYCLE_LOCKS: OnceLock<Mutex<ContextLifecycleLocks>> = OnceLock::new();

/// The directory that holds one session's shared-history files.
pub fn context_history_dir(data_dir: &Path, session_id: &str) -> PathBuf {
    data_dir.join("context-history").join(session_id)
}

/// Run `action` while holding `session_id`'s lifecycle lock.
pub(crate) fn with_context_lifecycle<T>(
    data_dir: &Path,
    session_id: &str,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    validate_id(session_id, "session")?;
    let root = fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let lock = {
        let mut locks = CONTEXT_LIFECYCLE_LOCKS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .map_err(|_| "Shared context lifecycle registry is locked")?;
        locks.retain(|_, lock| lock.strong_count() > 0);
        let key = (root, session_id.to_string());
        match locks.get(&key).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(Mutex::new(()));
                locks.insert(key, Arc::downgrade(&lock));
                lock
            }
        }
    };
    let _guard = lock
        .lock()
        .map_err(|_| "Shared context lifecycle is locked")?;
    action()
}

/// Run a file write for a session that has not been deleted. The database
/// lock covers only the deleted-id check.
pub(crate) fn with_context_write<T>(
    store: &SessionStore,
    data_dir: &Path,
    session_id: &str,
    write: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    with_context_lifecycle(data_dir, session_id, || {
        {
            let conn = store.lock_conn()?;
            ensure_context_writable(&conn, session_id).map_err(|error| error.to_string())?;
        }
        write()
    })
}

/// `session_context_snapshot`: save the shared history for one switch and
/// return its path. A second write with the same content returns the same
/// path. Different content for the same switch is an error.
pub fn session_context_snapshot(
    store: &SessionStore,
    data_dir: &Path,
    session_id: &str,
    switch_id: &str,
    content: &str,
) -> Result<String, String> {
    with_context_write(store, data_dir, session_id, || {
        write_context_snapshot(data_dir, session_id, switch_id, content)
    })
}

fn write_context_snapshot(
    data_dir: &Path,
    session_id: &str,
    switch_id: &str,
    content: &str,
) -> Result<String, String> {
    validate_id(session_id, "session")?;
    validate_id(switch_id, "switch")?;
    if content.len() > MAX_SNAPSHOT_BYTES {
        return Err("Shared conversation history exceeds 64 MB".into());
    }
    let directory = context_history_dir(data_dir, session_id);
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let destination = directory.join(format!("{switch_id}.md"));
    if destination.exists() {
        if fs::read_to_string(&destination).map_err(|error| error.to_string())? != content {
            return Err("The saved switch history differs from this request".into());
        }
        return Ok(destination.to_string_lossy().into_owned());
    }
    let temporary = directory.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = private_file(&temporary)?;
        file.write_all(content.as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::rename(&temporary, &destination).map_err(|error| error.to_string())?;
        Ok(destination.to_string_lossy().into_owned())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn private_file(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|error| error.to_string())
}

/// Remove the files of every deleted session whose cleanup is pending. A
/// failed removal stays pending and retries on the next call. The database
/// lock is released during file work.
pub fn retry_context_cleanup(store: &SessionStore, root: &Path) -> Result<(), String> {
    let pending = {
        let conn = store.lock_conn()?;
        let mut statement = conn
            .prepare("SELECT session_id FROM context_history_cleanup WHERE pending = 1")
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?
    };
    for id in pending {
        let result = with_context_lifecycle(root, &id, || {
            {
                let conn = store.lock_conn()?;
                let pending: bool = conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM context_history_cleanup WHERE session_id = ?1 AND pending = 1)",
                        [&id],
                        |row| row.get(0),
                    )
                    .map_err(|error| error.to_string())?;
                if !pending {
                    return Ok(());
                }
            }
            match fs::remove_dir_all(context_history_dir(root, &id)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
            let conn = store.lock_conn()?;
            conn.execute(
                "UPDATE context_history_cleanup SET pending = 0 WHERE session_id = ?1",
                [&id],
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        });
        if let Err(error) = result {
            eprintln!("Shared context cleanup will need a retry: {error}");
        }
    }
    Ok(())
}

/// `ContextAssetSource`: where one historical attachment's bytes are. A
/// readable `path` wins over `data`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextAssetSource {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub data: Option<String>,
}

/// `ContextAssetSnapshot`: the saved copy, or why there is none.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextAssetSnapshot {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
}

/// `session_context_assets`: copy historical attachments into durable,
/// content-addressed storage for `session_id`.
pub fn session_context_assets(
    store: &SessionStore,
    data_dir: &Path,
    session_id: &str,
    attachments: Vec<ContextAssetSource>,
) -> Result<Vec<ContextAssetSnapshot>, String> {
    with_context_write(store, data_dir, session_id, || {
        snapshot_assets(data_dir, session_id, attachments)
    })
}

fn snapshot_assets(
    root: &Path,
    session_id: &str,
    attachments: Vec<ContextAssetSource>,
) -> Result<Vec<ContextAssetSnapshot>, String> {
    if validate_id(session_id, "session").is_err() {
        return Err("Invalid session id for historical attachment storage".into());
    }
    let assets = context_history_dir(root, session_id).join("assets");
    fs::create_dir_all(&assets).map_err(|error| error.to_string())?;
    let mut total = saved_asset_bytes(&assets)?;
    let index_path = assets.with_file_name("assets.index.json");
    let mut by_id = read_asset_index(&index_path)?;
    let mut results = Vec::with_capacity(attachments.len());
    for source in attachments {
        if let Some(saved) = by_id
            .get(&source.id)
            .filter(|saved| saved.id == source.id && valid_cached_asset(&assets, saved))
        {
            results.push(saved.clone());
            continue;
        }
        let saved = match snapshot_one(&assets, &source, &mut total) {
            Ok((path, hash)) => ContextAssetSnapshot {
                id: source.id.clone(),
                path: Some(path.to_string_lossy().into_owned()),
                sha256: Some(hash),
                unavailable_reason: None,
            },
            Err(reason) => ContextAssetSnapshot {
                id: source.id.clone(),
                path: None,
                sha256: None,
                unavailable_reason: Some(reason),
            },
        };
        by_id.insert(source.id, saved.clone());
        results.push(saved);
    }
    write_asset_index(&index_path, &by_id)?;
    Ok(results)
}

fn read_asset_index(path: &Path) -> Result<HashMap<String, ContextAssetSnapshot>, String> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_INDEX_BYTES as u64 {
        return Err("Historical attachment index is not a bounded regular file".into());
    }
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|_| "Historical attachment index is invalid".into())
}

fn valid_cached_asset(directory: &Path, snapshot: &ContextAssetSnapshot) -> bool {
    let (Some(path), Some(hash)) = (&snapshot.path, &snapshot.sha256) else {
        return false;
    };
    let path = Path::new(path);
    hash.len() == 64
        && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        && path.parent() == Some(directory)
        && path.file_stem().and_then(|stem| stem.to_str()) == Some(hash.as_str())
        && verify_saved_asset(path, hash).is_ok()
}

fn write_asset_index(
    path: &Path,
    index: &HashMap<String, ContextAssetSnapshot>,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(index).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_INDEX_BYTES {
        return Err("Historical attachment index exceeds the 4 MiB metadata limit".into());
    }
    let temporary = path.with_file_name(format!(".{}.index.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<(), String> {
        let mut file = private_file(&temporary)?;
        file.write_all(&bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        fs::rename(&temporary, path).map_err(|error| error.to_string())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn saved_asset_bytes(directory: &Path) -> Result<u64, String> {
    let mut total = 0u64;
    for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let metadata = entry.metadata().map_err(|error| error.to_string())?;
        if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

fn snapshot_one(
    directory: &Path,
    source: &ContextAssetSource,
    total: &mut u64,
) -> Result<(PathBuf, String), String> {
    let bytes = read_source(source)?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let extension = Path::new(&source.name)
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 12
                && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        .unwrap_or("bin")
        .to_ascii_lowercase();
    let target = directory.join(format!("{hash}.{extension}"));
    if target.exists() {
        verify_saved_asset(&target, &hash)?;
        return Ok((target, hash));
    }
    if total.saturating_add(bytes.len() as u64) > MAX_SESSION_BYTES {
        return Err("Historical attachment storage exceeds the 64 MiB session limit".into());
    }
    let temporary = directory.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = write_immutable_asset(&temporary, &target, &bytes, &hash);
    let _ = fs::remove_file(&temporary);
    result?;
    *total = total.saturating_add(bytes.len() as u64);
    Ok((target, hash))
}

fn read_source(source: &ContextAssetSource) -> Result<Vec<u8>, String> {
    if source.data.is_none()
        && let Some(path) = &source.path
    {
        let path = Path::new(path);
        if !path.is_absolute() {
            return Err("Historical attachment path is not absolute".into());
        }
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("Historical attachment is unavailable. {error}"))?;
        if !metadata.is_file() {
            return Err("Historical attachment path is not a regular file".into());
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Err("Historical attachment exceeds the 20 MiB file limit".into());
        }
        let file = File::open(path)
            .map_err(|error| format!("Historical attachment is unavailable. {error}"))?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err("Historical attachment exceeds the 20 MiB file limit".into());
        }
        return Ok(bytes);
    }
    let Some(data) = &source.data else {
        return Err("Historical attachment has no accessible file or saved bytes".into());
    };
    let encoded = if data.starts_with("data:") {
        data.split_once(',')
            .filter(|(header, _)| header.ends_with(";base64"))
            .map(|(_, encoded)| encoded)
            .ok_or("Historical attachment data URL is not base64")?
    } else {
        data.as_str()
    };
    if encoded.len() as u64 > MAX_FILE_BYTES.div_ceil(3) * 4 {
        return Err("Historical attachment exceeds the 20 MiB file limit".into());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| "Historical attachment data is not valid base64".to_string())?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("Historical attachment exceeds the 20 MiB file limit".into());
    }
    Ok(bytes)
}

fn verify_saved_asset(path: &Path, expected_hash: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return Err("Saved historical attachment is not a bounded regular file".into());
    }
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    if format!("{:x}", Sha256::digest(bytes)) != expected_hash {
        return Err("Saved historical attachment content does not match its hash".into());
    }
    Ok(())
}

/// Publish with a hard link so an existing copy is never replaced.
fn write_immutable_asset(
    temporary: &Path,
    target: &Path,
    bytes: &[u8],
    expected_hash: &str,
) -> Result<(), String> {
    let mut file = private_file(temporary)?;
    file.write_all(bytes).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    drop(file);
    match fs::hard_link(temporary, target) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            verify_saved_asset(target, expected_hash)
        }
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests;
