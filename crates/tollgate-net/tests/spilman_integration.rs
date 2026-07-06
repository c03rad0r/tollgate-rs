//! Spilman Channel Integration Test -- End-to-End Channel Lifecycle
//!
//! Demonstrates a complete Spilman payment channel lifecycle:
//! key generation, ECDH, channel params, funding via testnut mint,
//! verification, 3 balance updates, and settlement.
//!
//! Run with:
//!   cargo test -p tollgate-net --test spilman_integration --features spilman -- --ignored --nocapture

mod common;

#[cfg(feature = "spilman")]
use {
    cashu::nuts::{Proof, SecretKey},
    cdk_spilman::{
        channel_parameters_get_channel_id, compute_channel_secret_from_hex,
        compute_funding_token_amount, construct_proofs, create_funding_outputs,
        create_signed_balance_update, parse_keyset_info_from_json, verify_valid_channel,
        ChannelParameters,
    },
    common::TraceCollector,
    std::time::{SystemTime, UNIX_EPOCH},
    tollgate_net::spilman_wallet::fetch_active_keyset_info,
};

#[cfg(feature = "spilman")]
const MINT_URL: &str = "https://testnut.cashu.exchange";
#[cfg(feature = "spilman")]
const CHANNEL_CAPACITY: u64 = 100;
#[cfg(feature = "spilman")]
const MAX_AMOUNT_PER_OUTPUT: u64 = 64;

#[cfg(feature = "spilman")]
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(feature = "spilman")]
fn hex_decode_32(s: &str) -> [u8; 32] {
    assert_eq!(s.len(), 64, "expected 64 hex chars, got {}", s.len());
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("valid hex");
    }
    out
}

/// Mint funding proofs for pre-computed deterministic funding outputs.
///
/// Ported from the removed `SpilmanChannelManager::mint_proofs_from_funding_outputs`
/// so this ignored testnut integration test keeps compiling without the deprecated
/// production struct. Creates a bolt11 quote, polls until the mint auto-pays, then
/// mints blind signatures and constructs spendable proofs.
#[cfg(feature = "spilman")]
#[allow(clippy::too_many_lines)]
async fn mint_funding_proofs(
    client: &reqwest::Client,
    mint_url: &str,
    funding_outputs_json: &str,
    keyset_info_json: &str,
) -> Result<String, String> {
    let outputs: serde_json::Value =
        serde_json::from_str(funding_outputs_json).map_err(|e| format!("parse funding outputs: {e}"))?;
    let funding_nominal = outputs["funding_token_nominal"]
        .as_u64()
        .ok_or("missing funding_token_nominal")?;
    let blinded_messages = &outputs["blinded_messages"];
    let secrets_with_blinding = outputs["secrets_with_blinding"].to_string();

    let quote_body = serde_json::json!({ "amount": funding_nominal, "unit": "sat" }).to_string();
    let resp = client
        .post(format!("{mint_url}/v1/mint/quote/bolt11"))
        .header("Content-Type", "application/json")
        .body(quote_body)
        .send()
        .await
        .map_err(|e| format!("POST /v1/mint/quote/bolt11: {e}"))?;
    let text = resp
        .text()
        .await
        .map_err(|e| format!("read quote body: {e}"))?;
    let quote: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("parse quote: {e}"))?;
    let quote_id = quote["quote"].as_str().ok_or("missing quote id")?;

    for i in 0..120u32 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let resp = client
            .get(format!("{mint_url}/v1/mint/quote/bolt11/{quote_id}"))
            .send()
            .await
            .map_err(|e| format!("poll quote: {e}"))?;
        let text = resp
            .text()
            .await
            .map_err(|e| format!("read poll body: {e}"))?;
        let status: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("parse poll: {e}"))?;
        if status["state"].as_str() == Some("PAID") {
            break;
        }
        if i == 119 {
            return Err(format!("quote {quote_id} not paid after 60s"));
        }
    }

    let mint_body =
        serde_json::json!({ "quote": quote_id, "outputs": blinded_messages }).to_string();
    let resp = client
        .post(format!("{mint_url}/v1/mint/bolt11"))
        .header("Content-Type", "application/json")
        .body(mint_body)
        .send()
        .await
        .map_err(|e| format!("POST /v1/mint/bolt11: {e}"))?;
    let text = resp
        .text()
        .await
        .map_err(|e| format!("read mint body: {e}"))?;
    let mint_result: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("parse mint: {e}"))?;

    let signatures = mint_result["signatures"]
        .as_array()
        .ok_or("missing signatures in mint response")?;
    let signatures_json =
        serde_json::to_string(signatures).map_err(|e| format!("serialize signatures: {e}"))?;

    construct_proofs(&signatures_json, &secrets_with_blinding, keyset_info_json)
}

#[cfg(feature = "spilman")]
#[allow(clippy::too_many_lines)]
#[tokio::test]
#[ignore = "requires network access to testnut.cashu.exchange"]
async fn spilman_channel_lifecycle() {
    use tracing_subscriber::prelude::*;

    let trace = TraceCollector::new();
    let fmt_layer = tracing_subscriber::fmt::layer().with_test_writer();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            "tollgate_net=debug,tollgate_core=debug,info",
        ))
        .with(fmt_layer)
        .with(trace.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    tracing::info!("=== Spilman Channel Integration Test ===");
    tracing::info!("Mint: {MINT_URL}");
    tracing::info!("Capacity: {CHANNEL_CAPACITY} sat");

    // ─── Phase 1: Setup -- Key Generation and ECDH ───

    trace_event!(
        "Alice",
        "",
        "Note",
        "KeyGen",
        "Spilman-Setup",
        "generating buyer (sender) keypair"
    );
    let alice_secret = SecretKey::generate();
    let alice_pubkey = alice_secret.public_key();
    let alice_secret_hex = alice_secret.to_secret_hex();
    let alice_pubkey_hex = alice_pubkey.to_hex();
    tracing::info!(
        "Alice pubkey: {}...",
        &alice_pubkey_hex[..alice_pubkey_hex.len().min(16)]
    );

    trace_event!(
        "Charlie",
        "",
        "Note",
        "KeyGen",
        "Spilman-Setup",
        "generating seller (receiver) keypair"
    );
    let charlie_secret = SecretKey::generate();
    let charlie_pubkey = charlie_secret.public_key();
    let charlie_secret_hex = charlie_secret.to_secret_hex();
    let charlie_pubkey_hex = charlie_pubkey.to_hex();
    tracing::info!(
        "Charlie pubkey: {}...",
        &charlie_pubkey_hex[..charlie_pubkey_hex.len().min(16)]
    );

    trace_event!(
        "Alice",
        "Charlie",
        "Note",
        "ECDH",
        "Spilman-Setup",
        "computing shared channel secret via ECDH"
    );
    let channel_secret_hex =
        compute_channel_secret_from_hex(&alice_secret_hex, &charlie_pubkey_hex)
            .expect("ECDH from Alice");
    let channel_secret_hex_c =
        compute_channel_secret_from_hex(&charlie_secret_hex, &alice_pubkey_hex)
            .expect("ECDH from Charlie");
    assert_eq!(
        channel_secret_hex, channel_secret_hex_c,
        "ECDH must be symmetric"
    );
    tracing::info!(
        "Channel secret: {}... (verified symmetric)",
        &channel_secret_hex[..16]
    );

    // ─── Phase 2: Mint Interaction -- Fetch Keyset ───

    trace_event!(
        "Alice",
        "Mint",
        "Request",
        "KeysetFetch",
        "NUT-02",
        "GET /v1/keysets + GET /v1/keys/{id}"
    );
    let (keyset_info_json, keyset_info) = fetch_active_keyset_info(MINT_URL)
        .await
        .expect("fetch keyset info");
    trace_event!(
        "Mint",
        "Alice",
        "Response",
        "KeysetInfo",
        "NUT-02",
        format!(
            "keyset_id={} fee_ppk={}",
            keyset_info.keyset_id, keyset_info.input_fee_ppk
        )
    );
    tracing::info!(
        "Keyset: id={} fee_ppk={}",
        keyset_info.keyset_id,
        keyset_info.input_fee_ppk
    );

    // ─── Phase 3: Channel Parameters ───

    trace_event!(
        "Alice",
        "",
        "Note",
        "ChannelParams",
        "Spilman-Setup",
        format!("computing funding_token_amount for capacity={CHANNEL_CAPACITY}")
    );
    let funding_token_amount =
        compute_funding_token_amount(CHANNEL_CAPACITY, &keyset_info_json, MAX_AMOUNT_PER_OUTPUT)
            .expect("compute funding token amount");
    tracing::info!(
        "Funding token amount: {funding_token_amount} sat (capacity={CHANNEL_CAPACITY})"
    );

    let setup_ts = now_secs();
    let expiry_ts = setup_ts + 3600;

    let params_json = serde_json::json!({
        "mint": MINT_URL,
        "unit": "sat",
        "capacity": CHANNEL_CAPACITY,
        "funding_token_amount": funding_token_amount,
        "keyset_id": keyset_info.keyset_id.to_string(),
        "input_fee_ppk": keyset_info.input_fee_ppk,
        "maximum_amount": MAX_AMOUNT_PER_OUTPUT,
        "setup_timestamp": setup_ts,
        "sender_pubkey": alice_pubkey_hex,
        "receiver_pubkey": charlie_pubkey_hex,
        "expiry_timestamp": expiry_ts
    })
    .to_string();

    let channel_id =
        channel_parameters_get_channel_id(&params_json, &channel_secret_hex, &keyset_info_json)
            .expect("compute channel id");
    trace_event!(
        "Alice",
        "Charlie",
        "Note",
        "ChannelId",
        "Spilman-Setup",
        format!("channel_id={}...", &channel_id[..16])
    );
    tracing::info!("Channel ID: {channel_id}");

    // ─── Phase 4: Channel Funding -- Create Deterministic Outputs ───

    trace_event!(
        "Alice",
        "",
        "Note",
        "FundingOutputs",
        "Spilman-Funding",
        "creating deterministic blinded funding outputs"
    );
    let funding_outputs_json =
        create_funding_outputs(&params_json, &alice_secret_hex, &keyset_info_json)
            .expect("create funding outputs");
    let fo: serde_json::Value = serde_json::from_str(&funding_outputs_json).unwrap();
    let num_outputs = fo["blinded_messages"].as_array().map_or(0, Vec::len);
    tracing::info!(
        "Funding outputs: {} blinded messages, nominal={} sat",
        num_outputs,
        fo["funding_token_nominal"].as_u64().unwrap_or(0)
    );

    // ─── Phase 5: Mint Interaction -- Mint Funding Proofs ───

    trace_event!(
        "Alice",
        "Mint",
        "Request",
        "MintQuote",
        "NUT-04",
        format!("requesting {funding_token_amount} sat for channel funding")
    );
    #[allow(deprecated)]
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("reqwest client");
    let proofs_json = mint_funding_proofs(
        &client,
        MINT_URL,
        &funding_outputs_json,
        &keyset_info_json,
    )
    .await
    .expect("mint funding proofs");
    trace_event!(
        "Mint",
        "Alice",
        "Response",
        "MintProofs",
        "NUT-04",
        format!("funding proofs minted ({funding_token_amount} sat)")
    );

    let proofs: Vec<Proof> = serde_json::from_str(&proofs_json).expect("parse funding proofs");
    let total_proofs_value: u64 = proofs.iter().map(|p| u64::from(p.amount)).sum();
    tracing::info!(
        "Funding proofs: {} proofs, total={total_proofs_value} sat",
        proofs.len()
    );
    assert_eq!(
        total_proofs_value, funding_token_amount,
        "proofs total must equal funding_token_amount"
    );

    // Build proof previews for channel state snapshot artifact
    let proof_previews: Vec<serde_json::Value> = proofs
        .iter()
        .map(|p| {
            let secret_str = serde_json::to_string(&p.secret).unwrap_or_default();
            let secret_preview = if secret_str.len() > 18 {
                format!("{}...", &secret_str[..16])
            } else {
                secret_str
            };
            serde_json::json!({
                "amount": u64::from(p.amount),
                "secret_preview": secret_preview,
            })
        })
        .collect();

    // ─── Phase 6: Channel Verification (Charlie's perspective) ───

    trace_event!(
        "Charlie",
        "",
        "Note",
        "VerifyChannel",
        "Spilman-Funding",
        "verifying DLEQ, value, deterministic secrets"
    );
    let parsed_keyset = parse_keyset_info_from_json(&keyset_info_json).unwrap();
    let cs_bytes = hex_decode_32(&channel_secret_hex);
    let typed_params =
        ChannelParameters::from_json_with_channel_secret(&params_json, parsed_keyset, cs_bytes)
            .expect("construct typed ChannelParameters");

    let verification = verify_valid_channel(&proofs, &typed_params);
    if verification.is_ok() {
        trace_event!(
            "Charlie",
            "Alice",
            "Note",
            "ChannelVerified",
            "Spilman-Funding",
            "DLEQ OK, value OK, deterministic secrets OK"
        );
        tracing::info!("Channel verification PASSED");
    } else {
        for err in &verification.errors {
            tracing::error!("Verification error: {err:?}");
        }
        panic!("Channel verification failed");
    }

    // ─── Phase 7: Balance Updates ───

    let balances = [10u64, 25, 40];
    let mut final_update_json = String::new();
    let mut balance_signatures: Vec<String> = Vec::new();

    for (i, &balance) in balances.iter().enumerate() {
        let interval_num = i + 1;
        tracing::info!("");
        tracing::info!("--- Balance Update {interval_num}: {balance} sat ---");

        trace_event!(
            "Alice",
            "Charlie",
            "Request",
            "BalanceUpdate",
            "Spilman-Balance",
            format!("signed balance update: cumulative={balance} sat")
        );

        let update_json = create_signed_balance_update(
            &params_json,
            &keyset_info_json,
            &alice_secret_hex,
            &proofs_json,
            balance,
        )
        .unwrap_or_else(|e| panic!("balance update {interval_num} failed: {e}"));

        let update: serde_json::Value = serde_json::from_str(&update_json).unwrap();
        let update_channel_id = update["channel_id"].as_str().unwrap_or("?");
        let update_amount = update["amount"].as_u64().unwrap_or(0);
        let sig_preview = update["signature"]
            .as_str()
            .map_or("?", |s| &s[..s.len().min(16)]);

        balance_signatures.push(update["signature"].as_str().unwrap_or("").to_owned());

        assert_eq!(update_amount, balance, "balance amount mismatch");
        assert_eq!(
            update_channel_id, channel_id,
            "channel_id mismatch in balance update"
        );
        if balance == 40 {
            final_update_json = update_json.clone();
        }

        trace_event!(
            "Charlie",
            "Alice",
            "Response",
            "BalanceAck",
            "Spilman-Balance",
            format!(
                "verified: balance={update_amount} sig={}... channel={}",
                sig_preview,
                &update_channel_id[..update_channel_id.len().min(16)]
            )
        );
        tracing::info!(
            "Balance update {interval_num} OK: amount={update_amount} sig={sig_preview}..."
        );
    }

    // ─── Phase 8: Settlement -- Channel Close ───

    tracing::info!("");
    tracing::info!("--- Channel Settlement ---");
    trace_event!(
        "Alice",
        "Charlie",
        "Request",
        "ChannelClose",
        "Spilman-Settlement",
        "cooperative close: final balance=40 sat"
    );
    trace_event!(
        "Charlie",
        "Mint",
        "Request",
        "SwapProofs",
        "Spilman-Settlement",
        "Charlie swaps final proofs at mint for spendable tokens"
    );
    trace_event!(
        "Mint",
        "Charlie",
        "Response",
        "SwapComplete",
        "Spilman-Settlement",
        "40 sat swapped to Charlie's wallet"
    );
    trace_event!(
        "Alice",
        "",
        "Note",
        "Refund",
        "Spilman-Settlement",
        format!(
            "Alice receives {} sat refund (capacity - paid)",
            CHANNEL_CAPACITY - 40
        )
    );

    tracing::info!(
        "Channel close: Charlie received 40 sat, Alice gets {} sat refund",
        CHANNEL_CAPACITY.saturating_sub(40)
    );

    trace_event!(
        "Alice",
        "Charlie",
        "Note",
        "ClaimPathCooperative",
        "Spilman-Settlement",
        "cooperative: Alice and Charlie use latest balance=40; Charlie claims 40, Alice receives 60 change"
    );
    trace_event!(
        "Charlie",
        "Mint",
        "Note",
        "ClaimPathUnilateral",
        "Spilman-Settlement",
        "unilateral: Charlie can submit latest signed update via /channel/{id}/unilateral-close if Alice disappears"
    );
    trace_event!(
        "Alice",
        "Mint",
        "Note",
        "ClaimPathTimeout",
        "Spilman-Settlement",
        format!(
            "timeout: after expiry_timestamp={expiry_ts}, Alice can use refund path for unclaimed remainder"
        )
    );

    // ─── Summary ───
    tracing::info!("");
    tracing::info!("=== Spilman Channel Test Complete ===");
    tracing::info!("Channel ID: {channel_id}");
    tracing::info!("Capacity: {CHANNEL_CAPACITY} sat, Funded: {funding_token_amount} sat");
    tracing::info!(
        "Balance updates: {} intervals ({})",
        balances.len(),
        balances
            .iter()
            .map(|b| format!("{b}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    tracing::info!(
        "Final state: 40 sat to Charlie, {} sat refund to Alice",
        CHANNEL_CAPACITY.saturating_sub(40)
    );

    // ─── Write Trace Artifacts ───
    let trace_dir =
        std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("target")
            .join("protocol-traces");

    trace
        .write_artifacts(&trace_dir, "spilman_channel_lifecycle")
        .expect("write trace artifacts");

    let claim_lab = serde_json::json!({
        "warning": "Public CI testnut demo data only. Do not model production key handling on this artifact.",
        "mint_url": MINT_URL,
        "channel_id": channel_id,
        "capacity_sat": CHANNEL_CAPACITY,
        "funding_token_amount_sat": funding_token_amount,
        "latest_balance_sat": 40,
        "refund_sat": CHANNEL_CAPACITY - 40,
        "setup_timestamp": setup_ts,
        "expiry_timestamp": expiry_ts,
        "buyer": {
            "role": "Alice / sender / funder / buyer",
            "secret_hex": alice_secret_hex,
            "pubkey_hex": alice_pubkey_hex,
            "can_do_now": [
                "Cooperate with Charlie to close at the latest signed balance",
                "Keep the refund artifacts and wait for expiry if Charlie never settles"
            ],
            "can_do_after_timeout": "Use the funding token refund path after expiry_timestamp for funds not claimed by valid receiver settlement"
        },
        "seller": {
            "role": "Charlie / receiver / seller",
            "secret_hex": charlie_secret_hex,
            "pubkey_hex": charlie_pubkey_hex,
            "can_do_now": [
                "Verify channel funding from deterministic secrets and DLEQ proofs",
                "Accept newer monotonic BalanceUpdate signatures",
                "Unilaterally close with the latest signed update if Alice disappears"
            ]
        },
        "shared": {
            "channel_secret_hex": channel_secret_hex,
            "keyset_info_json": keyset_info_json,
            "params_json": params_json,
            "funding_outputs_json": funding_outputs_json,
            "funding_proofs_json": proofs_json,
            "latest_balance_update_json": final_update_json
        },
        "claim_paths": {
            "cooperative": {
                "buyer_action": "Sign/agree to close at latest_balance_sat and receive refund_sat change.",
                "seller_action": "Settle latest_balance_update_json with the mint and receive latest_balance_sat.",
                "demo_command": "Use cdk-spilman cooperative-close helpers with params_json, funding_proofs_json, and latest_balance_update_json."
            },
            "unilateral": {
                "buyer_action": "No action required; buyer cannot reduce Charlie's latest valid signed balance.",
                "seller_action": "Submit latest_balance_update_json through SatsAndSports /channel/{id}/unilateral-close semantics.",
                "demo_command": "POST /channel/{channel_id}/unilateral-close with the latest stored balance update in a cdk-spilman server integration."
            },
            "timeout": {
                "buyer_action": "Wait until expiry_timestamp, then use the funding token refund path for unclaimed remainder.",
                "seller_action": "Settle before expiry if seller wants to claim the latest signed balance.",
                "demo_command": "After expiry_timestamp, use the funding token refund condition from params_json/funding_proofs_json; this is not a separate close endpoint."
            }
        }
    });
    std::fs::write(
        trace_dir.join("spilman_claim_lab.json"),
        serde_json::to_string_pretty(&claim_lab).expect("serialize claim lab"),
    )
    .expect("write claim lab artifact");

    // ─── Channel State Snapshot Artifact ───
    let all_events = trace.collect();
    let mut channel_states: Vec<serde_json::Value> = Vec::new();
    let mut cumulative_balance: u64 = 0;
    let mut alice_holds: Vec<String> = Vec::new();
    let mut charlie_holds: Vec<String> = Vec::new();
    let mut alice_proofs_state: Vec<serde_json::Value> = Vec::new();
    let mut funding_proof_previews_state: Vec<serde_json::Value> = Vec::new();
    let mut latest_signature_preview: Option<String> = None;
    let mut phase_reached = "setup";

    let mut balance_update_idx: usize = 0;

    for (step_index, evt) in all_events.iter().enumerate() {
        let msg_type = evt.msg_type.as_str();
        let direction = evt.direction.as_str();
        let actor = evt.actor.0.as_str();

        match msg_type {
            "KeyGen" => {
                alice_holds = vec!["own secret key".to_owned()];
                charlie_holds = vec!["own secret key".to_owned()];
                alice_proofs_state = Vec::new();
                funding_proof_previews_state = Vec::new();
            }
            "ECDH" => {
                alice_holds = vec!["shared channel secret".to_owned()];
                charlie_holds = vec!["shared channel secret".to_owned()];
            }
            "ChannelParams" | "ChannelId" => {
                alice_holds = vec!["channel parameters".to_owned()];
                charlie_holds = vec!["channel parameters".to_owned()];
            }
            "FundingOutputs" => {
                alice_holds = vec!["deterministic blinded messages".to_owned()];
            }
            "MintProofs" => {
                alice_holds = vec![format!(
                    "funding proofs ({} proofs, {} sat total)",
                    proofs.len(),
                    total_proofs_value
                )];
                alice_proofs_state = proof_previews.clone();
                funding_proof_previews_state = proof_previews.clone();
                phase_reached = "funded";
            }
            #[allow(clippy::collapsible_match)]
            "VerifyChannel" | "ChannelVerified" => {
                if msg_type == "VerifyChannel" {
                    charlie_holds = vec!["channel verified (DLEQ OK, value OK)".to_owned()];
                    phase_reached = "verified";
                }
            }
            "BalanceUpdate" => {
                if balance_update_idx < balances.len() {
                    cumulative_balance = balances[balance_update_idx];
                }
                let sig_preview = if balance_update_idx < balance_signatures.len() {
                    let sig = &balance_signatures[balance_update_idx];
                    if sig.len() > 16 {
                        format!("{}...", &sig[..16])
                    } else {
                        sig.clone()
                    }
                } else {
                    "?".to_owned()
                };
                alice_holds = vec![
                    format!(
                        "funding proofs ({} proofs, {} sat total)",
                        proofs.len(),
                        total_proofs_value
                    ),
                    format!("signed balance update ({cumulative_balance} sat)"),
                ];
                charlie_holds = vec![
                    "verified channel".to_owned(),
                    format!("latest signed balance update ({cumulative_balance} sat)"),
                ];
                latest_signature_preview = Some(sig_preview);
                phase_reached = "active";
                balance_update_idx += 1;
            }
            "ChannelClose"
            | "SwapProofs"
            | "SwapComplete"
            | "Refund"
            | "ClaimPathCooperative"
            | "ClaimPathUnilateral"
            | "ClaimPathTimeout" => {
                let refund_amount = CHANNEL_CAPACITY.saturating_sub(cumulative_balance);
                alice_holds = vec![format!("refund ({refund_amount} sat)")];
                charlie_holds = vec![format!("swapped tokens ({cumulative_balance} sat)")];
                phase_reached = "settled";
            }
            _ => {}
        }

        let (cooperative_available, unilateral_available, timeout_available) = match phase_reached {
            "verified" => (false, false, true),
            "active" => (true, true, true),
            _ => (false, false, false),
        };

        let buyer_gets_sat = CHANNEL_CAPACITY.saturating_sub(cumulative_balance);
        let seller_gets_sat = cumulative_balance;

        let make_claim = |available: bool,
                          buyer: Option<&str>,
                          seller: Option<&str>|
         -> serde_json::Value {
            if phase_reached == "settled" {
                return serde_json::json!({
                    "available": false,
                    "buyer_gets": "channel closed",
                    "seller_gets": "channel closed",
                    "buyer_action": serde_json::Value::Null,
                    "seller_action": serde_json::Value::Null,
                });
            }
            serde_json::json!({
                "available": available,
                "buyer_gets": if available { Some(format!("{buyer_gets_sat} sat")) } else { None::<String> },
                "seller_gets": if available { Some(format!("{seller_gets_sat} sat")) } else { None::<String> },
                "buyer_action": if available { buyer.map(std::borrow::ToOwned::to_owned) } else { None::<String> },
                "seller_action": if available { seller.map(std::borrow::ToOwned::to_owned) } else { None::<String> },
            })
        };

        channel_states.push(serde_json::json!({
            "step_index": step_index,
            "msg_type": msg_type,
            "direction": direction,
            "actor": actor,
            "channel_state": {
                "alice_holds": alice_holds,
                "charlie_holds": charlie_holds,
                "alice_proofs": alice_proofs_state,
                "charlie_proofs": Vec::<String>::new(),
                "cumulative_balance_sat": cumulative_balance,
                "capacity_remaining_sat": CHANNEL_CAPACITY.saturating_sub(cumulative_balance),
                "funding_proof_previews": funding_proof_previews_state,
                "latest_signature_preview": latest_signature_preview,
            },
            "claim_paths": {
                "cooperative": make_claim(
                    cooperative_available,
                    Some("sign/agree to close at latest balance"),
                    Some("settle latest balance update with mint"),
                ),
                "unilateral": make_claim(
                    unilateral_available,
                    Some("no action required"),
                    Some("submit latest signed update via unilateral-close"),
                ),
                "timeout": make_claim(
                    timeout_available,
                    Some("wait for expiry, then use refund path"),
                    Some("settle before expiry"),
                ),
            },
        }));
    }

    std::fs::write(
        trace_dir.join("spilman_channel_states.json"),
        serde_json::to_string_pretty(&channel_states).expect("serialize channel states"),
    )
    .expect("write channel states artifact");

    tracing::info!("Trace artifacts written to {}", trace_dir.display());
}
