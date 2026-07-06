//! Spilman channel client wrapper.
//!
//! [`SpilmanService`] is the buyer-side facade over `cdk_spilman`'s
//! `SpilmanClientBridge`. This module owns the typed [`SpilmanError`] surface and
//! the settlement-DLEQ verifier ([`verify_settlement_proofs_dleq`]).
//!
//! NOTE: the upstream `cdk_spilman` traits (`SpilmanClientAsyncNetworking`,
//! `SpilmanHost`, ...) are still typed `Result<_, String>` — those signatures are
//! fixed by the crate and cannot be changed downstream. [`SpilmanError`] types
//! *this* wrapper's public API; bridge/trait `String` errors are folded into
//! [`SpilmanError::Bridge`].

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use cashu::nuts::{Proof as CashuProof, SecretKey};
use cdk_spilman::{
    ConfigurableClientHost, KeysetInfo, MemoryClientStorage, SpilmanClientAsyncNetworking,
    SpilmanClientBridge, SpilmanClientNetworking,
};
use thiserror::Error;

pub use cdk_spilman::{
    ClientChannelInfo, CloseSuccess, OpenChannelResult, Payment, PaymentProof, PaymentSuccess,
    SpilmanAsyncNetworking, SpilmanBridge, SpilmanHost,
};

// ---------------------------------------------------------------------------
// Typed error
// ---------------------------------------------------------------------------

/// Typed errors for the Spilman wrapper API.
///
/// Replaces the former `Result<_, String>` surface of [`SpilmanService`] and
/// [`crate::spilman_wallet::fetch_active_keyset_info`].
#[derive(Debug, Error)]
pub enum SpilmanError {
    /// A reqwest HTTP transport failure (connection, DNS, timeout, body read).
    #[error("network error: {0}")]
    Network(String),

    /// The mint returned a non-success HTTP status.
    #[error("mint returned HTTP {status}: {body}")]
    MintStatus {
        /// HTTP status code returned by the mint.
        status: u16,
        /// Response body (truncated by the mint).
        body: String,
    },

    /// A response body could not be parsed (JSON/schema mismatch).
    #[error("invalid response: {0}")]
    InvalidResponse(String),

    /// Keyset lookup/parsing failure (no active sat keyset, missing fields, ...).
    #[error("keyset error: {0}")]
    Keyset(String),

    /// An error propagated from the `cdk_spilman` bridge (which returns `String`).
    #[error("channel operation failed: {0}")]
    Bridge(String),

    /// DLEQ proof verification failed on funding or settlement proofs.
    #[error("DLEQ verification failed: {0}")]
    DleqVerification(String),

    /// (De)serialization failure.
    #[error("serialization error: {0}")]
    Serialization(String),
}

impl SpilmanError {
    /// Returns `true` if this is a DLEQ verification failure.
    #[must_use]
    pub fn is_dleq(&self) -> bool {
        matches!(self, Self::DleqVerification(_))
    }
}

impl From<String> for SpilmanError {
    /// Fold a `cdk_spilman` bridge/trait `String` error into [`SpilmanError::Bridge`].
    fn from(s: String) -> Self {
        Self::Bridge(s)
    }
}

impl From<serde_json::Error> for SpilmanError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serialization(e.to_string())
    }
}

impl From<reqwest::Error> for SpilmanError {
    fn from(e: reqwest::Error) -> Self {
        Self::Network(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Settlement DLEQ verification
// ---------------------------------------------------------------------------

/// Verify DLEQ proofs on channel settlement/close outputs.
///
/// After a cooperative or unilateral close, the mint returns `sender_proofs`
/// (a JSON array of Cashu [`Proof`]s) that the receiver must validate before
/// trusting. For each proof this checks that:
///   - a DLEQ proof is present, and
///   - `proof.verify_dleq(mint_pubkey)` succeeds for the keyset's amount key.
///
/// This mirrors the funding-side verification that `cdk_spilman` performs on
/// channel open; the settlement path previously skipped DLEQ entirely.
///
/// # Errors
///
/// Returns [`SpilmanError::InvalidResponse`] if the proofs JSON cannot be parsed,
/// [`SpilmanError::Keyset`] if the keyset lacks a pubkey for a proof's amount,
/// or [`SpilmanError::DleqVerification`] listing every proof that fails. An empty
/// proof set is accepted (vacuously true — nothing to verify).
///
/// [`Proof`]: CashuProof
#[allow(clippy::missing_errors_doc)]
pub fn verify_settlement_proofs_dleq(
    sender_proofs_json: &str,
    keyset_info: &KeysetInfo,
) -> Result<(), SpilmanError> {
    let proofs: Vec<CashuProof> = serde_json::from_str(sender_proofs_json)
        .map_err(|e| SpilmanError::InvalidResponse(format!("parse settlement proofs: {e}")))?;

    let mut errors: Vec<String> = Vec::new();
    for (i, proof) in proofs.iter().enumerate() {
        let amount_sat = u64::from(proof.amount);

        if proof.dleq.is_none() {
            errors.push(format!("proof #{i} ({amount_sat} sat): missing DLEQ"));
            continue;
        }

        let Some(mint_pubkey) = keyset_info.active_keys.amount_key(proof.amount) else {
            errors.push(format!(
                "proof #{i} ({amount_sat} sat): no mint pubkey in keyset"
            ));
            continue;
        };

        if let Err(e) = proof.verify_dleq(mint_pubkey) {
            errors.push(format!("proof #{i} ({amount_sat} sat): {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(SpilmanError::DleqVerification(format!(
            "{} proof(s) failed: {}",
            errors.len(),
            errors.join("; ")
        )))
    }
}

// ---------------------------------------------------------------------------
// Networking
// ---------------------------------------------------------------------------

pub struct ReqwestNetworking {
    client: reqwest::Client,
}

impl Default for ReqwestNetworking {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestNetworking {
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl SpilmanClientAsyncNetworking for ReqwestNetworking {
    async fn call_mint_swap(
        &self,
        mint_url: &str,
        swap_request_json: &str,
    ) -> Result<String, String> {
        let url = format!("{mint_url}/v1/swap");
        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(swap_request_json.to_string())
            .send()
            .await
            .map_err(|e| format!("swap request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("swap failed: {status} - {body}"));
        }

        resp.text()
            .await
            .map_err(|e| format!("failed to read swap response: {e}"))
    }
}

pub struct DummySyncNetworking;

impl SpilmanClientNetworking for DummySyncNetworking {
    fn call_mint_swap(&self, _mint_url: &str, _json: &str) -> Result<String, String> {
        panic!("sync networking not used — use async path instead")
    }
}

type ClientBridge =
    SpilmanClientBridge<ConfigurableClientHost<MemoryClientStorage>, DummySyncNetworking>;

// ---------------------------------------------------------------------------
// SpilmanService
// ---------------------------------------------------------------------------

pub struct SpilmanService {
    client_bridge: ClientBridge,
    mint_url: String,
    sender_pubkey_hex: String,
}

impl SpilmanService {
    #[must_use]
    pub fn new(mint_url: &str, sender_secret: SecretKey) -> Self {
        let sender_pubkey_hex = sender_secret.public_key().to_hex();
        let mut host = ConfigurableClientHost::new(MemoryClientStorage::new());
        host.add_key(sender_secret);
        let client_bridge = SpilmanClientBridge::new(host, DummySyncNetworking);
        Self {
            client_bridge,
            mint_url: mint_url.to_owned(),
            sender_pubkey_hex,
        }
    }

    #[must_use]
    pub fn mint_url(&self) -> &str {
        &self.mint_url
    }

    #[must_use]
    pub fn sender_pubkey(&self) -> &str {
        &self.sender_pubkey_hex
    }

    /// Open a Spilman channel from a Cashu token.
    ///
    /// # Errors
    ///
    /// Returns [`SpilmanError::Bridge`] if `cdk_spilman` rejects the open (funding
    /// verification, mint swap, ...).
    #[allow(clippy::missing_errors_doc)]
    pub async fn open_channel(
        &self,
        token_str: &str,
        receiver_pubkey_hex: &str,
        expiry_secs: u64,
        keyset_info_json: &str,
        max_amount_per_output: u64,
        net: &ReqwestNetworking,
    ) -> Result<OpenChannelResult, SpilmanError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let expiry_timestamp = now.saturating_add(expiry_secs);

        self.client_bridge
            .open_channel_from_token_async(
                token_str,
                receiver_pubkey_hex,
                &self.sender_pubkey_hex,
                expiry_timestamp,
                keyset_info_json,
                max_amount_per_output,
                net,
            )
            .await
            .map_err(SpilmanError::from)
    }

    /// Create a balance-update payment (no funding proofs).
    ///
    /// # Errors
    ///
    /// Returns [`SpilmanError::Bridge`] on bridge failure (e.g. balance exceeds capacity).
    #[allow(clippy::missing_errors_doc)]
    pub fn create_payment(&self, channel_id: &str, balance: u64) -> Result<Payment, SpilmanError> {
        self.client_bridge
            .create_payment(channel_id, balance)
            .map_err(SpilmanError::from)
    }

    /// Create a balance-update payment that (re-)attaches funding proofs.
    ///
    /// # Errors
    ///
    /// Returns [`SpilmanError::Bridge`] on bridge failure.
    #[allow(clippy::missing_errors_doc)]
    pub fn create_payment_with_funding(
        &self,
        channel_id: &str,
        balance: u64,
    ) -> Result<Payment, SpilmanError> {
        self.client_bridge
            .create_payment_with_funding(channel_id, balance)
            .map_err(SpilmanError::from)
    }

    /// Build a cooperative-close request payment.
    ///
    /// # Errors
    ///
    /// Returns [`SpilmanError::Bridge`] on bridge failure.
    #[allow(clippy::missing_errors_doc)]
    pub fn request_cooperative_close(
        &self,
        channel_id: &str,
        final_balance: u64,
    ) -> Result<Payment, SpilmanError> {
        self.client_bridge
            .create_cooperative_close_request(channel_id, final_balance)
            .map_err(SpilmanError::from)
    }

    /// Process a cooperative-close response from the counterparty.
    ///
    /// # Errors
    ///
    /// Returns [`SpilmanError::Bridge`] if the response is invalid.
    #[allow(clippy::missing_errors_doc)]
    pub fn confirm_cooperative_close(&self, response_json: &str) -> Result<(), SpilmanError> {
        self.client_bridge
            .process_cooperative_close_response(response_json)
            .map_err(SpilmanError::from)
    }

    #[must_use]
    pub fn get_channel_info(&self, channel_id: &str) -> Option<ClientChannelInfo> {
        self.client_bridge.get_channel_info(channel_id)
    }
}

// ---------------------------------------------------------------------------
// Unit tests (no network, no mint)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #![allow(clippy::needless_pass_by_value)]

    use super::*;

    #[test]
    fn spilman_service_derives_sender_pubkey_from_secret() {
        let secret = SecretKey::generate();
        let expected = secret.public_key().to_hex();
        let svc = SpilmanService::new("http://127.0.0.1:1", secret);
        assert_eq!(svc.sender_pubkey(), expected);
        assert_eq!(svc.mint_url(), "http://127.0.0.1:1");
    }

    #[test]
    fn string_error_folds_into_bridge_variant() {
        let err = SpilmanError::from("channel not found".to_string());
        assert!(
            matches!(err, SpilmanError::Bridge(ref s) if s == "channel not found"),
            "String errors should fold into Bridge, got {err:?}"
        );
        assert!(!err.is_dleq());
    }

    #[test]
    fn dleq_variant_is_detected() {
        let err = SpilmanError::DleqVerification("proof #0 bad".to_string());
        assert!(err.is_dleq());
        assert!(
            err.to_string().contains("DLEQ verification failed"),
            "Display should mention DLEQ: {}",
            err
        );
    }

    #[test]
    fn verify_settlement_dleq_accepts_empty_proofs() {
        // No keyset needed for the empty case — parsing succeeds, loop is vacuous.
        // Build a throwaway KeysetInfo via its JSON parser to satisfy the type.
        let keyset_info = parse_minimal_keyset_info();
        let res = verify_settlement_proofs_dleq("[]", &keyset_info);
        assert!(res.is_ok(), "empty proofs should verify: {res:?}");
    }

    #[test]
    fn verify_settlement_dleq_rejects_malformed_json() {
        let keyset_info = parse_minimal_keyset_info();
        let err = verify_settlement_proofs_dleq("not json", &keyset_info)
            .expect_err("malformed JSON should error");
        assert!(matches!(err, SpilmanError::InvalidResponse(_)), "{err:?}");
    }

    /// Construct a minimal valid `KeysetInfo` for tests that only exercise control
    /// flow (empty/malformed input). Built from a real pubkey + computed keyset id so
    /// it stays valid regardless of upstream parsing quirks.
    fn parse_minimal_keyset_info() -> KeysetInfo {
        use std::collections::BTreeMap;
        use std::str::FromStr;

        use cashu::nuts::{CurrencyUnit, Id, Keys, PublicKey};
        use cdk_spilman::KeysetInfo;

        // secp256k1 generator G compressed — a valid public key.
        let pk = PublicKey::from_str(
            "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
        )
        .expect("valid pubkey");
        let mut keys_map = BTreeMap::new();
        keys_map.insert(cashu::Amount::from(1), pk);
        let keys = Keys::new(keys_map);
        let id = Id::v1_from_keys(&keys);

        KeysetInfo::new(id, CurrencyUnit::Sat, keys, 0, None)
    }
}
