//! Spilman channel types and structures.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::time::Duration;

use tollgate_protocol::PublicKey;

/// Unique identifier for a Spilman channel.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ChannelId([u8; 32]);

impl ChannelId {
    /// Create a new random channel ID.
    pub fn new() -> Self {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        ChannelId(bytes)
    }

    /// Create a channel ID from bytes.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        ChannelId(bytes)
    }

    /// Get the channel ID as bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// The current state of a Spilman channel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChannelState {
    /// Channel has been proposed but not yet funded.
    Init,
    /// Channel is funded and ready for use.
    Funded,
    /// Channel is active and balance updates can flow.
    Active,
    /// Channel is being closed cooperatively.
    Closing,
    /// Channel has been closed and settled.
    Closed,
    /// Channel expired due to timeout.
    Expired,
}

/// Reason for channel closure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CloseReason {
    /// Cooperative close agreed by both parties.
    Cooperative,
    /// Unilateral close due to timeout or dispute.
    Unilateral,
    /// Channel expired.
    Expired,
}

/// Represents a Spilman payment channel between two peers.
#[derive(Clone, Debug)]
pub struct SpilmanChannel {
    /// Unique identifier for this channel.
    pub id: ChannelId,
    /// The peer we're connected to.
    pub peer_pubkey: PublicKey,
    /// Current state of the channel.
    pub state: ChannelState,
    /// Channel capacity in milli-units.
    pub capacity: u64,
    /// Current balance on our side (what we can spend).
    pub our_balance: u64,
    /// Current balance on the peer's side.
    pub peer_balance: u64,
    /// When the channel was created.
    pub created_at: Duration,
    /// When the channel expires (for timeout enforcement).
    pub expires_at: Duration,
    /// Sequence number for the last balance update.
    pub sequence: u64,
    /// Pending balance updates that haven't been acknowledged.
    pub pending_updates: Vec<PendingUpdate>,
}

/// A pending balance update awaiting acknowledgment.
#[derive(Clone, Debug)]
pub struct PendingUpdate {
    /// Sequence number of this update.
    pub sequence: u64,
    /// Amount being transferred.
    pub amount: u64,
    /// When this update was sent.
    pub sent_at: Duration,
}

impl SpilmanChannel {
    /// Create a new Spilman channel.
    pub fn new(
        id: ChannelId,
        peer_pubkey: PublicKey,
        capacity: u64,
        expires_at: Duration,
    ) -> Self {
        Self {
            id,
            peer_pubkey,
            state: ChannelState::Init,
            capacity,
            our_balance: capacity,
            peer_balance: 0,
            created_at: Duration::from_millis(0),
            expires_at,
            sequence: 0,
            pending_updates: Vec::new(),
        }
    }

    /// Check if the channel is active and can be used for payments.
    pub fn is_active(&self) -> bool {
        self.state == ChannelState::Active
    }

    /// Check if the channel has sufficient balance for a payment.
    pub fn has_balance(&self, amount: u64) -> bool {
        self.our_balance >= amount
    }

    /// Update our balance (after sending a payment).
    pub fn update_our_balance(&mut self, amount: u64) -> Result<(), ChannelError> {
        if !self.is_active() {
            return Err(ChannelError::NotActive);
        }
        if self.our_balance < amount {
            return Err(ChannelError::InsufficientBalance);
        }
        
        self.our_balance -= amount;
        self.sequence += 1;
        Ok(())
    }

    /// Update peer's balance (after receiving a payment).
    pub fn update_peer_balance(&mut self, amount: u64) -> Result<(), ChannelError> {
        if !self.is_active() {
            return Err(ChannelError::NotActive);
        }
        
        self.our_balance += amount;
        Ok(())
    }

    /// Move channel to funded state.
    pub fn fund(&mut self) -> Result<(), ChannelError> {
        if self.state != ChannelState::Init {
            return Err(ChannelError::InvalidStateTransition);
        }
        
        self.state = ChannelState::Funded;
        Ok(())
    }

    /// Activate the channel (ready for balance updates).
    pub fn activate(&mut self) -> Result<(), ChannelError> {
        if self.state != ChannelState::Funded {
            return Err(ChannelError::InvalidStateTransition);
        }
        
        self.state = ChannelState::Active;
        Ok(())
    }

    /// Start cooperative close.
    pub fn start_cooperative_close(&mut self) -> Result<(), ChannelError> {
        if self.state != ChannelState::Active {
            return Err(ChannelError::NotActive);
        }
        
        self.state = ChannelState::Closing;
        Ok(())
    }

    /// Complete channel close.
    pub fn close(&mut self, reason: CloseReason) -> Result<(), ChannelError> {
        if !matches!(self.state, ChannelState::Closing | ChannelState::Active) {
            return Err(ChannelError::InvalidStateTransition);
        }
        
        self.state = ChannelState::Closed;
        Ok(())
    }
}

/// Channel-specific errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelError {
    /// Channel is not in active state.
    NotActive,
    /// Insufficient balance for operation.
    InsufficientBalance,
    /// Invalid state transition.
    InvalidStateTransition,
    /// Channel has expired.
    Expired,
}

impl core::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ChannelError::NotActive => write!(f, "channel is not active"),
            ChannelError::InsufficientBalance => write!(f, "insufficient balance"),
            ChannelError::InvalidStateTransition => write!(f, "invalid state transition"),
            ChannelError::Expired => write!(f, "channel has expired"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ChannelError {}