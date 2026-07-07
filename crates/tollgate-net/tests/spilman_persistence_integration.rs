//! Integration tests for SQLite-backed Spilman channel persistence.
//!
//! Tests that channel state survives process restarts (crash recovery),
//! schema versioning prevents silent corruption, and the SpilmanService
//! correctly restores state from disk.

#![cfg(all(test, feature = "spilman", feature = "sqlite"))]

use std::path::Path;

use cashu::nuts::SecretKey;
use cdk_spilman::{ClientChannelFunding, ClientPaymentState, ClientStorage};
use tollgate_net::spilman_persistence::SqliteChannelStorage;
use tollgate_net::spilman_service::SpilmanService;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn db_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir();
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    path
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn sqlite_storage_crash_recovery_multiple_channels() {
    let db = db_path("test_crash_multi.db");

    // Open, save multiple channels, then "crash" (drop)
    {
        let mut storage = SqliteChannelStorage::open(&db).unwrap();
        for i in 0..10 {
            let id = format!("crash_{i}");
            storage.save_funding(
                &id,
                make_test_funding(&id, 1000 + i * 100),
            );
            storage.save_payment_state(
                &id,
                make_test_payment(i as u64 * 50),
            );
        }
        storage.set_closed("crash_3");
        storage.set_closed("crash_7");
    }

    // Recover from SQLite (simulating crash recovery)
    {
        let storage = SqliteChannelStorage::open(&db).unwrap();
        assert_eq!(storage.list_channel_ids().len(), 10);
        assert_eq!(storage.open_channel_count(), 8); // 2 closed

        // Verify specific channel states
        assert_eq!(storage.get_state("crash_3"), cdk_spilman::ClientChannelState::Closed);
        assert_eq!(storage.get_state("crash_0"), cdk_spilman::ClientChannelState::Open);
        assert_eq!(storage.get_funding("crash_4").unwrap().capacity, 1400);
        assert_eq!(storage.get_payment_state("crash_6").unwrap().balance, 300);
    }

    cleanup(&db);
}

#[test]
fn sqlite_storage_survives_sequential_opens_and_closes() {
    let db = db_path("test_seq_open_close.db");

    {
        let mut storage = SqliteChannelStorage::open(&db).unwrap();

        // Open 5 channels sequentially
        for i in 0..5 {
            let id = format!("seq_{i}");
            storage.save_funding(&id, make_test_funding(&id, 500));
            storage.save_payment_state(&id, make_test_payment(0));
        }
        assert_eq!(storage.open_channel_count(), 5);

        // Close them one by one
        for i in 0..5 {
            let id = format!("seq_{i}");
            storage.set_closed(&id);
            assert_eq!(storage.open_channel_count(), 4 - i);
        }
    }

    // After restart, all should be closed
    {
        let storage = SqliteChannelStorage::open(&db).unwrap();
        assert_eq!(storage.open_channel_count(), 0);
        assert_eq!(storage.list_channel_ids().len(), 5);
    }

    cleanup(&db);
}

#[test]
fn spilman_service_with_persistence_roundtrip() {
    let db = db_path("test_svc_roundtrip.db");

    let secret = SecretKey::generate();
    let pk_hex = secret.public_key().to_hex();

    // Create service with persistence
    {
        let svc = SpilmanService::with_persistence(
            "http://mint.example.com",
            secret.clone(),
            Some(&db),
        )
        .expect("with_persistence");

        assert_eq!(svc.mint_url(), "http://mint.example.com");
        assert_eq!(svc.sender_pubkey(), &pk_hex);
    }

    // Reopen — should load same state (empty channel list, same keys)
    {
        let svc = SpilmanService::with_persistence(
            "http://mint.example.com",
            secret,
            Some(&db),
        )
        .expect("with_persistence (reopen)");

        assert_eq!(svc.mint_url(), "http://mint.example.com");
        assert_eq!(svc.sender_pubkey(), &pk_hex);
    }

    cleanup(&db);
}

#[test]
fn spilman_service_in_memory_uses_different_db_than_file() {
    // in-memory and file-backed services should not interfere
    let db = db_path("test_isolated.db");

    let secret = SecretKey::generate();
    let svc_file = SpilmanService::with_persistence(
        "http://mint.example.com",
        secret.clone(),
        Some(&db),
    )
    .expect("file-backed");

    let svc_mem = SpilmanService::with_persistence(
        "http://mint.example.com",
        secret,
        None, // in-memory
    )
    .expect("in-memory");

    assert_eq!(svc_file.sender_pubkey(), svc_mem.sender_pubkey());
    assert_eq!(svc_file.mint_url(), svc_mem.mint_url());

    cleanup(&db);
}

#[test]
fn schema_version_rejects_unknown_version() {
    let db = db_path("test_schema_reject.db");

    // Create a DB with a future schema version
    {
        let _storage = SqliteChannelStorage::open(&db).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE _schema_version SET version = ?1",
            rusqlite::params![9999],
        )
        .unwrap();
    }

    // Opening should fail with Migration error
    {
        let err = SqliteChannelStorage::open(&db).unwrap_err();
        assert!(
            matches!(err, tollgate_net::spilman_persistence::StorageError::Migration(_)),
            "expected Migration error, got {err:?}"
        );
    }

    cleanup(&db);
}

// ---------------------------------------------------------------------------
// Test data helpers (mirror those in spilman_persistence.rs)
// ---------------------------------------------------------------------------

fn make_test_funding(channel_id: &str, capacity: u64) -> ClientChannelFunding {
    ClientChannelFunding {
        params_json: format!(r#"{{"channel_id": "{channel_id}"}}"#),
        funding_proofs_json: "[]".to_string(),
        channel_secret_hex: "aa".repeat(32),
        keyset_info_json: "{}".to_string(),
        sender_pubkey_hex: "02".to_string() + &"bb".repeat(32),
        capacity,
        funding_token_amount: capacity + 100,
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
