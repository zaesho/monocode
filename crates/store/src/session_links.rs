//! Links between two sessions. A linked pair can read and message each
//! other through the app CLI without `/operator`. Each row stores the pair
//! once, with the smaller id first.

use rusqlite::{Connection, params};

use crate::session_store::{SessionStore, now_millis, validate_id};

/// Create the table. Safe to run on every open, so a database written by an
/// older build gains it without a recorded migration.
pub fn ensure_table(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_links (
           session_a TEXT NOT NULL,
           session_b TEXT NOT NULL,
           created_at INTEGER NOT NULL,
           PRIMARY KEY (session_a, session_b)
         );
         CREATE INDEX IF NOT EXISTS session_links_b_idx ON session_links (session_b);",
    )
}

/// The pair in stored order.
pub fn ordered<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if a <= b { (a, b) } else { (b, a) }
}

fn checked<'a>(a: &'a str, b: &'a str) -> Result<(&'a str, &'a str), String> {
    validate_id(a, "session")?;
    validate_id(b, "session")?;
    if a == b {
        return Err("A session cannot be linked to itself".into());
    }
    Ok(ordered(a, b))
}

/// Every stored link, each pair once in stored order.
pub fn list_links(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    conn.prepare("SELECT session_a, session_b FROM session_links ORDER BY created_at, session_a")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect()
}

/// Remove every link that names the session.
pub fn forget_session(conn: &Connection, session_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM session_links WHERE session_a = ?1 OR session_b = ?1",
        [session_id],
    )?;
    Ok(())
}

pub fn session_list_links(store: &SessionStore) -> Result<Vec<(String, String)>, String> {
    let conn = store.lock_conn()?;
    list_links(&conn).map_err(|e| e.to_string())
}

/// Add or remove the link between two sessions. Adding an existing link or
/// removing a missing one does nothing.
pub fn session_set_link(
    store: &SessionStore,
    a: String,
    b: String,
    linked: bool,
) -> Result<(), String> {
    let (first, second) = checked(&a, &b)?;
    let conn = store.lock_conn()?;
    if linked {
        conn.execute(
            "INSERT OR IGNORE INTO session_links (session_a, session_b, created_at)
             VALUES (?1, ?2, ?3)",
            params![first, second, now_millis()],
        )
    } else {
        conn.execute(
            "DELETE FROM session_links WHERE session_a = ?1 AND session_b = ?2",
            params![first, second],
        )
    }
    .map(|_| ())
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_each_pair_once_in_either_order() {
        let store = SessionStore::open_in_memory().unwrap();
        session_set_link(&store, "b".into(), "a".into(), true).unwrap();
        session_set_link(&store, "a".into(), "b".into(), true).unwrap();
        session_set_link(&store, "a".into(), "c".into(), true).unwrap();
        assert_eq!(
            session_list_links(&store).unwrap(),
            vec![("a".to_string(), "b".to_string()), ("a".into(), "c".into())]
        );
        session_set_link(&store, "b".into(), "a".into(), false).unwrap();
        assert_eq!(
            session_list_links(&store).unwrap(),
            vec![("a".to_string(), "c".to_string())]
        );
    }

    #[test]
    fn rejects_a_self_link_and_invalid_ids() {
        let store = SessionStore::open_in_memory().unwrap();
        assert!(session_set_link(&store, "a".into(), "a".into(), true).is_err());
        assert!(session_set_link(&store, "a".into(), "../b".into(), true).is_err());
    }

    #[test]
    fn deleting_a_session_drops_its_links() {
        let store = SessionStore::open_in_memory().unwrap();
        session_set_link(&store, "a".into(), "b".into(), true).unwrap();
        session_set_link(&store, "c".into(), "d".into(), true).unwrap();
        forget_session(&store.lock_conn().unwrap(), "b").unwrap();
        assert_eq!(
            session_list_links(&store).unwrap(),
            vec![("c".to_string(), "d".to_string())]
        );
    }

    #[test]
    fn an_older_database_without_the_table_still_opens() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE unrelated (id TEXT)")
            .unwrap();
        ensure_table(&conn).unwrap();
        ensure_table(&conn).unwrap();
        assert!(list_links(&conn).unwrap().is_empty());
    }
}
