//! Spilman error types.

use alloc::string::String;
use core::fmt;

/// Comprehensive error type for Spilman operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpilmanError {
    /// Channel-specific error.
    Channel(ChannelError),
    /// Crypto operation failed.
    Crypto(String),
    /// Storage operation failed.
    Storage(String),
    /// Bridge operation failed.
    Bridge(String),
    /// Invalid message received.
    InvalidMessage(String),
    /// Protocol error.
    Protocol(String),
    /// Channel not found.
    ChannelNotFound(ChannelId),
    /// Invalid state transition.
    InvalidStateTransition { current: ChannelState, target: ChannelState },
    /// Channel has expired.
    ChannelExpired,
    /// Insufficient balance.
    InsufficientBalance { required: u64, available: u64 },
    /// Peer disconnected.
    PeerDisconnected,
    /// Internal error.
    Internal(String),
}

impl fmt::Display for SpilmanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpilmanError::Channel(e) => write!(f, "channel error: {}", e),
            SpilmanError::Crypto(msg) => write!(f, "crypto error: {}", msg),
            SpilmanError::Storage(msg) => write!(f, "storage error: {}", msg),
            SpilmanError::Bridge(msg) => write!(f, "bridge error: {}", msg),
            SpilmanError::InvalidMessage(msg) => write!(f, "invalid message: {}", msg),
            SpilmanError::Protocol(msg) => write!(f, "protocol error: {}", msg),
            SpilmanError::ChannelNotFound(id) => write!(f, "channel not found: {:?}", id),
            SpilmanError::InvalidStateTransition { current, target } => {
                write!(f, "invalid state transition: {:?} -> {:?}", current, target)
            }
            SpilmanError::ChannelExpired => write!(f, "channel has expired"),
            SpilmanError::InsufficientBalance { required, available } => {
                write!(f, "insufficient balance: required {}, available {}", required, available)
            }
            SpilmanError::PeerDisconnected => write!(f, "peer disconnected"),
            SpilmanError::Internal(msg) => write!(f, "internal error: {}", msg),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for SpilmanError {}

impl From<ChannelError> for SpilmanError {
    fn from(err: ChannelError) -> Self {
        SpilmanError::Channel(err)
    }
}

/// Re-export channel types for convenience.
pub use super::channel::{ChannelError, ChannelId, ChannelState, CloseReason};