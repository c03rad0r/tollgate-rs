#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::Mutex;

use super::CustomerSession;

#[derive(Debug, thiserror::Error)]
pub enum SessionStoreError {
    #[error("sqlite error: {0}")]
    #[cfg(feature = "sqlite")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn get(&self, mac: &str) -> Result<Option<CustomerSession>, SessionStoreError>;
    async fn insert(&self, session: CustomerSession) -> Result<(), SessionStoreError>;
    async fn remove(&self, mac: &str) -> Result<Option<CustomerSession>, SessionStoreError>;
    async fn update(&self, mac: &str, session: CustomerSession) -> Result<(), SessionStoreError>;
    async fn list_all(&self) -> Result<Vec<CustomerSession>, SessionStoreError>;
    /// Find sessions whose `allotment` (interpreted as milliseconds when
    /// `metric == "milliseconds"`) has been fully consumed by time elapsed
    /// since `start_time`.
    async fn list_expired(&self, now_secs: i64) -> Result<Vec<CustomerSession>, SessionStoreError>;
}

#[derive(Default)]
pub struct InMemorySessionStore {
    map: Mutex<HashMap<String, CustomerSession>>,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SessionStore for InMemorySessionStore {
    async fn get(&self, mac: &str) -> Result<Option<CustomerSession>, SessionStoreError> {
        let map = self.map.lock().await;
        Ok(map.get(mac).cloned())
    }

    async fn insert(&self, session: CustomerSession) -> Result<(), SessionStoreError> {
        let mut map = self.map.lock().await;
        map.insert(session.mac_address.clone(), session);
        Ok(())
    }

    async fn remove(&self, mac: &str) -> Result<Option<CustomerSession>, SessionStoreError> {
        let mut map = self.map.lock().await;
        Ok(map.remove(mac))
    }

    async fn update(&self, mac: &str, session: CustomerSession) -> Result<(), SessionStoreError> {
        let mut map = self.map.lock().await;
        map.insert(mac.to_owned(), session);
        Ok(())
    }

    async fn list_all(&self) -> Result<Vec<CustomerSession>, SessionStoreError> {
        let map = self.map.lock().await;
        Ok(map.values().cloned().collect())
    }

    async fn list_expired(&self, now_secs: i64) -> Result<Vec<CustomerSession>, SessionStoreError> {
        let map = self.map.lock().await;
        let expired: Vec<CustomerSession> = map
            .values()
            .filter(|s| {
                if s.metric == "milliseconds" {
                    let elapsed_ms = (now_secs - s.start_time) * 1000;
                    elapsed_ms >= s.allotment as i64
                } else {
                    false
                }
            })
            .cloned()
            .collect();
        Ok(expired)
    }
}

#[cfg(feature = "sqlite")]
pub struct SqliteSessionStore {
    conn: Mutex<rusqlite::Connection>,
}

#[cfg(feature = "sqlite")]
impl SqliteSessionStore {
    const CREATE_TABLE_SQL: &str = "\
        CREATE TABLE IF NOT EXISTS sessions (\
            mac_address TEXT PRIMARY KEY, \
            start_time  INTEGER NOT NULL, \
            metric      TEXT    NOT NULL, \
            allotment   INTEGER NOT NULL\
        )";

    fn init_tables(conn: &rusqlite::Connection) -> Result<(), SessionStoreError> {
        conn.execute(Self::CREATE_TABLE_SQL, [])?;
        Ok(())
    }

    /// Open (or create) a SQLite database at `path`.
    pub fn open(path: &str) -> Result<Self, SessionStoreError> {
        let conn = rusqlite::Connection::open(path)?;
        Self::init_tables(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Open an in-memory database (useful for tests).
    pub fn open_in_memory() -> Result<Self, SessionStoreError> {
        let conn = rusqlite::Connection::open_in_memory()?;
        Self::init_tables(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<CustomerSession> {
        Ok(CustomerSession {
            mac_address: row.get(0)?,
            start_time: row.get(1)?,
            metric: row.get(2)?,
            allotment: row.get(3)?,
            last_external_usage: None,
        })
    }
}

#[cfg(feature = "sqlite")]
#[async_trait]
impl SessionStore for SqliteSessionStore {
    async fn get(&self, mac: &str) -> Result<Option<CustomerSession>, SessionStoreError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare("SELECT mac_address, start_time, metric, allotment FROM sessions WHERE mac_address = ?1")?;
        let session = stmt
            .query_row(rusqlite::params![mac], Self::row_to_session)
            .ok();
        Ok(session)
    }

    async fn insert(&self, session: CustomerSession) -> Result<(), SessionStoreError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO sessions (mac_address, start_time, metric, allotment) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                session.mac_address,
                session.start_time,
                session.metric,
                session.allotment,
            ],
        )?;
        Ok(())
    }

    async fn remove(&self, mac: &str) -> Result<Option<CustomerSession>, SessionStoreError> {
        let existing = self.get(mac).await?;
        if existing.is_some() {
            let conn = self.conn.lock().await;
            conn.execute(
                "DELETE FROM sessions WHERE mac_address = ?1",
                rusqlite::params![mac],
            )?;
        }
        Ok(existing)
    }

    async fn update(&self, mac: &str, session: CustomerSession) -> Result<(), SessionStoreError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE sessions SET start_time = ?1, metric = ?2, allotment = ?3 WHERE mac_address = ?4",
            rusqlite::params![
                session.start_time,
                session.metric,
                session.allotment,
                mac,
            ],
        )?;
        Ok(())
    }

    async fn list_all(&self) -> Result<Vec<CustomerSession>, SessionStoreError> {
        let conn = self.conn.lock().await;
        let mut stmt =
            conn.prepare("SELECT mac_address, start_time, metric, allotment FROM sessions")?;
        let sessions = stmt
            .query_map([], Self::row_to_session)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(sessions)
    }

    async fn list_expired(&self, now_secs: i64) -> Result<Vec<CustomerSession>, SessionStoreError> {
        // Elapsed in ms = (now_secs - start_time) * 1000
        // Expired when: elapsed_ms >= allotment
        // ⟹  (now_secs - start_time) * 1000 >= allotment
        // ⟹  now_secs - start_time >= allotment / 1000
        // We keep the multiplication to avoid integer-division edge-cases.
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT mac_address, start_time, metric, allotment FROM sessions \
             WHERE metric = 'milliseconds' AND (?1 - start_time) * 1000 >= allotment",
        )?;
        let sessions = stmt
            .query_map(rusqlite::params![now_secs], Self::row_to_session)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(sessions)
    }
}

#[cfg(feature = "sqlite")]
#[cfg(test)]
mod tests {
    use super::*;

    fn make_session(mac: &str, start: i64, metric: &str, allotment: u64) -> CustomerSession {
        CustomerSession {
            mac_address: mac.to_owned(),
            start_time: start,
            metric: metric.to_owned(),
            allotment,
            last_external_usage: None,
        }
    }

    #[tokio::test]
    async fn sqlite_create_table_on_open() {
        let store = SqliteSessionStore::open_in_memory().unwrap();
        let all = store.list_all().await.unwrap();
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn sqlite_insert_and_get() {
        let store = SqliteSessionStore::open_in_memory().unwrap();
        let session = make_session("aa:bb:cc:dd:ee:ff", 1000, "milliseconds", 60_000);
        store.insert(session.clone()).await.unwrap();

        let got = store.get("aa:bb:cc:dd:ee:ff").await.unwrap();
        assert_eq!(got, Some(session));

        let missing = store.get("00:00:00:00:00:00").await.unwrap();
        assert_eq!(missing, None);
    }

    #[tokio::test]
    async fn sqlite_remove() {
        let store = SqliteSessionStore::open_in_memory().unwrap();
        let session = make_session("aa:bb:cc:dd:ee:ff", 1000, "milliseconds", 60_000);
        store.insert(session.clone()).await.unwrap();

        let removed = store.remove("aa:bb:cc:dd:ee:ff").await.unwrap();
        assert_eq!(removed, Some(session));

        let gone = store.get("aa:bb:cc:dd:ee:ff").await.unwrap();
        assert_eq!(gone, None);

        let remove_again = store.remove("aa:bb:cc:dd:ee:ff").await.unwrap();
        assert_eq!(remove_again, None);
    }

    #[tokio::test]
    async fn sqlite_update() {
        let store = SqliteSessionStore::open_in_memory().unwrap();
        let session = make_session("aa:bb:cc:dd:ee:ff", 1000, "milliseconds", 60_000);
        store.insert(session).await.unwrap();

        let updated = make_session("aa:bb:cc:dd:ee:ff", 2000, "milliseconds", 120_000);
        store
            .update("aa:bb:cc:dd:ee:ff", updated.clone())
            .await
            .unwrap();

        let got = store.get("aa:bb:cc:dd:ee:ff").await.unwrap();
        assert_eq!(got, Some(updated));
    }

    #[tokio::test]
    async fn sqlite_list_expired() {
        let store = SqliteSessionStore::open_in_memory().unwrap();

        let s1 = make_session("aa:bb:cc:dd:ee:ff", 100, "milliseconds", 60_000);
        store.insert(s1).await.unwrap();

        let s2 = make_session("11:22:33:44:55:66", 200, "milliseconds", 30_000);
        store.insert(s2).await.unwrap();

        let s3 = make_session("99:88:77:66:55:44", 100, "bytes", 1000);
        store.insert(s3).await.unwrap();

        let expired = store.list_expired(250).await.unwrap();
        assert_eq!(expired.len(), 2);

        let not_yet = store.list_expired(150).await.unwrap();
        assert_eq!(not_yet.len(), 0);
    }

    #[tokio::test]
    async fn sqlite_list_all() {
        let store = SqliteSessionStore::open_in_memory().unwrap();
        assert!(store.list_all().await.unwrap().is_empty());

        let s1 = make_session("aa:bb:cc:dd:ee:ff", 100, "milliseconds", 60_000);
        let s2 = make_session("11:22:33:44:55:66", 200, "milliseconds", 30_000);
        store.insert(s1.clone()).await.unwrap();
        store.insert(s2.clone()).await.unwrap();

        let mut all = store.list_all().await.unwrap();
        all.sort_by(|a, b| a.mac_address.cmp(&b.mac_address));
        assert_eq!(all, vec![s2, s1]);
    }
}
