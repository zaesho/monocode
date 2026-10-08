//! Port of host/context-assets.ts: durable copies of the historical
//! attachments a provider switch refers to.
//!
//! Each copy is named by the SHA-256 of its bytes and published with a hard
//! link, so a reader never sees a partial file and a copy never changes.
//! `assets.index.json`, next to the assets directory, remembers which
//! attachment each copy came from, so a later switch still finds it after
//! the original file changed or was removed.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use monocode_core::Attachment;
use monocode_core::portable_context::ContextAssetSnapshot;
use sha2::{Digest, Sha256};

const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// How much one copy, and all of a session's copies, may hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextAssetLimits {
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for ContextAssetLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: MAX_FILE_BYTES,
            max_total_bytes: MAX_TOTAL_BYTES,
        }
    }
}

/// Why one attachment has no copy. A file system error reads as a generic
/// message, as Node's coded errors did.
enum Failure {
    Io,
    Message(String),
}

impl From<std::io::Error> for Failure {
    fn from(_: std::io::Error) -> Self {
        Self::Io
    }
}

fn fail<T>(message: &str) -> Result<T, Failure> {
    Err(Failure::Message(message.to_string()))
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn is_hash(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Test seams for the write and publish steps, which a test cannot make
/// fail through the file system alone.
#[cfg(test)]
pub(crate) mod faults {
    use std::cell::RefCell;

    pub(crate) enum Fault {
        /// Write the first three bytes, then fail.
        PartialWrite,
        /// Another writer publishes these bytes first.
        RacingPublisher(Vec<u8>),
    }

    thread_local! {
        pub(crate) static NEXT: RefCell<Option<Fault>> = const { RefCell::new(None) };
    }

    pub(crate) fn take() -> Option<Fault> {
        NEXT.with(|next| next.borrow_mut().take())
    }
}

/// `snapshotHostContextAssets`: save each attachment once under
/// `directory`, or record why it could not be saved.
pub fn snapshot_host_context_assets(
    directory: &Path,
    attachments: &[Attachment],
    limits: ContextAssetLimits,
) -> Result<Vec<ContextAssetSnapshot>, String> {
    if attachments.is_empty() {
        return Ok(Vec::new());
    }
    create_private_dir(directory).map_err(|error| error.to_string())?;
    let index_path = directory
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("assets.index.json");
    let saved = read_index(&index_path, directory);
    let mut total: u64 = 0;
    for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        if !is_hash(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let info = fs::symlink_metadata(entry.path()).map_err(|error| error.to_string())?;
        if info.is_file() {
            total += info.len();
        }
    }
    let mut seen = HashSet::new();
    let mut snapshots = Vec::new();
    for attachment in attachments {
        if !seen.insert(attachment.id.as_str()) {
            continue;
        }
        let retained = saved.iter().find(|entry| entry.id == attachment.id);
        let result = match retained {
            Some(retained) => verify_retained(retained, limits).map(|()| retained.clone()),
            None => save_copy(directory, attachment, limits, &mut total).map(|(path, hash)| {
                ContextAssetSnapshot {
                    id: attachment.id.clone(),
                    path: Some(path.to_string_lossy().into_owned()),
                    sha256: Some(hash),
                    unavailable_reason: None,
                }
            }),
        };
        snapshots.push(match result {
            Ok(snapshot) => snapshot,
            Err(failure) => ContextAssetSnapshot {
                id: attachment.id.clone(),
                unavailable_reason: Some(match failure {
                    Failure::Io => "The historical attachment cannot be read on this host".into(),
                    Failure::Message(message) => message,
                }),
                ..Default::default()
            },
        });
    }
    let mut merged: BTreeMap<String, ContextAssetSnapshot> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for entry in saved.into_iter().chain(
        snapshots
            .iter()
            .filter(|entry| entry.path.is_some() && entry.sha256.is_some())
            .cloned(),
    ) {
        if !merged.contains_key(&entry.id) {
            order.push(entry.id.clone());
        }
        merged.insert(entry.id.clone(), entry);
    }
    let index: Vec<&ContextAssetSnapshot> = order.iter().map(|id| &merged[id]).collect();
    write_index(&index_path, &index).map_err(|error| error.to_string())?;
    Ok(snapshots)
}

/// The saved index entries that still point at a hash-named copy in
/// `directory`. An unreadable index counts as empty.
fn read_index(path: &Path, directory: &Path) -> Vec<ContextAssetSnapshot> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(entries) = serde_json::from_str::<Vec<serde_json::Value>>(&text) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| serde_json::from_value::<ContextAssetSnapshot>(entry).ok())
        .filter(|entry| {
            entry.sha256.as_deref().is_some_and(|hash| {
                is_hash(hash)
                    && entry.path.as_deref()
                        == Some(directory.join(hash).to_string_lossy().as_ref())
            })
        })
        .collect()
}

fn verify_retained(
    retained: &ContextAssetSnapshot,
    limits: ContextAssetLimits,
) -> Result<(), Failure> {
    let (Some(path), Some(hash)) = (&retained.path, &retained.sha256) else {
        return fail("The saved attachment failed its content hash check");
    };
    let info = fs::symlink_metadata(path)?;
    if !info.is_file() || info.len() > limits.max_file_bytes {
        return fail(
            "The saved attachment exceeds the snapshot file limit or is not a regular file",
        );
    }
    if sha256(&fs::read(path)?) != *hash {
        return fail("The saved attachment failed its content hash check");
    }
    Ok(())
}

/// Read the attachment's bytes and publish them under their hash.
fn save_copy(
    directory: &Path,
    attachment: &Attachment,
    limits: ContextAssetLimits,
    total: &mut u64,
) -> Result<(PathBuf, String), Failure> {
    let data = read_attachment(attachment, limits)?;
    if data.len() as u64 > limits.max_file_bytes {
        return fail("The historical attachment exceeds the snapshot file limit");
    }
    let hash = sha256(&data);
    let path = directory.join(&hash);
    if path.exists() {
        if sha256(&fs::read(&path)?) != hash {
            return fail("The saved attachment failed its content hash check");
        }
        return Ok((path, hash));
    }
    if *total + data.len() as u64 > limits.max_total_bytes {
        return fail("The session attachment snapshots exceed the total size limit");
    }
    let temporary = directory.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let published = publish(&temporary, &path, &data, &hash);
    if temporary.exists() {
        let _ = fs::remove_file(&temporary);
    }
    published?;
    *total += data.len() as u64;
    Ok((path, hash))
}

fn read_attachment(
    attachment: &Attachment,
    limits: ContextAssetLimits,
) -> Result<Vec<u8>, Failure> {
    if let Some(data) = &attachment.data {
        if data.len() as u64 > limits.max_file_bytes.div_ceil(3) * 4 + 4 {
            return fail("The historical attachment exceeds the snapshot file limit");
        }
        return base64::engine::general_purpose::STANDARD
            .decode(data.trim())
            .or_else(|_| fail("The historical attachment cannot be read on this host"));
    }
    let Some(path) = attachment.path.as_deref().filter(|path| !path.is_empty()) else {
        return fail("The historical attachment is unavailable on this host");
    };
    if !Path::new(path).is_absolute() {
        return fail("The historical attachment has no absolute host path");
    }
    if !fs::symlink_metadata(path)?.is_file() {
        return fail("The historical attachment is not a regular file");
    }
    let mut file = open_no_follow(Path::new(path))?;
    let info = file.metadata()?;
    if !info.is_file() {
        return fail("The historical attachment is not a regular file");
    }
    if info.len() > limits.max_file_bytes {
        return fail("The historical attachment exceeds the snapshot file limit");
    }
    if info.len() as i64 != attachment.size {
        return fail("The historical attachment changed size before it could be saved");
    }
    let mut data = Vec::with_capacity(info.len() as usize);
    (&mut file).take(info.len() + 1).read_to_end(&mut data)?;
    if data.len() as u64 != info.len() {
        return fail("The historical attachment changed while it was being saved");
    }
    Ok(data)
}

/// Write `data` to a private temporary file, then link it into place. A
/// copy another writer published first must have the same content.
fn publish(temporary: &Path, path: &Path, data: &[u8], hash: &str) -> Result<(), Failure> {
    #[cfg(test)]
    let fault = faults::take();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o400);
    }
    let mut file = options.open(temporary)?;
    #[cfg(test)]
    if matches!(fault, Some(faults::Fault::PartialWrite)) {
        file.write_all(&data[..data.len().min(3)])?;
        return fail("Interrupted asset write");
    }
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);
    #[cfg(test)]
    if let Some(faults::Fault::RacingPublisher(contents)) = fault {
        fs::write(path, contents)?;
    }
    match fs::hard_link(temporary, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if sha256(&fs::read(path)?) != hash {
                return fail("The saved attachment failed its content hash check");
            }
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    // O_NOFOLLOW refuses a link swapped in after the check above. O_NONBLOCK
    // keeps a FIFO from blocking the open.
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

fn create_private_dir(directory: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(directory)
    }
}

fn write_index(path: &Path, index: &[&ContextAssetSnapshot]) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(index)?;
    let temporary = path.with_file_name(format!(
        "{}.{}.tmp",
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        uuid::Uuid::new_v4()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::faults::{Fault, NEXT};
    use super::*;
    use monocode_core::AttachmentKind;

    struct Setup {
        directory: tempfile::TempDir,
        original: PathBuf,
        attachment: Attachment,
        assets: PathBuf,
    }

    fn setup() -> Setup {
        let directory = tempfile::Builder::new()
            .prefix("monocode-context-assets-")
            .tempdir()
            .unwrap();
        let original = directory.path().join("original.txt");
        fs::write(&original, "original").unwrap();
        let attachment = Attachment {
            id: "file".into(),
            path: Some(original.to_string_lossy().into_owned()),
            size: 8,
            name: "original.txt".into(),
            mime_type: "text/plain".into(),
            kind: AttachmentKind::File,
            ..Default::default()
        };
        let assets = directory.path().join("assets");
        Setup {
            directory,
            original,
            attachment,
            assets,
        }
    }

    fn snapshot(s: &Setup, attachments: &[Attachment]) -> Vec<ContextAssetSnapshot> {
        snapshot_host_context_assets(&s.assets, attachments, ContextAssetLimits::default()).unwrap()
    }

    fn limits(max_file_bytes: u64, max_total_bytes: u64) -> ContextAssetLimits {
        ContextAssetLimits {
            max_file_bytes,
            max_total_bytes,
        }
    }

    fn listing(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn saves_content_by_hash_and_preserves_it_after_the_original_file_changes() {
        let s = setup();
        let saved = snapshot(&s, &[s.attachment.clone(), s.attachment.clone()]);
        assert_eq!(saved.len(), 1);
        let saved = saved[0].clone();
        assert_eq!(saved.sha256.as_deref(), Some(sha256(b"original").as_str()));
        let path = s.assets.join(saved.sha256.as_deref().unwrap());
        assert_eq!(saved.path.as_deref(), Some(path.to_string_lossy().as_ref()));
        fs::write(&s.original, "modified").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
        fs::remove_file(&s.original).unwrap();
        assert_eq!(snapshot(&s, std::slice::from_ref(&s.attachment))[0], saved);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o400
            );
        }
    }

    #[test]
    fn deduplicates_matching_content_without_charging_the_total_limit_twice() {
        let s = setup();
        let same = Attachment {
            id: "same-file".into(),
            ..s.attachment.clone()
        };
        let snapshots =
            snapshot_host_context_assets(&s.assets, &[s.attachment.clone(), same], limits(8, 8))
                .unwrap();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].path, snapshots[1].path);
        assert!(
            snapshots
                .iter()
                .all(|entry| entry.unavailable_reason.is_none())
        );
    }

    #[test]
    fn keeps_a_partial_write_outside_the_published_hash_path_and_allows_retry() {
        let s = setup();
        let final_path = s.assets.join(sha256(b"original"));
        NEXT.with(|next| *next.borrow_mut() = Some(Fault::PartialWrite));
        let failed = snapshot(&s, std::slice::from_ref(&s.attachment)).remove(0);
        assert!(failed.path.is_none());
        assert!(
            failed
                .unavailable_reason
                .as_deref()
                .unwrap()
                .contains("Interrupted asset write")
        );
        assert!(!final_path.exists());
        assert!(listing(&s.assets).is_empty());
        let saved = snapshot(&s, std::slice::from_ref(&s.attachment)).remove(0);
        assert_eq!(
            saved.path.as_deref(),
            Some(final_path.to_string_lossy().as_ref())
        );
        assert_eq!(fs::read_to_string(&final_path).unwrap(), "original");
    }

    #[test]
    fn preserves_a_racing_publishers_existing_file_only_with_matching_content() {
        for matching in [true, false] {
            let s = setup();
            let contents: &[u8] = if matching { b"original" } else { b"different" };
            let final_path = s.assets.join(sha256(b"original"));
            NEXT.with(|next| *next.borrow_mut() = Some(Fault::RacingPublisher(contents.to_vec())));
            let saved = snapshot(&s, std::slice::from_ref(&s.attachment)).remove(0);
            assert_eq!(fs::read(&final_path).unwrap(), contents);
            assert_eq!(listing(&s.assets), vec![sha256(b"original")]);
            if matching {
                assert_eq!(
                    saved.path.as_deref(),
                    Some(final_path.to_string_lossy().as_ref())
                );
            } else {
                assert!(saved.path.is_none());
                assert!(
                    saved
                        .unavailable_reason
                        .as_deref()
                        .unwrap()
                        .contains("content hash check")
                );
            }
        }
    }

    #[test]
    fn reports_missing_files_changed_files_symbolic_links_and_both_size_limits() {
        let s = setup();
        // Only Unix adds the symbolic link case below.
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut attachments = vec![
            Attachment {
                id: "missing".into(),
                path: Some(
                    s.directory
                        .path()
                        .join("missing")
                        .to_string_lossy()
                        .into_owned(),
                ),
                ..s.attachment.clone()
            },
            Attachment {
                id: "changed".into(),
                size: 7,
                ..s.attachment.clone()
            },
        ];
        #[cfg(unix)]
        {
            let link = s.directory.path().join("link");
            std::os::unix::fs::symlink(&s.original, &link).unwrap();
            attachments.push(Attachment {
                id: "link".into(),
                path: Some(link.to_string_lossy().into_owned()),
                ..s.attachment.clone()
            });
        }
        let snapshots = snapshot(&s, &attachments);
        assert!(
            snapshots
                .iter()
                .all(|entry| entry.unavailable_reason.is_some()
                    && entry.path.is_none()
                    && entry.sha256.is_none())
        );
        let file_limit = snapshot_host_context_assets(
            &s.assets,
            std::slice::from_ref(&s.attachment),
            limits(7, 8),
        )
        .unwrap();
        assert!(
            file_limit[0]
                .unavailable_reason
                .as_deref()
                .unwrap()
                .contains("file limit")
        );
        let total_limit = snapshot_host_context_assets(
            &s.assets,
            std::slice::from_ref(&s.attachment),
            limits(8, 7),
        )
        .unwrap();
        assert!(
            total_limit[0]
                .unavailable_reason
                .as_deref()
                .unwrap()
                .contains("total size limit")
        );
    }

    #[test]
    fn saves_retained_image_bytes_when_the_historical_attachment_has_no_source_path() {
        let s = setup();
        let saved = snapshot(
            &s,
            &[Attachment {
                path: None,
                data: Some(base64::engine::general_purpose::STANDARD.encode("original")),
                ..s.attachment.clone()
            }],
        )
        .remove(0);
        assert_eq!(fs::read_to_string(saved.path.unwrap()).unwrap(), "original");
    }

    #[test]
    fn uses_retained_submitted_bytes_when_the_original_file_has_changed() {
        let s = setup();
        fs::write(&s.original, "modified").unwrap();
        let saved = snapshot(
            &s,
            &[Attachment {
                data: Some(base64::engine::general_purpose::STANDARD.encode("original")),
                ..s.attachment.clone()
            }],
        )
        .remove(0);
        assert_eq!(fs::read_to_string(saved.path.unwrap()).unwrap(), "original");
    }
}
