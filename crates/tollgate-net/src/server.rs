use std::sync::Arc;

use axum::{extract::State, http::StatusCode, routing::post, Router};
use tokio::sync::Mutex;
use tollgate_core::bootstrap::ExhaustionConfig;
use tollgate_core::config::{ProductConfig, ProductMintConfig};
use tollgate_core::metering::PeerMetrics;
use tollgate_core::protocol::{Hash32, Message, PubKey};
use tollgate_core::session::{PeerSession, SessionConfig};
use tollgate_core::wallet::Wallet;

use crate::mock::MockAdapter;

struct AppState<W: Wallet> {
    session: Mutex<PeerSession<W, MockAdapter>>,
    adapter: Arc<MockAdapter>,
}

fn provider_pubkey() -> PubKey {
    PubKey([0x01; 33])
}

fn test_product_id() -> Hash32 {
    Hash32([0x11; 32])
}

fn test_option_id() -> Hash32 {
    Hash32([0x22; 32])
}

pub fn provider_config() -> SessionConfig {
    SessionConfig {
        pubkey: provider_pubkey(),
        protocol_version: 1,
        unit: "bytes".to_owned(),
        capabilities: 0x01,
        products: vec![ProductConfig {
            product_id: test_product_id(),
            extensions: vec![],
            pricing_scale: 1000,
            mint_options: vec![ProductMintConfig {
                option_id: test_option_id(),
                mint_url: "https://mint.example.com".to_owned(),
                price_per_second: 10,
                price_per_unit: 1,
                mint_unit: "sat".to_owned(),
            }],
        }],
        interval_ms: 5000,
        min_checkin_ms: 1000,
        max_interval_ms: 10000,
        exhaustion: ExhaustionConfig::default(),
    }
}

#[allow(clippy::missing_panics_doc)]
pub async fn run<W: Wallet + 'static>(port: u16, wallet: Arc<W>) {
    let adapter = Arc::new(MockAdapter::new());
    let config = provider_config();
    let session = PeerSession::new(wallet, adapter.clone(), config);

    let state = Arc::new(AppState {
        session: Mutex::new(session),
        adapter,
    });

    let app = Router::new()
        .route("/tollgate/message", post(handle_message::<W>))
        .with_state(state);

    tracing::info!("Provider listening on port {port}");
    tracing::info!("Product: 10 sat/s + 1 sat/unit, scale=1000");

    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind listener");
    axum::serve(listener, app)
        .await
        .expect("server exited with error");
}

async fn handle_message<W: Wallet>(
    State(state): State<Arc<AppState<W>>>,
    body: axum::body::Bytes,
) -> (StatusCode, Vec<u8>) {
    let msg: Message = match minicbor::decode(&body) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("Failed to decode CBOR message: {e}");
            return (StatusCode::BAD_REQUEST, vec![]);
        }
    };

    if let Message::MeteringReport(ref report) = msg {
        state.adapter.set_metrics(PeerMetrics {
            elapsed_ms: report.elapsed_ms,
            delivered: report.delivered,
            received: report.received,
        });
    }

    let is_announce = matches!(msg, Message::Announce(_));
    let msg_name = message_name(&msg);

    let mut session = state.session.lock().await;
    let mut responses = session.handle_message(msg).await;

    tracing::info!("Received {msg_name}, state: {:?}", session.state());

    if is_announce && responses.is_empty() {
        tracing::info!("Sending Announce + PriceSheet to peer");
        responses.push(session.create_announce());
        responses.push(session.create_price_sheet());
    }

    log_responses(&responses);

    if responses.is_empty() {
        (StatusCode::OK, vec![])
    } else {
        match minicbor::to_vec(&responses) {
            Ok(bytes) => (StatusCode::OK, bytes),
            Err(e) => {
                tracing::error!("Failed to encode response: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, vec![])
            }
        }
    }
}

fn log_responses(responses: &[Message]) {
    for resp in responses {
        match resp {
            Message::BootstrapAck(ack) => {
                tracing::info!("BootstrapAck: {:?}", ack.status);
            }
            Message::Reject(r) => {
                tracing::info!(
                    "Reject: {}",
                    r.reason_text.as_deref().unwrap_or("(no reason)")
                );
            }
            Message::Disconnect(_) => {
                tracing::info!("Peer disconnected. Session closed.");
            }
            _ => {}
        }
    }
}

fn message_name(msg: &Message) -> &'static str {
    match msg {
        Message::Announce(_) => "Announce",
        Message::PriceSheet(_) => "PriceSheet",
        Message::Accept(_) => "Accept",
        Message::ChannelReady(_) => "ChannelReady",
        Message::MeteringReport(_) => "MeteringReport",
        Message::BalanceUpdate(_) => "BalanceUpdate",
        Message::BalanceAck(_) => "BalanceAck",
        Message::BootstrapToken(_) => "BootstrapToken",
        Message::BootstrapAck(_) => "BootstrapAck",
        Message::RolloverInit(_) => "RolloverInit",
        Message::RolloverReady(_) => "RolloverReady",
        Message::ChannelClose(_) => "ChannelClose",
        Message::CloseAck(_) => "CloseAck",
        Message::Reject(_) => "Reject",
        Message::Disconnect(_) => "Disconnect",
        Message::MeteringReportResponse(_) => "MeteringReportResponse",
    }
}

// ---------------------------------------------------------------------------
// Spilman payment channel server (requires --features spilman)
// ---------------------------------------------------------------------------

#[cfg(feature = "spilman")]
use {
    crate::spilman_service::{
        verify_settlement_proofs_dleq, Payment, PaymentProof, SpilmanAsyncNetworking, SpilmanBridge,
        SpilmanHost,
    },
    async_trait::async_trait,
    cashu::nuts::{CurrencyUnit, Id, Proof as CashuProof, PublicKey, SecretKey},
    cdk_spilman::{
        compute_channel_secret_from_hex, parse_keyset_info_from_json, sign_with_tweaked_key_util,
        BridgeError, ChannelFunding, ChannelPolicy, ChannelState, CloseError, CloseSuccess,
        ClosingData,
    },
    serde_json,
    std::collections::HashMap,
    std::sync::atomic::{AtomicU64, Ordering},
    std::time::{SystemTime, UNIX_EPOCH},
    tollgate_core::protocol::{
        BalanceAck, BalanceUpdate, ChannelClose, CloseAck, MessageType, ReasonCode, Reject,
    },
};

// ---------------------------------------------------------------------------
// Channel lifecycle state
// ---------------------------------------------------------------------------

/// Server-side per-channel lifecycle state. In-memory only for M3.
#[cfg(feature = "spilman")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelLifecycleState {
    Opening,
    Open,
    ClosingCooperative,
    ClosingUnilateral,
    Closed,
    SettlementFailedRetryable,
    SettlementFailedFinal,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[cfg(feature = "spilman")]
fn encode_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Convert a CBOR BalanceUpdate into a cdk-spilman Payment struct for the JSON API.
#[cfg(feature = "spilman")]
fn balance_update_to_payment(update: &BalanceUpdate) -> Result<Payment, String> {
    let channel_id_hex = encode_hex(&update.channel_id.0);
    let signature_hex = encode_hex(&update.balance_signature.0);
    let params = update
        .channel_params_json
        .as_ref()
        .map(|b| serde_json::from_slice::<serde_json::Value>(b))
        .transpose()
        .map_err(|e| format!("invalid channel_params_json: {e}"))?;
    let funding_proofs: Option<Vec<CashuProof>> = update
        .funding_proofs_json
        .as_ref()
        .map(|b| serde_json::from_slice(b))
        .transpose()
        .map_err(|e| format!("invalid funding_proofs_json: {e}"))?;

    Ok(Payment {
        channel_id: channel_id_hex,
        balance: update.cumulative_balance,
        signature: signature_hex,
        params,
        funding_proofs,
    })
}

/// Convert a CBOR ChannelClose into a cdk-spilman Payment struct for the JSON API.
#[cfg(feature = "spilman")]
fn channel_close_to_payment(close: &ChannelClose) -> Result<Payment, String> {
    let channel_id_hex = encode_hex(&close.channel_id.0);
    let signature_hex = encode_hex(&close.final_signature.0);
    let params = close
        .channel_params_json
        .as_ref()
        .map(|b| serde_json::from_slice::<serde_json::Value>(b))
        .transpose()
        .map_err(|e| format!("invalid channel_params_json: {e}"))?;
    let funding_proofs: Option<Vec<CashuProof>> = close
        .funding_proofs_json
        .as_ref()
        .map(|b| serde_json::from_slice(b))
        .transpose()
        .map_err(|e| format!("invalid funding_proofs_json: {e}"))?;

    Ok(Payment {
        channel_id: channel_id_hex,
        balance: close.final_balance,
        signature: signature_hex,
        params,
        funding_proofs,
    })
}

/// Map `BridgeError` to a protocol `ReasonCode`.
#[cfg(feature = "spilman")]
fn bridge_error_to_reason(err: &BridgeError) -> ReasonCode {
    match err {
        BridgeError::InvalidSignature(_)
        | BridgeError::BalanceMismatch { .. }
        | BridgeError::InsufficientBalance { .. }
        | BridgeError::BalanceExceedsCapacity { .. }
        | BridgeError::ChannelIdMismatch
        | BridgeError::ChannelClosed
        | BridgeError::ChannelClosing => ReasonCode::BalanceVerificationFailed,
        BridgeError::UnknownChannel
        | BridgeError::ValidationFailed(_)
        | BridgeError::CapacityTooSmall { .. }
        | BridgeError::ExpiryTooSoon { .. }
        | BridgeError::MaxAmountExceeded { .. }
        | BridgeError::ReceiverKeyNotAcceptable
        | BridgeError::MintOrKeysetNotAcceptable
        | BridgeError::UnsupportedUnit(_) => ReasonCode::FundingInvalid,
        BridgeError::InvalidRequest(_)
        | BridgeError::ServerMisconfigured(_)
        | BridgeError::Internal(_) => ReasonCode::Other,
    }
}

/// Map `CloseError` to a protocol `ReasonCode`.
#[cfg(feature = "spilman")]
fn close_error_to_reason(err: &CloseError) -> ReasonCode {
    match err {
        CloseError::ValidationFailed { .. } | CloseError::AlreadyClosed { .. } => {
            ReasonCode::BalanceVerificationFailed
        }
        CloseError::UnknownChannel { .. } => ReasonCode::FundingInvalid,
        CloseError::MintRejected { .. }
        | CloseError::MintRejectedAfterRetry { .. }
        | CloseError::UnblindFailed { .. }
        | CloseError::StorageFailed { .. } => ReasonCode::Other,
    }
}

/// Whether a `CloseError` is retryable (transient mint/network issue).
#[cfg(feature = "spilman")]
fn close_error_is_retryable(err: &CloseError) -> bool {
    matches!(
        err,
        CloseError::MintRejected { .. } | CloseError::MintRejectedAfterRetry { .. }
    )
}

// ---------------------------------------------------------------------------
// Extracted handler functions (testable in isolation)
// ---------------------------------------------------------------------------

/// Process a `BalanceUpdate` through the Spilman bridge. Returns the
/// appropriate response message (`BalanceAck` on success, `Reject` on failure).
#[cfg(feature = "spilman")]
pub(crate) fn process_balance_update<H>(
    bridge: &SpilmanBridge<H, ()>,
    update: &BalanceUpdate,
) -> Message
where
    H: SpilmanHost<()>,
{
    let payment = match balance_update_to_payment(update) {
        Ok(p) => p,
        Err(e) => {
            return Message::Reject(Reject {
                msg_type: MessageType::Reject as u8,
                rejected_type: MessageType::BalanceUpdate as u8,
                reason_code: ReasonCode::Other,
                reason_text: Some(format!("payment conversion failed: {e}")),
            });
        }
    };
    let payment_json = match serde_json::to_string(&payment) {
        Ok(j) => j,
        Err(e) => {
            return Message::Reject(Reject {
                msg_type: MessageType::Reject as u8,
                rejected_type: MessageType::BalanceUpdate as u8,
                reason_code: ReasonCode::Other,
                reason_text: Some(format!("payment serialization failed: {e}")),
            });
        }
    };

    match bridge.process_payment_via_json(&payment_json, &()) {
        Ok(result) => {
            tracing::info!(
                "[spilman] Payment accepted: channel={} balance={}",
                &payment.channel_id[..payment.channel_id.len().min(16)],
                result.balance,
            );
            Message::BalanceAck(BalanceAck {
                msg_type: MessageType::BalanceAck as u8,
                channel_id: update.channel_id.clone(),
                accepted_balance: result.balance,
            })
        }
        Err(e) => {
            let reason_code = bridge_error_to_reason(&e);
            tracing::warn!("[spilman] Payment rejected ({reason_code:?}): {e}");
            Message::Reject(Reject {
                msg_type: MessageType::Reject as u8,
                rejected_type: MessageType::BalanceUpdate as u8,
                reason_code,
                reason_text: Some(format!("balance update rejected: {e}")),
            })
        }
    }
}

/// Verify DLEQ proofs on the settlement outputs returned by a close.
///
/// Looks up the keyset each proof was minted under (via the host's keyset
/// registry) and delegates to [`verify_settlement_proofs_dleq`]. An empty
/// `sender_proofs` set is accepted (nothing for the sender to settle).
///
/// # Errors
///
/// Returns a human-readable `String` describing the first failure (parse,
/// missing keyset, or DLEQ).
#[cfg(feature = "spilman")]
fn verify_close_settlement_dleq<H>(
    host: &H,
    mint_url: &str,
    close: &CloseSuccess,
) -> Result<(), String>
where
    H: SpilmanHost<()>,
{
    let proofs: Vec<CashuProof> = serde_json::from_str(&close.sender_proofs)
        .map_err(|e| format!("parse settlement proofs: {e}"))?;

    // Nothing for the sender to settle — vacuously valid.
    if proofs.is_empty() {
        return Ok(());
    }

    let keyset_id = proofs[0].keyset_id;
    let keyset_json = host.get_keyset_info(mint_url, &keyset_id).ok_or_else(|| {
        format!("keyset {keyset_id} not registered with host (cannot verify DLEQ)")
    })?;
    let keyset_info = parse_keyset_info_from_json(&keyset_json)
        .map_err(|e| format!("parse keyset info for {keyset_id}: {e}"))?;

    verify_settlement_proofs_dleq(&close.sender_proofs, &keyset_info).map_err(|e| e.to_string())
}

/// Process a `ChannelClose` through the Spilman bridge. Executes cooperative
/// close settlement against the mint. Returns `CloseAck` on success, `Reject`
/// on failure.
///
/// As of Phase 0 the settlement `sender_proofs` are DLEQ-verified before a
/// `CloseAck` is emitted — a failed DLEQ proof is treated as an invalid
/// settlement and rejected.
#[cfg(feature = "spilman")]
pub(crate) async fn process_channel_close<H, N>(
    bridge: &SpilmanBridge<H, ()>,
    net: &N,
    close: &ChannelClose,
    mint_url: &str,
) -> Message
where
    H: SpilmanHost<()>,
    N: SpilmanAsyncNetworking + Sync,
{
    let payment = match channel_close_to_payment(close) {
        Ok(p) => p,
        Err(e) => {
            return Message::Reject(Reject {
                msg_type: MessageType::Reject as u8,
                rejected_type: MessageType::ChannelClose as u8,
                reason_code: ReasonCode::Other,
                reason_text: Some(format!("close payment conversion failed: {e}")),
            });
        }
    };
    let channel_id_short = &payment.channel_id[..payment.channel_id.len().min(16)];
    let payment_json = match serde_json::to_string(&payment) {
        Ok(j) => j,
        Err(e) => {
            return Message::Reject(Reject {
                msg_type: MessageType::Reject as u8,
                rejected_type: MessageType::ChannelClose as u8,
                reason_code: ReasonCode::Other,
                reason_text: Some(format!("close payment serialization failed: {e}")),
            });
        }
    };

    tracing::info!(
        "[spilman] ChannelClose: channel={} balance={} — executing cooperative close",
        channel_id_short,
        close.final_balance,
    );

    match bridge
        .execute_cooperative_close_async(&payment_json, net)
        .await
    {
        Ok(result) => {
            // Phase 0: verify DLEQ on settlement outputs before trusting the close.
            // A bad/missing DLEQ proof means the mint may not have actually signed
            // these proofs — reject rather than emit a CloseAck.
            if let Err(dleq_err) = verify_close_settlement_dleq(bridge.host(), mint_url, &result) {
                tracing::warn!(
                    "[spilman] Cooperative close DLEQ verification failed: {dleq_err}"
                );
                return Message::Reject(Reject {
                    msg_type: MessageType::Reject as u8,
                    rejected_type: MessageType::ChannelClose as u8,
                    reason_code: ReasonCode::FundingInvalid,
                    reason_text: Some(format!("settlement DLEQ verification failed: {dleq_err}")),
                });
            }
            tracing::info!(
                "[spilman] Cooperative close settled (DLEQ verified): channel={} receiver_sum={} sender_sum={}",
                &result.channel_id[..result.channel_id.len().min(16)],
                result.receiver_sum,
                result.sender_sum,
            );
            Message::CloseAck(CloseAck {
                msg_type: MessageType::CloseAck as u8,
                channel_id: close.channel_id.clone(),
                accepted_balance: close.final_balance,
            })
        }
        Err(e) => {
            let reason_code = close_error_to_reason(&e);
            tracing::warn!("[spilman] Cooperative close failed ({reason_code:?}): {e}");
            Message::Reject(Reject {
                msg_type: MessageType::Reject as u8,
                rejected_type: MessageType::ChannelClose as u8,
                reason_code,
                reason_text: Some(format!("cooperative close failed: {e}")),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Server networking
// ---------------------------------------------------------------------------

#[cfg(feature = "spilman")]
struct ServerNetworking {
    client: reqwest::Client,
}

#[cfg(feature = "spilman")]
impl ServerNetworking {
    fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[cfg(feature = "spilman")]
#[async_trait]
impl SpilmanAsyncNetworking for ServerNetworking {
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
            .map_err(|e| format!("server swap failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("server swap failed: {status} - {body}"));
        }

        resp.text()
            .await
            .map_err(|e| format!("read swap response: {e}"))
    }

    async fn refresh_all_keysets(&self, mint: &str) -> Result<(), String> {
        tracing::info!("[server-net] keyset refresh skipped (mint={mint})");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Server host
// ---------------------------------------------------------------------------

#[cfg(feature = "spilman")]
struct ServerSpilmanHost {
    receiver_secret: SecretKey,
    channels: std::sync::Mutex<HashMap<String, ChannelFunding>>,
    payments: std::sync::Mutex<HashMap<String, PaymentProof>>,
    states: std::sync::Mutex<HashMap<String, ChannelState>>,
    closing_data: std::sync::Mutex<HashMap<String, ClosingData>>,
    keyset_infos: std::sync::Mutex<HashMap<Id, String>>,
    active_keysets: std::sync::Mutex<HashMap<String, Vec<Id>>>,
    amount_due: AtomicU64,
    lifecycle_states: std::sync::Mutex<HashMap<String, ChannelLifecycleState>>,
}

#[cfg(feature = "spilman")]
impl ServerSpilmanHost {
    fn new(receiver_secret: SecretKey) -> Self {
        Self {
            receiver_secret,
            channels: std::sync::Mutex::new(HashMap::new()),
            payments: std::sync::Mutex::new(HashMap::new()),
            states: std::sync::Mutex::new(HashMap::new()),
            closing_data: std::sync::Mutex::new(HashMap::new()),
            keyset_infos: std::sync::Mutex::new(HashMap::new()),
            active_keysets: std::sync::Mutex::new(HashMap::new()),
            amount_due: AtomicU64::new(0),
            lifecycle_states: std::sync::Mutex::new(HashMap::new()),
        }
    }

    #[allow(dead_code)]
    fn add_keyset(&self, mint_url: &str, keyset_id: Id, keyset_info_json: String) {
        self.keyset_infos
            .lock()
            .unwrap()
            .insert(keyset_id, keyset_info_json);
        self.active_keysets
            .lock()
            .unwrap()
            .entry(mint_url.to_string())
            .or_default()
            .push(keyset_id);
    }

    fn receiver_pubkey_hex(&self) -> String {
        self.receiver_secret.public_key().to_hex()
    }

    fn set_lifecycle(&self, cid: &str, state: ChannelLifecycleState) {
        self.lifecycle_states
            .lock()
            .unwrap()
            .insert(cid.to_string(), state);
    }

    #[allow(dead_code)]
    fn get_lifecycle(&self, cid: &str) -> Option<ChannelLifecycleState> {
        self.lifecycle_states.lock().unwrap().get(cid).copied()
    }
}

#[cfg(feature = "spilman")]
impl SpilmanHost<()> for ServerSpilmanHost {
    fn receiver_key_is_acceptable(&self, pk: &PublicKey) -> bool {
        *pk == self.receiver_secret.public_key()
    }

    fn mint_and_keyset_is_acceptable(&self, _mint: &str, _id: &Id) -> bool {
        true
    }

    fn get_funding(&self, cid: &str) -> Option<ChannelFunding> {
        self.channels.lock().unwrap().get(cid).cloned()
    }

    fn save_funding(&self, cid: &str, funding: ChannelFunding, payment: PaymentProof) {
        self.channels
            .lock()
            .unwrap()
            .insert(cid.to_string(), funding);
        self.payments
            .lock()
            .unwrap()
            .insert(cid.to_string(), payment);
        self.states
            .lock()
            .unwrap()
            .insert(cid.to_string(), ChannelState::Open);
        self.set_lifecycle(cid, ChannelLifecycleState::Open);
    }

    fn get_amount_due(&self, _cid: &str, _ctx: Option<&()>) -> u64 {
        self.amount_due.load(Ordering::SeqCst)
    }

    fn record_payment(&self, cid: &str, payment: PaymentProof, _ctx: &()) {
        self.payments
            .lock()
            .unwrap()
            .insert(cid.to_string(), payment.clone());
        self.amount_due.store(payment.balance, Ordering::SeqCst);
    }

    fn get_channel_state(&self, cid: &str) -> ChannelState {
        self.states
            .lock()
            .unwrap()
            .get(cid)
            .copied()
            .unwrap_or(ChannelState::Open)
    }

    fn mark_channel_closing(
        &self,
        cid: &str,
        expiry_ts: u64,
        payment: PaymentProof,
    ) -> Result<(), String> {
        self.states
            .lock()
            .unwrap()
            .insert(cid.to_string(), ChannelState::Closing);
        self.closing_data.lock().unwrap().insert(
            cid.to_string(),
            ClosingData {
                expiry_timestamp: expiry_ts,
                balance: payment.balance,
                signature: payment.signature,
            },
        );
        Ok(())
    }

    fn get_closing_data(&self, cid: &str) -> Option<ClosingData> {
        self.closing_data.lock().unwrap().get(cid).cloned()
    }

    fn get_channel_policy(&self, _unit: &str) -> Option<ChannelPolicy> {
        Some(ChannelPolicy {
            min_capacity: 1,
            min_expiry_in_seconds: 60,
            max_amount_per_output: Some(64),
        })
    }

    fn now_seconds(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    fn get_balance_and_signature_for_unilateral_exit(&self, cid: &str) -> Option<PaymentProof> {
        self.payments.lock().unwrap().get(cid).cloned()
    }

    fn get_active_keyset_ids(&self, mint: &str, _unit: &CurrencyUnit) -> Vec<Id> {
        self.active_keysets
            .lock()
            .unwrap()
            .get(mint)
            .cloned()
            .unwrap_or_default()
    }

    fn get_keyset_info(&self, _mint: &str, kid: &Id) -> Option<String> {
        self.keyset_infos.lock().unwrap().get(kid).cloned()
    }

    fn mark_channel_closed(
        &self,
        cid: &str,
        _expiry_ts: u64,
        _balance: u64,
        _receiver_proofs: &str,
        _sender_proofs: &str,
        _receiver_sum: u64,
        _sender_sum: u64,
    ) -> Result<(), String> {
        self.states
            .lock()
            .unwrap()
            .insert(cid.to_string(), ChannelState::Closed);
        self.set_lifecycle(cid, ChannelLifecycleState::Closed);
        Ok(())
    }

    fn compute_channel_secret(
        &self,
        receiver_pk_hex: &str,
        sender_pk_hex: &str,
    ) -> Result<String, String> {
        let expected = self.receiver_secret.public_key().to_hex();
        if receiver_pk_hex != expected {
            return Err(format!(
                "receiver pubkey mismatch: expected {expected}, got {receiver_pk_hex}"
            ));
        }
        compute_channel_secret_from_hex(&self.receiver_secret.to_secret_hex(), sender_pk_hex)
    }

    fn sign_with_tweaked_key(
        &self,
        signer_pk_hex: &str,
        message_hex: &str,
        tweak_hex: &str,
    ) -> Result<String, String> {
        let expected = self.receiver_secret.public_key().to_hex();
        if signer_pk_hex != expected {
            return Err(format!(
                "signer pubkey mismatch: expected {expected}, got {signer_pk_hex}"
            ));
        }
        sign_with_tweaked_key_util(
            &self.receiver_secret.to_secret_hex(),
            message_hex,
            tweak_hex,
        )
    }
}

// ---------------------------------------------------------------------------
// Spilman app state and entry point
// ---------------------------------------------------------------------------

#[cfg(feature = "spilman")]
#[allow(dead_code)]
struct SpilmanServerState {
    bridge: SpilmanBridge<ServerSpilmanHost, ()>,
    net: ServerNetworking,
    mint_url: String,
}

#[cfg(feature = "spilman")]
struct SpilmanAppState {
    session: Mutex<PeerSession<crate::cdk_wallet::CdkWallet, MockAdapter>>,
    adapter: Arc<MockAdapter>,
    spilman: Mutex<SpilmanServerState>,
}

#[cfg(feature = "spilman")]
#[allow(clippy::missing_panics_doc)]
pub async fn run_spilman(
    port: u16,
    wallet: Arc<crate::cdk_wallet::CdkWallet>,
    receiver_secret: SecretKey,
    mint_url: &str,
) {
    let adapter = Arc::new(MockAdapter::new());
    let config = provider_config();
    let session = PeerSession::new(wallet, adapter.clone(), config);

    let server_host = ServerSpilmanHost::new(receiver_secret);
    let receiver_pubkey_hex = server_host.receiver_pubkey_hex();

    tracing::info!("Fetching keyset info from {mint_url}...");
    let (keyset_info_json, keyset_info) = crate::spilman_wallet::fetch_active_keyset_info(mint_url)
        .await
        .expect("fetch keyset info");
    server_host.add_keyset(mint_url, keyset_info.keyset_id, keyset_info_json);
    tracing::info!("Keyset loaded: id={}", keyset_info.keyset_id.to_string());

    let server_bridge = SpilmanBridge::new(server_host);

    let spilman_state = SpilmanServerState {
        bridge: server_bridge,
        net: ServerNetworking::new(),
        mint_url: mint_url.to_owned(),
    };

    let state = Arc::new(SpilmanAppState {
        session: Mutex::new(session),
        adapter,
        spilman: Mutex::new(spilman_state),
    });

    let app = Router::new()
        .route("/tollgate/message", post(handle_spilman_message))
        .route("/tollgate/force-close", post(handle_force_close))
        .with_state(state);

    tracing::info!("Spilman provider listening on port {port}");
    tracing::info!("Receiver pubkey: {receiver_pubkey_hex}");

    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind listener");
    axum::serve(listener, app)
        .await
        .expect("server exited with error");
}

// ---------------------------------------------------------------------------
// Spilman message handler
// ---------------------------------------------------------------------------

#[cfg(feature = "spilman")]
#[allow(clippy::too_many_lines)]
async fn handle_spilman_message(
    State(state): State<Arc<SpilmanAppState>>,
    body: axum::body::Bytes,
) -> (StatusCode, Vec<u8>) {
    let msg: Message = match minicbor::decode(&body) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("Failed to decode CBOR message: {e}");
            return (StatusCode::BAD_REQUEST, vec![]);
        }
    };

    if let Message::MeteringReport(ref report) = msg {
        state.adapter.set_metrics(PeerMetrics {
            elapsed_ms: report.elapsed_ms,
            delivered: report.delivered,
            received: report.received,
        });
    }

    let is_announce = matches!(msg, Message::Announce(_));
    let msg_name = message_name(&msg);

    let mut session = state.session.lock().await;
    let mut responses = session.handle_message(msg.clone()).await;

    tracing::info!("Received {msg_name}, state: {:?}", session.state());

    if is_announce && responses.is_empty() {
        tracing::info!("Sending Announce + PriceSheet to peer");
        responses.push(session.create_announce());
        responses.push(session.create_price_sheet());
    }

    drop(session);

    if let Message::BalanceUpdate(ref update) = msg {
        let spilman_state = state.spilman.lock().await;
        let resp = process_balance_update(&spilman_state.bridge, update);
        responses.push(resp);
    }

    if let Message::ChannelClose(ref close) = msg {
        let spilman_state = state.spilman.lock().await;
        let channel_id_hex = encode_hex(&close.channel_id.0);
        spilman_state
            .bridge
            .host()
            .set_lifecycle(&channel_id_hex, ChannelLifecycleState::ClosingCooperative);
        let resp = process_channel_close(
            &spilman_state.bridge,
            &spilman_state.net,
            close,
            &spilman_state.mint_url,
        )
        .await;
        if matches!(resp, Message::Reject(_)) {
            spilman_state.bridge.host().set_lifecycle(
                &channel_id_hex,
                ChannelLifecycleState::SettlementFailedRetryable,
            );
        }
        responses.push(resp);
    }

    log_responses(&responses);

    if responses.is_empty() {
        (StatusCode::OK, vec![])
    } else {
        match minicbor::to_vec(&responses) {
            Ok(bytes) => (StatusCode::OK, bytes),
            Err(e) => {
                tracing::error!("Failed to encode response: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, vec![])
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Server-initiated unilateral close (requires --features spilman)
// ---------------------------------------------------------------------------

#[cfg(feature = "spilman")]
async fn handle_force_close(
    State(state): State<Arc<SpilmanAppState>>,
    body: axum::body::Bytes,
) -> (StatusCode, String) {
    let channel_id = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(v) => v["channel_id"].as_str().unwrap_or_default().to_string(),
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid JSON: {e}"));
        }
    };

    if channel_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing channel_id".to_string());
    }

    tracing::info!(
        "[spilman] Force-close requested: channel={}",
        &channel_id[..channel_id.len().min(16)],
    );

    let spilman_state = state.spilman.lock().await;
    spilman_state
        .bridge
        .host()
        .set_lifecycle(&channel_id, ChannelLifecycleState::ClosingUnilateral);

    match spilman_state
        .bridge
        .execute_unilateral_close_async(&channel_id, &spilman_state.net)
        .await
    {
        Ok(result) => {
            // Phase 0: verify DLEQ on settlement outputs before reporting closed.
            if let Err(dleq_err) =
                verify_close_settlement_dleq(spilman_state.bridge.host(), &spilman_state.mint_url, &result)
            {
                spilman_state
                    .bridge
                    .host()
                    .set_lifecycle(&channel_id, ChannelLifecycleState::SettlementFailedFinal);
                tracing::warn!("[spilman] Unilateral close DLEQ verification failed: {dleq_err}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("settlement DLEQ verification failed: {dleq_err}"),
                );
            }
            tracing::info!(
                "[spilman] Unilateral close settled (DLEQ verified): channel={} receiver_sum={} sender_sum={}",
                &channel_id[..channel_id.len().min(16)],
                result.receiver_sum,
                result.sender_sum,
            );
            (
                StatusCode::OK,
                serde_json::json!({
                    "status": "closed",
                    "channel_id": result.channel_id,
                    "receiver_sum": result.receiver_sum,
                    "sender_sum": result.sender_sum,
                })
                .to_string(),
            )
        }
        Err(e) => {
            let lifecycle = if close_error_is_retryable(&e) {
                ChannelLifecycleState::SettlementFailedRetryable
            } else {
                ChannelLifecycleState::SettlementFailedFinal
            };
            spilman_state
                .bridge
                .host()
                .set_lifecycle(&channel_id, lifecycle);
            tracing::warn!("[spilman] Unilateral close failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("close failed: {e}"),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Inline tests for Spilman handler functions
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "spilman"))]
mod spilman_handler_tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use tollgate_core::protocol::{CloseReason, Signature};

    struct TestHost {
        receiver_secret: SecretKey,
        channels: RefCell<HashMap<String, ChannelFunding>>,
        payments: RefCell<HashMap<String, PaymentProof>>,
        states: RefCell<HashMap<String, ChannelState>>,
        closing_data: RefCell<HashMap<String, ClosingData>>,
        amount_due: Cell<u64>,
    }

    impl TestHost {
        fn new() -> Self {
            Self {
                receiver_secret: SecretKey::generate(),
                channels: RefCell::new(HashMap::new()),
                payments: RefCell::new(HashMap::new()),
                states: RefCell::new(HashMap::new()),
                closing_data: RefCell::new(HashMap::new()),
                amount_due: Cell::new(0),
            }
        }

        fn seed_channel(&self, channel_id: &str) {
            let funding = ChannelFunding {
                params_json: r#"{"capacity":1000,"expiry_timestamp":99999999999,"mint":"http://invalid.local","unit":"sat"}"#.to_owned(),
                funding_proofs_json: "[]".to_owned(),
                channel_secret_hex: "0000000000000000000000000000000000000000000000000000000000000001"
                    .to_owned(),
                keyset_info_json: r#"{"id":"009a1f293253e41e","unit":"sat","active":true,"input_fee_ppk":0,"keys":{}}"#.to_owned(),
            };
            self.channels
                .borrow_mut()
                .insert(channel_id.to_string(), funding);
            self.states
                .borrow_mut()
                .insert(channel_id.to_string(), ChannelState::Open);
        }

        fn record_accepted(&self, channel_id: &str, balance: u64, signature: &str) {
            self.payments.borrow_mut().insert(
                channel_id.to_string(),
                PaymentProof {
                    balance,
                    signature: signature.to_owned(),
                },
            );
            self.amount_due.set(balance);
        }
    }

    impl SpilmanHost<()> for TestHost {
        fn receiver_key_is_acceptable(&self, pk: &PublicKey) -> bool {
            *pk == self.receiver_secret.public_key()
        }

        fn mint_and_keyset_is_acceptable(&self, _mint: &str, _id: &Id) -> bool {
            true
        }

        fn get_funding(&self, cid: &str) -> Option<ChannelFunding> {
            self.channels.borrow().get(cid).cloned()
        }

        fn save_funding(&self, cid: &str, funding: ChannelFunding, initial_payment: PaymentProof) {
            self.channels.borrow_mut().insert(cid.to_string(), funding);
            self.payments
                .borrow_mut()
                .insert(cid.to_string(), initial_payment);
            self.states
                .borrow_mut()
                .insert(cid.to_string(), ChannelState::Open);
        }

        fn get_amount_due(&self, _cid: &str, _ctx: Option<&()>) -> u64 {
            self.amount_due.get()
        }

        fn record_payment(&self, cid: &str, payment: PaymentProof, _ctx: &()) {
            self.payments
                .borrow_mut()
                .insert(cid.to_string(), payment.clone());
            self.amount_due.set(payment.balance);
        }

        fn get_channel_state(&self, cid: &str) -> ChannelState {
            self.states
                .borrow()
                .get(cid)
                .copied()
                .unwrap_or(ChannelState::Open)
        }

        fn mark_channel_closing(
            &self,
            cid: &str,
            expiry_ts: u64,
            payment: PaymentProof,
        ) -> Result<(), String> {
            self.states
                .borrow_mut()
                .insert(cid.to_string(), ChannelState::Closing);
            self.closing_data.borrow_mut().insert(
                cid.to_string(),
                ClosingData {
                    expiry_timestamp: expiry_ts,
                    balance: payment.balance,
                    signature: payment.signature,
                },
            );
            Ok(())
        }

        fn get_closing_data(&self, cid: &str) -> Option<ClosingData> {
            self.closing_data.borrow().get(cid).cloned()
        }

        fn get_channel_policy(&self, _unit: &str) -> Option<ChannelPolicy> {
            Some(ChannelPolicy {
                min_capacity: 1,
                min_expiry_in_seconds: 60,
                max_amount_per_output: Some(64),
            })
        }

        fn now_seconds(&self) -> u64 {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        }

        fn get_balance_and_signature_for_unilateral_exit(&self, cid: &str) -> Option<PaymentProof> {
            self.payments.borrow().get(cid).cloned()
        }

        fn get_active_keyset_ids(&self, _mint: &str, _unit: &CurrencyUnit) -> Vec<Id> {
            Vec::new()
        }

        fn get_keyset_info(&self, _mint: &str, _kid: &Id) -> Option<String> {
            None
        }

        fn mark_channel_closed(
            &self,
            cid: &str,
            _expiry_ts: u64,
            _balance: u64,
            _receiver_proofs_json: &str,
            _sender_proofs_json: &str,
            _receiver_sum: u64,
            _sender_sum: u64,
        ) -> Result<(), String> {
            self.states
                .borrow_mut()
                .insert(cid.to_string(), ChannelState::Closed);
            Ok(())
        }

        fn compute_channel_secret(
            &self,
            _receiver_pk_hex: &str,
            _sender_pk_hex: &str,
        ) -> Result<String, String> {
            Err("not used in these tests".to_string())
        }

        fn sign_with_tweaked_key(
            &self,
            _signer_pk_hex: &str,
            _message_hex: &str,
            _tweak_hex: &str,
        ) -> Result<String, String> {
            Err("not used in these tests".to_string())
        }
    }

    struct UnreachableNetworking;

    #[async_trait]
    impl SpilmanAsyncNetworking for UnreachableNetworking {
        async fn call_mint_swap(
            &self,
            _mint_url: &str,
            _swap_request_json: &str,
        ) -> Result<String, String> {
            panic!("call_mint_swap should not be reached in this test");
        }

        async fn refresh_all_keysets(&self, _mint: &str) -> Result<(), String> {
            panic!("refresh_all_keysets should not be reached in this test");
        }
    }

    // -----------------------------------------------------------------------
    // Helper functions
    // -----------------------------------------------------------------------

    fn channel_id_bytes(b: u8) -> [u8; 32] {
        [b; 32]
    }

    fn channel_id_hex(b: u8) -> String {
        encode_hex(&channel_id_bytes(b))
    }

    fn make_update(
        channel_id_byte: u8,
        balance: u64,
        sig_byte: u8,
        with_funding: bool,
    ) -> BalanceUpdate {
        let params_json = if with_funding {
            Some(
                br#"{"capacity":1000,"expiry_timestamp":99999999999,"mint":"http://invalid.local","unit":"sat"}"#
                    .to_vec(),
            )
        } else {
            None
        };
        let funding_json = if with_funding {
            Some(b"[]".to_vec())
        } else {
            None
        };
        BalanceUpdate {
            msg_type: MessageType::BalanceUpdate as u8,
            channel_id: Hash32(channel_id_bytes(channel_id_byte)),
            cumulative_balance: balance,
            balance_signature: Signature([sig_byte; 64]),
            net_amount: balance,
            channel_params_json: params_json,
            funding_proofs_json: funding_json,
        }
    }

    fn make_close(
        channel_id_byte: u8,
        balance: u64,
        sig_byte: u8,
        with_funding: bool,
    ) -> ChannelClose {
        let params_json = if with_funding {
            Some(
                br#"{"capacity":1000,"expiry_timestamp":99999999999,"mint":"http://invalid.local","unit":"sat"}"#
                    .to_vec(),
            )
        } else {
            None
        };
        let funding_json = if with_funding {
            Some(b"[]".to_vec())
        } else {
            None
        };
        ChannelClose {
            msg_type: MessageType::ChannelClose as u8,
            channel_id: Hash32(channel_id_bytes(channel_id_byte)),
            final_balance: balance,
            final_signature: Signature([sig_byte; 64]),
            reason: CloseReason::Normal,
            channel_params_json: params_json,
            funding_proofs_json: funding_json,
        }
    }

    // -----------------------------------------------------------------------
    // bridge_error_to_reason mapping tests
    // -----------------------------------------------------------------------

    #[test]
    fn bridge_error_invalid_signature_maps_to_balance_verification_failed() {
        let err = BridgeError::InvalidSignature("bad sig".to_string());
        assert_eq!(
            bridge_error_to_reason(&err),
            ReasonCode::BalanceVerificationFailed
        );
    }

    #[test]
    fn bridge_error_insufficient_balance_maps_to_balance_verification_failed() {
        let err = BridgeError::InsufficientBalance {
            balance: 5,
            amount_due: 10,
        };
        assert_eq!(
            bridge_error_to_reason(&err),
            ReasonCode::BalanceVerificationFailed
        );
    }

    #[test]
    fn bridge_error_balance_exceeds_capacity_maps_to_balance_verification_failed() {
        let err = BridgeError::BalanceExceedsCapacity {
            balance: 2000,
            capacity: 1000,
        };
        assert_eq!(
            bridge_error_to_reason(&err),
            ReasonCode::BalanceVerificationFailed
        );
    }

    #[test]
    fn bridge_error_unknown_channel_maps_to_funding_invalid() {
        let err = BridgeError::UnknownChannel;
        assert_eq!(bridge_error_to_reason(&err), ReasonCode::FundingInvalid);
    }

    #[test]
    fn bridge_error_invalid_request_maps_to_other() {
        let err = BridgeError::InvalidRequest("bad".to_string());
        assert_eq!(bridge_error_to_reason(&err), ReasonCode::Other);
    }

    // -----------------------------------------------------------------------
    // close_error_to_reason mapping tests
    // -----------------------------------------------------------------------

    #[test]
    fn close_error_validation_failed_maps_to_balance_verification_failed() {
        let err = CloseError::ValidationFailed {
            reason: "sig mismatch".to_string(),
            status: 402,
            expected_balance: Some(100),
            actual_balance: Some(50),
        };
        assert_eq!(
            close_error_to_reason(&err),
            ReasonCode::BalanceVerificationFailed
        );
    }

    #[test]
    fn close_error_unknown_channel_maps_to_funding_invalid() {
        let err = CloseError::UnknownChannel { status: 404 };
        assert_eq!(close_error_to_reason(&err), ReasonCode::FundingInvalid);
    }

    #[test]
    fn close_error_mint_rejected_maps_to_other() {
        let err = CloseError::MintRejected {
            mint_error: serde_json::json!({"error": "internal"}),
            status: 502,
        };
        assert_eq!(close_error_to_reason(&err), ReasonCode::Other);
    }

    // -----------------------------------------------------------------------
    // Conversion helper round-trip tests
    // -----------------------------------------------------------------------

    #[test]
    fn balance_update_to_payment_round_trip() {
        let update = make_update(0xAB, 500, 0xCC, true);
        let payment = balance_update_to_payment(&update).expect("conversion should succeed");

        assert_eq!(payment.channel_id, channel_id_hex(0xAB));
        assert_eq!(payment.balance, 500);
        assert!(payment.signature.starts_with("cc"));
        assert!(payment.params.is_some());
        let params = payment.params.unwrap();
        assert_eq!(params["capacity"], 1000);
        assert_eq!(params["unit"], "sat");
        assert!(payment.funding_proofs.is_some());
        assert!(payment.funding_proofs.as_ref().unwrap().is_empty());
    }

    #[test]
    fn channel_close_to_payment_round_trip() {
        let close = make_close(0xDD, 750, 0xEE, true);
        let payment = channel_close_to_payment(&close).expect("conversion should succeed");

        assert_eq!(payment.channel_id, channel_id_hex(0xDD));
        assert_eq!(payment.balance, 750);
        assert!(payment.signature.starts_with("ee"));
        assert!(payment.params.is_some());
        let params = payment.params.unwrap();
        assert_eq!(params["capacity"], 1000);
        assert_eq!(params["unit"], "sat");
        assert!(payment.funding_proofs.is_some());
        assert!(payment.funding_proofs.as_ref().unwrap().is_empty());
    }

    // -----------------------------------------------------------------------
    // process_balance_update rejection-path tests
    // -----------------------------------------------------------------------

    #[test]
    fn balance_update_unknown_channel_rejected_as_funding_invalid() {
        let host = TestHost::new();
        let bridge = SpilmanBridge::new(host);

        let update = make_update(0x42, 100, 0xFF, false);
        let msg = process_balance_update(&bridge, &update);

        match msg {
            Message::Reject(reject) => {
                assert_eq!(reject.reason_code, ReasonCode::FundingInvalid);
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn balance_update_invalid_signature_rejected_as_balance_verification_failed() {
        let host = TestHost::new();
        let cid = channel_id_hex(0x42);
        host.seed_channel(&cid);

        let bridge = SpilmanBridge::new(host);

        let update = make_update(0x42, 100, 0xFF, false);
        let msg = process_balance_update(&bridge, &update);

        match msg {
            Message::Reject(reject) => {
                assert_eq!(reject.reason_code, ReasonCode::BalanceVerificationFailed);
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn balance_update_stale_lower_balance_rejected_amount_due_unchanged() {
        let host = TestHost::new();
        let cid = channel_id_hex(0x55);
        host.seed_channel(&cid);
        host.record_accepted(&cid, 30, "deadbeef");

        let balance_before = host.amount_due.get();
        assert_eq!(balance_before, 30);

        let bridge = SpilmanBridge::new(host);

        let update = make_update(0x55, 10, 0xAA, false);
        let msg = process_balance_update(&bridge, &update);

        match msg {
            Message::Reject(reject) => {
                assert_eq!(reject.reason_code, ReasonCode::BalanceVerificationFailed);
            }
            other => panic!("expected Reject, got {other:?}"),
        }

        assert_eq!(bridge.host().amount_due.get(), 30);
    }

    // -----------------------------------------------------------------------
    // process_channel_close rejection-path tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn channel_close_unknown_channel_empty_sig_no_mint_call() {
        let host = TestHost::new();
        let bridge = SpilmanBridge::new(host);
        let net = UnreachableNetworking;

        let close = make_close(0x99, 0, 0x00, false);
        let msg = process_channel_close(&bridge, &net, &close, "http://test-mint").await;

        match msg {
            Message::Reject(reject) => {
                assert_eq!(reject.reason_code, ReasonCode::BalanceVerificationFailed);
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn channel_close_unknown_channel_with_sig_rejected_without_mint() {
        let host = TestHost::new();
        let bridge = SpilmanBridge::new(host);
        let net = UnreachableNetworking;

        let close = make_close(0x88, 100, 0xAA, false);
        let msg = process_channel_close(&bridge, &net, &close, "http://test-mint").await;

        match msg {
            Message::Reject(reject) => {
                assert_eq!(reject.reason_code, ReasonCode::BalanceVerificationFailed);
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }
}
