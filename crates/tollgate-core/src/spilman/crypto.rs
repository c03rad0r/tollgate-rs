//! Spilman cryptographic operations.

use alloc::string::String;
use alloc::vec::Vec;
use core::convert::TryFrom;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;

use tollgate_protocol::PublicKey;

/// Trait for Spilman cryptographic operations.
pub trait SpilmanCrypto: Send + Sync {
    /// Create a new channel funding signature.
    fn create_channel_funding_signature(
        &self,
        channel_id: &ChannelId,
        capacity: u64,
        peer_pubkey: &PublicKey,
    ) -> Result<Vec<u8>, SpilmanError>;

    /// Verify a channel funding signature.
    fn verify_channel_funding_signature(
        &self,
        channel_id: &ChannelId,
        capacity: u64,
        peer_pubkey: &PublicKey,
        signature: &[u8],
    ) -> Result<bool, SpilmanError>;

    /// Sign a balance update.
    fn sign_balance_update(
        &self,
        channel_id: &ChannelId,
        sequence: u64,
        amount: u64,
    ) -> Result<Vec<u8>, SpilmanError>;

    /// Verify a balance update signature.
    fn verify_balance_update(
        &self,
        channel_id: &ChannelId,
        sequence: u64,
        amount: u64,
        signature: &[u8],
        peer_pubkey: &PublicKey,
    ) -> Result<bool, SpilmanError>;

    /// Generate a random nonce for cryptographic operations.
    fn generate_nonce(&self) -> Result<Vec<u8>, SpilmanError>;
}

/// Default implementation using ed25519 signatures (ed25519-dalek v2 API).
pub struct DefaultSpilmanCrypto {
    signing_key: SigningKey,
}

impl DefaultSpilmanCrypto {
    /// Create a new crypto instance with a random keypair.
    pub fn new() -> Result<Self, SpilmanError> {
        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        Ok(Self { signing_key })
    }

    /// Create a crypto instance from an existing signing key.
    pub fn from_signing_key(signing_key: SigningKey) -> Self {
        Self { signing_key }
    }

    /// Get the verifying (public) key.
    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }
}

impl SpilmanCrypto for DefaultSpilmanCrypto {
    fn create_channel_funding_signature(
        &self,
        channel_id: &ChannelId,
        capacity: u64,
        _peer_pubkey: &PublicKey,
    ) -> Result<Vec<u8>, SpilmanError> {
        let message = build_funding_message(channel_id, capacity);
        let signature = self.signing_key.sign(&message);
        Ok(signature.to_bytes().to_vec())
    }

    fn verify_channel_funding_signature(
        &self,
        channel_id: &ChannelId,
        capacity: u64,
        peer_pubkey: &PublicKey,
        signature: &[u8],
    ) -> Result<bool, SpilmanError> {
        let message = build_funding_message(channel_id, capacity);
        let pk_bytes: &[u8; 33] = peer_pubkey.as_bytes();
        let verifying_key = VerifyingKey::from_bytes(&pk_bytes[..32].try_into().unwrap())
            .map_err(|e| SpilmanError::Crypto(format!("invalid public key: {:?}", e)))?;
        let sig = Signature::try_from(signature)
            .map_err(|e| SpilmanError::Crypto(format!("invalid signature: {:?}", e)))?;

        Ok(verifying_key.verify_strict(&message, &sig).is_ok())
    }

    fn sign_balance_update(
        &self,
        channel_id: &ChannelId,
        sequence: u64,
        amount: u64,
    ) -> Result<Vec<u8>, SpilmanError> {
        let message = build_balance_update_message(channel_id, sequence, amount);
        let signature = self.signing_key.sign(&message);
        Ok(signature.to_bytes().to_vec())
    }

    fn verify_balance_update(
        &self,
        channel_id: &ChannelId,
        sequence: u64,
        amount: u64,
        signature: &[u8],
        peer_pubkey: &PublicKey,
    ) -> Result<bool, SpilmanError> {
        let message = build_balance_update_message(channel_id, sequence, amount);
        let pk_bytes: &[u8; 33] = peer_pubkey.as_bytes();
        let verifying_key = VerifyingKey::from_bytes(&pk_bytes[..32].try_into().unwrap())
            .map_err(|e| SpilmanError::Crypto(format!("invalid public key: {:?}", e)))?;
        let sig = Signature::try_from(signature)
            .map_err(|e| SpilmanError::Crypto(format!("invalid signature: {:?}", e)))?;

        Ok(verifying_key.verify_strict(&message, &sig).is_ok())
    }

    fn generate_nonce(&self) -> Result<Vec<u8>, SpilmanError> {
        let mut nonce = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut nonce);
        Ok(nonce.to_vec())
    }
}

/// Build the message for channel funding signature.
fn build_funding_message(channel_id: &ChannelId, capacity: u64) -> [u8; 40] {
    let mut message = [0u8; 40];
    message[0..32].copy_from_slice(channel_id.as_bytes());
    message[32..40].copy_from_slice(&capacity.to_le_bytes());
    message
}

/// Build the message for balance update signature.
fn build_balance_update_message(channel_id: &ChannelId, sequence: u64, amount: u64) -> [u8; 48] {
    let mut message = [0u8; 48];
    message[0..32].copy_from_slice(channel_id.as_bytes());
    message[32..40].copy_from_slice(&sequence.to_le_bytes());
    message[40..48].copy_from_slice(&amount.to_le_bytes());
    message
}

/// Re-export types for convenience.
pub use super::channel::ChannelId;
pub use super::error::SpilmanError;

use rand::RngCore;
