//! Ported from the context tests in src-tauri/src/session_store.rs and
//! src-tauri/src/context_assets.rs.

use std::sync::mpsc;
use std::time::Duration;

use super::*;
use crate::session_store::tests::sample;
use crate::session_store::{delete_session, delete_stored_session, get_session, upsert_session};

struct Fixture(PathBuf);

impl Fixture {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("monocode-{label}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn pending(store: &SessionStore, id: &str) -> i64 {
    store
        .lock_conn()
        .unwrap()
        .query_row(
            "SELECT pending FROM context_history_cleanup WHERE session_id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn context_snapshot_is_scoped_immutable_and_readable() {
    let root = Fixture::new("context");
    let body = "User said café\nAssistant replied\n";
    let snapshot = write_context_snapshot(&root.0, "session-1", "switch-1", body).unwrap();
    assert!(Path::new(&snapshot).starts_with(root.0.join("context-history/session-1")));
    assert_eq!(fs::read_to_string(&snapshot).unwrap(), body);
    assert_eq!(
        write_context_snapshot(&root.0, "session-1", "switch-1", body).unwrap(),
        snapshot
    );
    assert!(write_context_snapshot(&root.0, "session-1", "switch-1", "Different history").is_err());
    assert!(write_context_snapshot(&root.0, "../escape", "switch-2", "History").is_err());
    assert!(write_context_snapshot(&root.0, "session-1", "../escape", "History").is_err());
}

#[test]
fn blocked_context_snapshot_does_not_block_another_session_upsert() {
    let root = Fixture::new("context-concurrency");
    let store = Arc::new(SessionStore::open(root.0.join("monocode.db")).unwrap());
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let writer_store = Arc::clone(&store);
    let writer_root = root.0.clone();
    let writer = std::thread::spawn(move || {
        with_context_write(&writer_store, &writer_root, "slow-session", || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            write_context_snapshot(&writer_root, "slow-session", "switch-1", "History")
        })
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (saved_tx, saved_rx) = mpsc::channel();
    let saving_store = Arc::clone(&store);
    let saving = std::thread::spawn(move || {
        let conn = saving_store.lock_conn().unwrap();
        upsert_session(&conn, &sample("other-session", "/tmp/other", "Independent")).unwrap();
        saved_tx.send(()).unwrap();
    });
    let saved_before_release = saved_rx.recv_timeout(Duration::from_secs(2));
    release_tx.send(()).unwrap();
    assert!(writer.join().unwrap().is_ok());
    saving.join().unwrap();
    assert!(
        get_session(&store.lock_conn().unwrap(), "other-session")
            .unwrap()
            .is_some()
    );
    assert!(
        saved_before_release.is_ok(),
        "An unrelated upsert waited for context filesystem work"
    );
}

#[test]
fn deletion_waits_for_context_publication_and_prevents_late_snapshots() {
    let root = Fixture::new("context-concurrency");
    let store = Arc::new(SessionStore::open(root.0.join("monocode.db")).unwrap());
    upsert_session(
        &store.lock_conn().unwrap(),
        &sample("deleted-session", "/tmp/a", "First"),
    )
    .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let writer_store = Arc::clone(&store);
    let writer_root = root.0.clone();
    let writer = std::thread::spawn(move || {
        with_context_write(&writer_store, &writer_root, "deleted-session", || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            write_context_snapshot(&writer_root, "deleted-session", "switch-1", "History")
        })
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (deleting_tx, deleting_rx) = mpsc::channel();
    let (deleted_tx, deleted_rx) = mpsc::channel();
    let deleting_store = Arc::clone(&store);
    let deleting_root = root.0.clone();
    let deleting = std::thread::spawn(move || {
        deleting_tx.send(()).unwrap();
        let result =
            delete_stored_session(&deleting_store, Some(&deleting_root), "deleted-session");
        deleted_tx.send(result).unwrap();
    });
    deleting_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let deleted_before_release = deleted_rx.recv_timeout(Duration::from_millis(200));
    release_tx.send(()).unwrap();
    assert!(writer.join().unwrap().is_ok());
    let deleted = deleted_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    deleting.join().unwrap();
    assert!(
        deleted_before_release.is_err(),
        "Deletion finished before an active context writer"
    );
    assert!(deleted.is_ok());
    assert!(!root.0.join("context-history/deleted-session").exists());
    assert!(
        get_session(&store.lock_conn().unwrap(), "deleted-session")
            .unwrap()
            .is_none()
    );
    delete_stored_session(&store, Some(&root.0), "deleted-session").unwrap();
    assert!(
        session_context_snapshot(&store, &root.0, "deleted-session", "switch-2", "Late").is_err()
    );
    assert_eq!(pending(&store, "deleted-session"), 0);
    assert!(!root.0.join("context-history/deleted-session").exists());
}

#[test]
fn waiting_context_cleanup_leaves_the_database_available() {
    let root = Fixture::new("context-concurrency");
    let store = Arc::new(SessionStore::open(root.0.join("monocode.db")).unwrap());
    delete_session(&store.lock_conn().unwrap(), "deleted-session").unwrap();
    write_context_snapshot(&root.0, "deleted-session", "switch-1", "Pending removal").unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let busy_root = root.0.clone();
    let busy = std::thread::spawn(move || {
        with_context_lifecycle(&busy_root, "deleted-session", || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        })
        .unwrap();
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (cleanup_tx, cleanup_rx) = mpsc::channel();
    let cleanup_store = Arc::clone(&store);
    let cleanup_root = root.0.clone();
    let cleanup = std::thread::spawn(move || {
        cleanup_tx.send(()).unwrap();
        retry_context_cleanup(&cleanup_store, &cleanup_root).unwrap();
    });
    cleanup_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (saved_tx, saved_rx) = mpsc::channel();
    let saving_store = Arc::clone(&store);
    let saving = std::thread::spawn(move || {
        upsert_session(
            &saving_store.lock_conn().unwrap(),
            &sample("other-session", "/tmp/b", "Other"),
        )
        .unwrap();
        saved_tx.send(()).unwrap();
    });
    let saved_before_release = saved_rx.recv_timeout(Duration::from_secs(2));
    let (opened_tx, opened_rx) = mpsc::channel();
    let reopening_root = root.0.clone();
    let reopening = std::thread::spawn(move || {
        let reopened = SessionStore::open(reopening_root.join("monocode.db")).unwrap();
        opened_tx.send(()).unwrap();
        reopened
    });
    let opened_before_release = opened_rx.recv_timeout(Duration::from_millis(200));
    release_tx.send(()).unwrap();
    busy.join().unwrap();
    cleanup.join().unwrap();
    saving.join().unwrap();
    drop(reopening.join().unwrap());
    assert!(
        saved_before_release.is_ok(),
        "Cleanup waited for a session lifecycle lock while holding the database"
    );
    assert!(
        opened_before_release.is_err(),
        "Startup cleanup ignored the active session lifecycle lock"
    );
    assert!(!root.0.join("context-history/deleted-session").exists());
}

#[test]
fn deleted_context_recovers_a_crash_before_filesystem_removal() {
    let root = Fixture::new("context-cleanup");
    let path = root.0.join("monocode.db");
    let history = root.0.join("context-history/deleted-session");
    {
        let store = SessionStore::open(path.clone()).unwrap();
        let conn = store.lock_conn().unwrap();
        upsert_session(&conn, &sample("deleted-session", "/tmp/a", "First")).unwrap();
        fs::create_dir_all(&history).unwrap();
        fs::write(history.join("history.md"), "Retained context").unwrap();
        delete_session(&conn, "deleted-session").unwrap();
        assert!(history.exists());
    }
    let reopened = SessionStore::open(path).unwrap();
    assert!(!history.exists());
    assert_eq!(pending(&reopened, "deleted-session"), 0);
}

#[test]
fn deleted_context_retries_failed_removal_and_rejects_late_snapshots() {
    let root = Fixture::new("context-cleanup");
    let store = SessionStore::open(root.0.join("monocode.db")).unwrap();
    let history = root.0.join("context-history/deleted-session");
    fs::create_dir_all(history.parent().unwrap()).unwrap();
    fs::write(&history, "A file blocks recursive directory cleanup").unwrap();
    {
        let conn = store.lock_conn().unwrap();
        upsert_session(&conn, &sample("deleted-session", "/tmp/a", "First")).unwrap();
        delete_session(&conn, "deleted-session").unwrap();
    }
    retry_context_cleanup(&store, &root.0).unwrap();
    assert_eq!(pending(&store, "deleted-session"), 1);
    fs::remove_file(&history).unwrap();
    retry_context_cleanup(&store, &root.0).unwrap();
    retry_context_cleanup(&store, &root.0).unwrap();
    assert_eq!(pending(&store, "deleted-session"), 0);
    assert!(
        session_context_snapshot(&store, &root.0, "deleted-session", "switch-1", "Late").is_err()
    );
    assert!(!history.exists());
}

fn inline(id: &str, bytes: &[u8]) -> ContextAssetSource {
    ContextAssetSource {
        id: id.into(),
        name: "image.png".into(),
        path: None,
        data: Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
    }
}

#[test]
fn deleted_session_cannot_recreate_historical_assets() {
    let fixture = Fixture::new("context-assets");
    let store = SessionStore::open_in_memory().unwrap();
    store
        .lock_conn()
        .unwrap()
        .execute(
            "INSERT INTO context_history_cleanup (session_id, pending) VALUES ('deleted-session', 0)",
            [],
        )
        .unwrap();
    assert!(
        session_context_assets(
            &store,
            &fixture.0,
            "deleted-session",
            vec![inline("late", b"Late asset")]
        )
        .is_err()
    );
    assert!(!fixture.0.join("context-history/deleted-session").exists());
}

#[test]
fn snapshots_content_by_hash_and_survives_source_deletion() {
    let fixture = Fixture::new("context-assets");
    let source_path = fixture.0.join("original.txt");
    fs::write(&source_path, b"Exact original bytes").unwrap();
    let source = ContextAssetSource {
        id: "attachment-1".into(),
        name: "original.txt".into(),
        path: Some(source_path.to_string_lossy().into_owned()),
        data: None,
    };
    let snapshot = snapshot_assets(&fixture.0, "session-1", vec![source.clone()])
        .unwrap()
        .remove(0);
    fs::remove_file(&source_path).unwrap();
    let saved_path = PathBuf::from(snapshot.path.as_ref().unwrap());
    assert_eq!(fs::read(&saved_path).unwrap(), b"Exact original bytes");
    assert_eq!(
        snapshot.sha256.as_deref(),
        Some(format!("{:x}", Sha256::digest(b"Exact original bytes")).as_str())
    );
    assert!(saved_path.starts_with(fixture.0.join("context-history/session-1/assets")));
    let recovered = snapshot_assets(&fixture.0, "session-1", vec![source])
        .unwrap()
        .remove(0);
    assert_eq!(recovered.path, snapshot.path);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&saved_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn deduplicates_identical_content_and_keeps_the_original_source() {
    let fixture = Fixture::new("context-assets");
    let first = snapshot_assets(&fixture.0, "session-1", vec![inline("a", b"same bytes")]).unwrap();
    let second =
        snapshot_assets(&fixture.0, "session-1", vec![inline("b", b"same bytes")]).unwrap();
    assert_eq!(first[0].path, second[0].path);
    assert_eq!(
        fs::read_dir(fixture.0.join("context-history/session-1/assets"))
            .unwrap()
            .count(),
        1
    );
    let changed = snapshot_assets(
        &fixture.0,
        "session-1",
        vec![inline("a", b"new original bytes")],
    )
    .unwrap();
    assert_eq!(changed[0].path, first[0].path);
}

#[test]
fn reports_missing_non_file_and_invalid_data_without_inventing_a_path() {
    let fixture = Fixture::new("context-assets");
    let sources = vec![
        ContextAssetSource {
            id: "missing".into(),
            name: "missing.png".into(),
            path: Some(fixture.0.join("missing.png").to_string_lossy().into_owned()),
            data: None,
        },
        ContextAssetSource {
            id: "directory".into(),
            name: "folder".into(),
            path: Some(fixture.0.to_string_lossy().into_owned()),
            data: None,
        },
        ContextAssetSource {
            id: "relative".into(),
            name: "relative.txt".into(),
            path: Some("relative.txt".into()),
            data: None,
        },
        ContextAssetSource {
            id: "invalid".into(),
            name: "invalid.png".into(),
            path: None,
            data: Some("not base64!".into()),
        },
    ];
    for snapshot in snapshot_assets(&fixture.0, "session-1", sources).unwrap() {
        assert!(snapshot.path.is_none());
        assert!(snapshot.sha256.is_none());
        assert!(snapshot.unavailable_reason.is_some());
    }
}

#[test]
fn rejects_traversal_session_ids_before_creating_any_files() {
    let fixture = Fixture::new("context-assets");
    assert!(snapshot_assets(&fixture.0, "../outside", vec![inline("a", b"bytes")]).is_err());
    assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
}

#[test]
fn enforces_the_file_limit_and_the_session_total() {
    let fixture = Fixture::new("context-assets");
    let large = fixture.0.join("large.bin");
    File::create(&large)
        .unwrap()
        .set_len(MAX_FILE_BYTES + 1)
        .unwrap();
    let snapshot = snapshot_assets(
        &fixture.0,
        "session-1",
        vec![ContextAssetSource {
            id: "large".into(),
            name: "large.bin".into(),
            path: Some(large.to_string_lossy().into_owned()),
            data: None,
        }],
    )
    .unwrap()
    .remove(0);
    assert!(snapshot.unavailable_reason.unwrap().contains("20 MiB"));
    let directory = fixture.0.join("context-history/session-1/assets");
    File::create(directory.join("existing.bin"))
        .unwrap()
        .set_len(MAX_SESSION_BYTES)
        .unwrap();
    let snapshot = snapshot_assets(
        &fixture.0,
        "session-1",
        vec![inline("another", b"new bytes")],
    )
    .unwrap()
    .remove(0);
    assert!(snapshot.unavailable_reason.unwrap().contains("64 MiB"));
    assert!(snapshot.path.is_none());
}

#[test]
fn refuses_to_replace_a_corrupted_immutable_asset() {
    let fixture = Fixture::new("context-assets");
    let saved = snapshot_assets(&fixture.0, "session-1", vec![inline("a", b"good bytes")])
        .unwrap()
        .remove(0);
    fs::write(saved.path.as_ref().unwrap(), b"corrupted").unwrap();
    let retried = snapshot_assets(&fixture.0, "session-1", vec![inline("b", b"good bytes")])
        .unwrap()
        .remove(0);
    assert!(retried.unavailable_reason.unwrap().contains("hash"));
    assert_eq!(fs::read(saved.path.unwrap()).unwrap(), b"corrupted");
}
