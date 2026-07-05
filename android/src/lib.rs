//! tollgate-android: native Rust core for the TollGate Android app.
//!
//! Phase 4 of the TollGate RS roadmap. Exposed to Kotlin/Jetpack Compose via
//! UniFFI (proc-macro mode — no `.udl`, no `build.rs` scaffolding).
//!
//! Composition:
//!   * identity + relay transport via `nostr-sdk` (rust-nostr) — the roadmap's
//!     "nostril-native", i.e. native Rust Nostr rather than JS/NDK;
//!   * Cashu token parsing/verification via the workspace `cashu` crate (same
//!     rev `tollgate-net` uses — no version drift);
//!   * NIP-60 wallet sync (Cashu proofs as encrypted Nostr kind-7375 events);
//!   * the sans-IO `tollgate-core` state machine available for session logic.

mod error;
mod keys;
mod wallet;

pub use error::TollgateError;
pub use keys::{keypair_from_nsec, keypair_from_secret_hex, generate_keypair, KeyPair};
pub use wallet::Wallet;

uniffi::setup_scaffolding!("tollgate_android");

/// Liveness probe for the FFI boundary. Cheap to call from Kotlin on startup.
#[uniffi::export]
pub fn hello_tollgate() -> String {
    "TollGate native core online".to_string()
}

/// Crate version, surfaced to the Compose UI for the diagnostics screen.
#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Which optional subsystems were compiled in. (Today: always `nostr` + `cashu`.)
#[uniffi::export]
pub fn core_features() -> Vec<String> {
    vec!["nostr".to_string(), "cashu".to_string(), "nip60".to_string()]
}
