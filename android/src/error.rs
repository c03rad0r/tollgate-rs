//! FFI error type. UniFFI requires a single error enum at the boundary; internal
//! `anyhow::Error`s are flattened to a detail + kind here.

use std::fmt;

#[derive(Debug, uniffi::Error)]
pub enum TollgateError {
    /// A bad key, token, or argument from the Kotlin side.
    InvalidInput { detail: String },
    /// Nostr relay / transport failure.
    Nostr { detail: String },
    /// Cashu mint rejected or could not be reached.
    Mint { detail: String },
    /// Wallet has no identity set, or is not connected.
    NotReady { detail: String },
    /// Catch-all.
    Internal { detail: String },
}

impl fmt::Display for TollgateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TollgateError::InvalidInput { detail } => write!(f, "invalid input: {detail}"),
            TollgateError::Nostr { detail } => write!(f, "nostr: {detail}"),
            TollgateError::Mint { detail } => write!(f, "mint: {detail}"),
            TollgateError::NotReady { detail } => write!(f, "not ready: {detail}"),
            TollgateError::Internal { detail } => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for TollgateError {}

impl From<anyhow::Error> for TollgateError {
    fn from(e: anyhow::Error) -> Self {
        TollgateError::Internal {
            detail: e.to_string(),
        }
    }
}

/// Convenience constructors used throughout the crate.
impl TollgateError {
    pub fn invalid(msg: impl Into<String>) -> Self {
        TollgateError::InvalidInput {
            detail: msg.into(),
        }
    }
    pub fn nostr(msg: impl Into<String>) -> Self {
        TollgateError::Nostr {
            detail: msg.into(),
        }
    }
    pub fn mint(msg: impl Into<String>) -> Self {
        TollgateError::Mint { detail: msg.into() }
    }
    pub fn not_ready(msg: impl Into<String>) -> Self {
        TollgateError::NotReady {
            detail: msg.into(),
        }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        TollgateError::Internal {
            detail: msg.into(),
        }
    }
}
