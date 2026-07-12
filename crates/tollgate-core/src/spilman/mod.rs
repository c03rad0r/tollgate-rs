//! Spilman payment channel implementation.
//!
//! This module provides the core Spilman channel functionality for unidirectional
//! payment channels built on Cashu ecash. Spilman channels allow streaming
//! micropayments without requiring on-chain transactions for each payment.
//!
//! ## Status
//!
//! - `channel` — implemented: channel state machine, balance tracking, close logic
//! - `crypto` — implemented: ed25519 signatures for funding + balance updates
//! - `error` — implemented: comprehensive error types
//! - `bridge` — planned: Cashu mint integration layer
//! - `state_machine` — planned: protocol-level state transitions (re-exports from channel for now)
//! - `storage` — planned: channel persistence and recovery

pub mod channel;
pub mod crypto;
pub mod error;

// Planned modules — not yet implemented:
// pub mod bridge;
// pub mod state_machine;
// pub mod storage;

pub use channel::{ChannelId, ChannelState, CloseReason, SpilmanChannel, ChannelError};
pub use crypto::{SpilmanCrypto, DefaultSpilmanCrypto};
pub use error::SpilmanError;

// Planned re-exports (currently in channel module):
// pub use bridge::SpilmanBridge;
// pub use state_machine::{ChannelState, CloseReason};
// pub use storage::SpilmanStorage;

/// Re-export commonly used types from the protocol crate.
pub use tollgate_protocol::PublicKey;
