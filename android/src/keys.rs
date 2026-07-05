//! Nostr identity for the app. Maps the roadmap's "nostril-native" requirement
//! onto `nostr-sdk` (rust-nostr): keys live in the native Rust crypto stack,
//! never in JS/WebView.

use crate::error::TollgateError;
use nostr_sdk::nips::nip19::ToBech32;
use nostr_sdk::{Keys, SecretKey};

/// A Nostr keypair, fully materialised as bech32 + hex strings so the Kotlin
/// side never touches crypto primitives.
#[derive(uniffi::Record)]
pub struct KeyPair {
    /// 32-byte secret key, lowercase hex.
    pub secret_hex: String,
    /// Public key, lowercase hex.
    pub public_hex: String,
    /// Secret key, bech32 (`nsec1…`).
    pub nsec: String,
    /// Public key, bech32 (`npub1…`).
    pub npub: String,
}

impl KeyPair {
    /// Build the record from a rust-nostr `Keys`. Errors here are programming
    /// errors (bech32 encoding of a valid key cannot fail), so we `expect`.
    pub(crate) fn from_keys(keys: &Keys) -> Self {
        let sk = keys.secret_key();
        let pk = keys.public_key();
        KeyPair {
            secret_hex: sk.to_secret_hex(),
            public_hex: pk.to_hex(),
            nsec: sk.to_bech32().expect("valid secret key -> bech32"),
            npub: pk.to_bech32().expect("valid public key -> bech32"),
        }
    }

    // Helper kept for any future code path that needs to validate before
    // constructing — currently from_keys always succeeds.
    #[allow(dead_code)]
    pub(crate) fn try_from_keys(keys: &Keys) -> Result<Self, TollgateError> {
        let sk = keys.secret_key();
        let pk = keys.public_key();
        Ok(KeyPair {
            secret_hex: sk.to_secret_hex(),
            public_hex: pk.to_hex(),
            nsec: sk
                .to_bech32()
                .map_err(|e| TollgateError::invalid(format!("bad nsec bech32: {e}")))?,
            npub: pk
                .to_bech32()
                .map_err(|e| TollgateError::invalid(format!("bad npub bech32: {e}")))?,
        })
    }

    /// Reconstruct the rust-nostr `Keys` from the stored secret. Used by the
    /// wallet when an identity is attached.
    pub(crate) fn to_keys(&self) -> Result<Keys, TollgateError> {
        let sk = SecretKey::parse(&self.secret_hex)
            .map_err(|e| TollgateError::invalid(format!("bad secret hex: {e}")))?;
        Ok(Keys::new(sk))
    }
}

/// Generate a fresh random Nostr identity (used on first launch / "create wallet").
#[uniffi::export]
pub fn generate_keypair() -> KeyPair {
    let keys = Keys::generate();
    KeyPair::from_keys(&keys)
}

/// Restore an identity from an `nsec1…` string.
#[uniffi::export]
pub fn keypair_from_nsec(nsec: String) -> Result<KeyPair, TollgateError> {
    let sk = SecretKey::parse(nsec.trim())
        .map_err(|e| TollgateError::invalid(format!("bad nsec: {e}")))?;
    let keys = Keys::new(sk);
    Ok(KeyPair::from_keys(&keys))
}

/// Restore an identity from a 32-byte hex secret key.
#[uniffi::export]
pub fn keypair_from_secret_hex(hex_str: String) -> Result<KeyPair, TollgateError> {
    let sk = SecretKey::parse(hex_str.trim())
        .map_err(|e| TollgateError::invalid(format!("bad secret hex: {e}")))?;
    let keys = Keys::new(sk);
    Ok(KeyPair::from_keys(&keys))
}
