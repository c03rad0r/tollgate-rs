//! SQLite-backed channel storage with write-through cache.
//!
//! Implements the `ClientStorage` trait from cdk-spilman, persisting channel
//! state to SQLite so it survives process restarts. Uses a write-through
//! pattern: all writes go to both an in-memory cache (for fast reference-based
//! reads required by the trait) and SQLite (for durability).
//!
//! On startup, the cache is repopulated from SQLite, enabling crash recovery.

#![cfg(feature = "spilman")]

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

use cdk_spilman::{
    ClientChannelFunding, ClientChannelState, ClientPaymentState, ClientStorage,
};
use rusqlite::{params, Connection};

/// Current schema version for the channel storage database.
/// Increment when adding migrations in [`migrate`].
const SCHEMA_VERSION: i64 = 1;

/// Error type for storage operations.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("Database lock poisoned")]
    LockPoisoned,
    #[error("Migration error: {0}")]
    Migration(String),
}

/// SQLite-backed channel storage with write-through in-memory cache.
///
/// All writes are persisted to SQLite immediately. Reads are served from the
/// in-memory cache, which is loaded from SQLite on construction. This satisfies
/// the `ClientStorage` trait's `&self` -> `&T` reference contract while
/// providing durability.
pub struct SqliteChannelStorage {
    conn: Mutex<Connection>,
    funding: HashMap<String, ClientChannelFunding>,
    payments: HashMap<String, ClientPaymentState>,
    closed: HashSet<String>,
}

impl std::fmt::Debug for SqliteChannelStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteChannelStorage")
            .field("channels", &self.funding.len())
            .field("closed", &self.closed.len())
            .finish()
    }
}

impl SqliteChannelStorage {
    /// Open or create a SQLite channel storage database at the given path.
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        let conn = Connection::open(path)?;
        Self::init_schema(&conn)?;
        let mut storage = Self {
            conn: Mutex::new(conn),
            funding: HashMap::new(),
            payments: HashMap::new(),
            closed: HashSet::new(),
        };
        storage.load_from_sqlite()?;
        Ok(storage)
    }

    /// Create an in-memory SQLite database (for tests).
    pub fn open_in_memory() -> Result<Self, StorageError> {
        let conn = Connection::open_in_memory()?;
        Self::init_schema(&conn)?;
        let mut storage = Self {
            conn: Mutex::new(conn),
            funding: HashMap::new(),
            payments: HashMap::new(),
            closed: HashSet::new(),
        };
        storage.load_from_sqlite()?;
        Ok(storage)
    }

    fn init_schema(conn: &Connection) -> Result<(), StorageError> {
        // Create schema version table first (used by migrations).
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS _schema_version (
                version INTEGER NOT NULL
            );
            "#,
        )?;

        // Ensure exactly one row in _schema_version.
        let current: i64 = conn
            .query_row("SELECT COALESCE((SELECT version FROM _schema_version LIMIT 1), 0)", [], |row| row.get(0))
            .unwrap_or(0);

        if current == 0 {
            conn.execute("INSERT INTO _schema_version (version) VALUES (?1)", params![SCHEMA_VERSION])?;
        } else if current > SCHEMA_VERSION {
            return Err(StorageError::Migration(format!(
                "database schema version {current} is newer than supported {SCHEMA_VERSION}"
            )));
        } else if current < SCHEMA_VERSION {
            Self::migrate(conn, current, SCHEMA_VERSION)?;
            conn.execute("UPDATE _schema_version SET version = ?1", params![SCHEMA_VERSION])?;
        }

        // Create core tables (idempotent — IF NOT EXISTS).
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS channel_funding (
                channel_id    TEXT PRIMARY KEY,
                params_json   TEXT NOT NULL,
                funding_proofs_json TEXT NOT NULL,
                channel_secret_hex  TEXT NOT NULL,
                keyset_info_json    TEXT NOT NULL,
                sender_pubkey_hex   TEXT NOT NULL,
                capacity       INTEGER NOT NULL,
                funding_token_amount INTEGER NOT NULL,
                mint_url       TEXT NOT NULL,
                created_at     INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS channel_payments (
                channel_id    TEXT PRIMARY KEY,
                balance       INTEGER NOT NULL,
                signature     TEXT NOT NULL,
                payment_count INTEGER NOT NULL,
                last_payment_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS channel_state (
                channel_id    TEXT PRIMARY KEY,
                is_closed     INTEGER NOT NULL DEFAULT 0
            );
            "#,
        )?;
        Ok(())
    }

    /// Apply schema migrations from `from_version` to `to_version`.
    fn migrate(_conn: &Connection, _from_version: i64, _to_version: i64) -> Result<(), StorageError> {
        // Schema starts at v1 — no migrations yet.
        // When adding migrations:
        //   1. Increment SCHEMA_VERSION.
        //   2. Add a migration step here:
        //      if from_version < 2 {
        //          conn.execute_batch("ALTER TABLE ... ADD COLUMN ...")?;
        //      }
        tracing::info!(
            "Schema at version {_from_version}, target {_to_version} — no migrations needed"
        );
        Ok(())
    }

    /// Check schema health. Returns an error if the DB is at a version this
    /// binary doesn't understand.
    pub fn check_schema(conn: &Connection) -> Result<i64, StorageError> {
        let current: i64 = conn
            .query_row("SELECT version FROM _schema_version", [], |row| row.get(0))
            .map_err(|e| StorageError::Migration(format!("cannot read schema version: {e}")))?;
        if current > SCHEMA_VERSION {
            return Err(StorageError::Migration(format!(
                "database schema version {current} is newer than supported {SCHEMA_VERSION}"
            )));
        }
        if current < SCHEMA_VERSION {
            return Err(StorageError::Migration(format!(
                "database schema version {current} is behind supported {SCHEMA_VERSION}"
            )));
        }
        Ok(current)
    }

    fn load_from_sqlite(&mut self) -> Result<(), StorageError> {
        let conn = self.conn.lock().map_err(|_| StorageError::LockPoisoned)?;
        let mut stmt = conn.prepare(
            r#"SELECT channel_id, params_json, funding_proofs_json, channel_secret_hex,
                      keyset_info_json, sender_pubkey_hex, capacity,
                      funding_token_amount, mint_url, created_at
               FROM channel_funding"#,
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                ClientChannelFunding {
                    params_json: row.get(1)?,
                    funding_proofs_json: row.get(2)?,
                    channel_secret_hex: row.get(3)?,
                    keyset_info_json: row.get(4)?,
                    sender_pubkey_hex: row.get(5)?,
                    capacity: row.get(6)?,
                    funding_token_amount: row.get(7)?,
                    mint_url: row.get(8)?,
                    created_at: row.get(9)?,
                },
            ))
        })?;
        for row in rows {
            let (id, funding) = row?;
            self.funding.insert(id, funding);
        }
        let mut stmt = conn.prepare(
            r#"SELECT channel_id, balance, signature, payment_count, last_payment_at
               FROM channel_payments"#,
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                ClientPaymentState {
                    balance: row.get(1)?,
                    signature: row.get(2)?,
                    payment_count: row.get(3)?,
                    last_payment_at: row.get(4)?,
                },
            ))
        })?;
        for row in rows {
            let (id, state) = row?;
            self.payments.insert(id, state);
        }
        let mut stmt =
            conn.prepare(r#"SELECT channel_id FROM channel_state WHERE is_closed = 1"#)?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            self.closed.insert(row?);
        }
        tracing::info!(
            "Loaded {} channels from SQLite ({} closed)",
            self.funding.len(),
            self.closed.len()
        );
        Ok(())
    }

    /// Number of open channels (not closed).
    pub fn open_channel_count(&self) -> usize {
        self.funding
            .keys()
            .filter(|id| !self.closed.contains(*id))
            .count()
    }
}

impl ClientStorage for SqliteChannelStorage {
    fn save_funding(&mut self, channel_id: &str, funding: ClientChannelFunding) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                r#"INSERT OR REPLACE INTO channel_funding
                   (channel_id, params_json, funding_proofs_json, channel_secret_hex,
                    keyset_info_json, sender_pubkey_hex, capacity,
                    funding_token_amount, mint_url, created_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"#,
                params![
                    channel_id,
                    funding.params_json,
                    funding.funding_proofs_json,
                    funding.channel_secret_hex,
                    funding.keyset_info_json,
                    funding.sender_pubkey_hex,
                    funding.capacity,
                    funding.funding_token_amount,
                    funding.mint_url,
                    funding.created_at,
                ],
            );
        }
        self.funding.insert(channel_id.to_string(), funding);
    }

    fn get_funding(&self, channel_id: &str) -> Option<&ClientChannelFunding> {
        self.funding.get(channel_id)
    }

    fn get_payment_state(&self, channel_id: &str) -> Option<&ClientPaymentState> {
        self.payments.get(channel_id)
    }

    fn save_payment_state(&mut self, channel_id: &str, state: ClientPaymentState) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                r#"INSERT OR REPLACE INTO channel_payments
                   (channel_id, balance, signature, payment_count, last_payment_at)
                   VALUES (?1, ?2, ?3, ?4, ?5)"#,
                params![
                    channel_id,
                    state.balance,
                    state.signature,
                    state.payment_count,
                    state.last_payment_at,
                ],
            );
        }
        self.payments.insert(channel_id.to_string(), state);
    }

    fn get_state(&self, channel_id: &str) -> ClientChannelState {
        if self.closed.contains(channel_id) {
            ClientChannelState::Closed
        } else if self.funding.contains_key(channel_id) {
            ClientChannelState::Open
        } else {
            ClientChannelState::Closed
        }
    }

    fn set_closed(&mut self, channel_id: &str) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                r#"INSERT OR REPLACE INTO channel_state (channel_id, is_closed) VALUES (?1, 1)"#,
                params![channel_id],
            );
        }
        self.closed.insert(channel_id.to_string());
    }

    fn list_channel_ids(&self) -> Vec<String> {
        self.funding.keys().cloned().collect()
    }

    fn delete(&mut self, channel_id: &str) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                r#"DELETE FROM channel_funding WHERE channel_id = ?1"#,
                params![channel_id],
            );
            let _ = conn.execute(
                r#"DELETE FROM channel_payments WHERE channel_id = ?1"#,
                params![channel_id],
            );
            let _ = conn.execute(
                r#"DELETE FROM channel_state WHERE channel_id = ?1"#,
                params![channel_id],
            );
        }
        self.funding.remove(channel_id);
        self.payments.remove(channel_id);
        self.closed.remove(channel_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_funding(channel_id: &str) -> ClientChannelFunding {
        ClientChannelFunding {
            params_json: format!(r#"{{"channel_id": "{channel_id}"}}"#),
            funding_proofs_json: "[]".to_string(),
            channel_secret_hex: "aa".repeat(32),
            keyset_info_json: "{}".to_string(),
            sender_pubkey_hex: "02".to_string() + &"bb".repeat(32),
            capacity: 1000,
            funding_token_amount: 1100,
            mint_url: "https://mint.example.com".to_string(),
            created_at: 1234567890,
        }
    }

    fn make_test_payment(balance: u64) -> ClientPaymentState {
        ClientPaymentState {
            balance,
            signature: format!("sig_{balance}"),
            payment_count: balance / 10,
            last_payment_at: 1234567890 + balance,
        }
    }

    #[test]
    fn test_open_in_memory() {
        let storage = SqliteChannelStorage::open_in_memory().unwrap();
        assert!(storage.list_channel_ids().is_empty());
    }

    #[test]
    fn test_save_and_get_funding() {
        let mut storage = SqliteChannelStorage::open_in_memory().unwrap();
        storage.save_funding("ch1", make_test_funding("ch1"));
        assert_eq!(storage.list_channel_ids(), vec!["ch1"]);
        assert_eq!(storage.get_funding("ch1").unwrap().capacity, 1000);
    }

    #[test]
    fn test_save_and_get_payment_state() {
        let mut storage = SqliteChannelStorage::open_in_memory().unwrap();
        storage.save_funding("ch1", make_test_funding("ch1"));
        storage.save_payment_state("ch1", make_test_payment(500));
        let state = storage.get_payment_state("ch1").unwrap();
        assert_eq!(state.balance, 500);
        assert_eq!(state.payment_count, 50);
    }

    #[test]
    fn test_channel_state_lifecycle() {
        let mut storage = SqliteChannelStorage::open_in_memory().unwrap();
        storage.save_funding("ch1", make_test_funding("ch1"));
        assert_eq!(storage.get_state("ch1"), ClientChannelState::Open);
        storage.set_closed("ch1");
        assert_eq!(storage.get_state("ch1"), ClientChannelState::Closed);
    }

    #[test]
    fn test_delete_channel() {
        let mut storage = SqliteChannelStorage::open_in_memory().unwrap();
        storage.save_funding("ch1", make_test_funding("ch1"));
        storage.save_payment_state("ch1", make_test_payment(500));
        assert_eq!(storage.list_channel_ids().len(), 1);
        storage.delete("ch1");
        assert!(storage.list_channel_ids().is_empty());
        assert!(storage.get_funding("ch1").is_none());
    }

    #[test]
    fn test_persistence_across_reopen() {
        let dir = std::env::temp_dir();
        let db_path = dir.join("tollgate_test_persist.db");
        let _ = std::fs::remove_file(&db_path);
        {
            let mut storage = SqliteChannelStorage::open(&db_path).unwrap();
            storage.save_funding("ch_p", make_test_funding("ch_p"));
            storage.save_payment_state("ch_p", make_test_payment(750));
        }
        {
            let storage = SqliteChannelStorage::open(&db_path).unwrap();
            assert_eq!(storage.list_channel_ids(), vec!["ch_p"]);
            assert_eq!(storage.get_funding("ch_p").unwrap().capacity, 1000);
            assert_eq!(storage.get_payment_state("ch_p").unwrap().balance, 750);
        }
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn test_crash_recovery_simulation() {
        let dir = std::env::temp_dir();
        let db_path = dir.join("tollgate_test_crash.db");
        let _ = std::fs::remove_file(&db_path);
        {
            let mut storage = SqliteChannelStorage::open(&db_path).unwrap();
            storage.save_funding("ch_crash", make_test_funding("ch_crash"));
            storage.save_payment_state("ch_crash", make_test_payment(100));
            storage.save_payment_state("ch_crash", make_test_payment(200));
            storage.save_payment_state("ch_crash", make_test_payment(300));
        }
        {
            let storage = SqliteChannelStorage::open(&db_path).unwrap();
            assert_eq!(storage.get_state("ch_crash"), ClientChannelState::Open);
            let state = storage.get_payment_state("ch_crash").unwrap();
            assert_eq!(state.balance, 300);
            assert_eq!(state.payment_count, 30);
        }
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn test_multiple_channels() {
        let mut storage = SqliteChannelStorage::open_in_memory().unwrap();
        for i in 0..5 {
            let id = format!("ch_{i}");
            storage.save_funding(&id, make_test_funding(&id));
            storage.save_payment_state(&id, make_test_payment(i * 100));
        }
        assert_eq!(storage.list_channel_ids().len(), 5);
        assert_eq!(storage.open_channel_count(), 5);
        storage.set_closed("ch_1");
        storage.set_closed("ch_3");
        assert_eq!(storage.open_channel_count(), 3);
        assert_eq!(storage.get_state("ch_1"), ClientChannelState::Closed);
    }

    #[test]
    fn test_schema_version_on_open() {
        // Verify that opening a fresh database sets schema version to SCHEMA_VERSION.
        let storage = SqliteChannelStorage::open_in_memory().unwrap();
        let conn = storage.conn.lock().unwrap();
        let version: i64 = conn
            .query_row("SELECT version FROM _schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn test_schema_version_persists_across_reopen() {
        let dir = std::env::temp_dir();
        let db_path = dir.join("tollgate_test_schema_ver.db");
        let _ = std::fs::remove_file(&db_path);
        {
            let _storage = SqliteChannelStorage::open(&db_path).unwrap();
        }
        {
            let _storage = SqliteChannelStorage::open(&db_path).unwrap();
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let version: i64 = conn
                .query_row("SELECT version FROM _schema_version", [], |row| row.get(0))
                .unwrap();
            assert_eq!(version, SCHEMA_VERSION);
        }
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn test_check_schema_rejects_unknown_future_version() {
        let dir = std::env::temp_dir();
        let db_path = dir.join("tollgate_test_future.db");
        let _ = std::fs::remove_file(&db_path);
        {
            let _storage = SqliteChannelStorage::open(&db_path).unwrap();
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute("UPDATE _schema_version SET version = ?1", params![9999])
                .unwrap();
        }
        {
            let err = SqliteChannelStorage::open(&db_path).unwrap_err();
            assert!(
                matches!(err, StorageError::Migration(_)),
                "expected Migration error, got {err:?}"
            );
        }
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn test_spilman_service_with_persistence() {
        use cashu::nuts::SecretKey;
        use crate::spilman_service::SpilmanService;

        let dir = std::env::temp_dir();
        let db_path = dir.join("tollgate_test_spilman_svc.db");
        let _ = std::fs::remove_file(&db_path);

        let secret = SecretKey::generate();
        let svc = SpilmanService::with_persistence(
            "http://127.0.0.1:1",
            secret.clone(),
            Some(&db_path),
        )
        .expect("with_persistence should succeed");

        assert_eq!(svc.mint_url(), "http://127.0.0.1:1");
        assert_eq!(
            svc.sender_pubkey(),
            &secret.public_key().to_hex()
        );

        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn test_persistence_with_roundtrip_channels() {
        let dir = std::env::temp_dir();
        let db_path = dir.join("tollgate_test_roundtrip.db");
        let _ = std::fs::remove_file(&db_path);

        let mut storage = SqliteChannelStorage::open(&db_path).unwrap();
        for i in 0..3 {
            let id = format!("rt_{i}");
            storage.save_funding(&id, make_test_funding(&id));
            storage.save_payment_state(&id, make_test_payment(i as u64 * 100));
        }
        assert_eq!(storage.list_channel_ids().len(), 3);
        assert_eq!(storage.open_channel_count(), 3);

        storage.set_closed("rt_1");
        assert_eq!(storage.open_channel_count(), 2);

        drop(storage);
        let storage = SqliteChannelStorage::open(&db_path).unwrap();
        assert_eq!(storage.list_channel_ids().len(), 3);
        assert_eq!(storage.open_channel_count(), 2);
        assert_eq!(storage.get_state("rt_1"), ClientChannelState::Closed);
        assert_eq!(storage.get_state("rt_0"), ClientChannelState::Open);
        assert_eq!(storage.get_funding("rt_2").unwrap().capacity, 1000);
        assert_eq!(storage.get_payment_state("rt_2").unwrap().balance, 200);

        let _ = std::fs::remove_file(&db_path);
    }
}
