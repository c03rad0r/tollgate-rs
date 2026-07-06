//! Mock-mint Spilman tests (no external network).
//!
//! Spins up the `cdk-spilman-test-mint` in-memory Cashu mint on a loopback
//! (127.0.0.1) socket and exercises the real wrapper code paths against it:
//!   - [`fetch_active_keyset_info`]
//!   - [`SpilmanService`] open / payments / channel-info
//!   - [`verify_settlement_proofs_dleq`] against real mint-issued DLEQ proofs
//!     (positive) and tampered proofs (negative)
//!
//! No internet / testnut access is required. The mint's `FakeWallet` auto-pays
//! the bolt11 quotes, so the full NUT-04 mint flow runs locally.
//!
//! Requires the `spilman` feature (which enables `cdk-spilman-test-mint`).

#![cfg(feature = "spilman")]

use std::str::FromStr;
use std::sync::Arc;

use cashu::mint_url::MintUrl;
use cashu::nuts::{CurrencyUnit, Proof as CashuProof, SecretKey};
use cashu::nuts::Token as CashuToken;
use cdk_spilman_test_mint::{build_router, build_test_mint, TestMintConfig};
use tokio::net::TcpListener;

use tollgate_net::cdk_wallet::CdkWallet;
use tollgate_net::spilman_service::{
    verify_settlement_proofs_dleq, ReqwestNetworking, SpilmanError, SpilmanService,
};
use tollgate_net::spilman_wallet::fetch_active_keyset_info;

// ---------------------------------------------------------------------------
// Loopback mock-mint fixture
// ---------------------------------------------------------------------------

/// A loopback mock mint bound to an ephemeral port.
///
/// Drop the returned `JoinHandle`-ish guard (or call `.abort()`) to stop the
/// server; the in-memory mint's background tasks stop with it.
struct MockMint {
    base_url: String,
    _serve: tokio::task::JoinHandle<()>,
}

async fn start_mock_mint() -> MockMint {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback mint");
    let port = listener
        .local_addr()
        .expect("local_addr")
        .port();
    let base_url = format!("http://127.0.0.1:{port}");

    let mut config = TestMintConfig::default();
    config.listen_host = "127.0.0.1".to_string();
    config.listen_port = port;
    config.base_url = base_url.clone();
    // Auto-pay as fast as possible to keep the test snappy.
    config.payment_delay_seconds = 0;
    // Zero fee so channel funding outputs balance (inputs == outputs); the
    // canonical mock-mint setup. Non-zero ppk requires fee-aware output sizing
    // in the open path, which is out of scope for this foundation test.
    config.default_input_fee_ppk = 0;

    let mint = build_test_mint(&config).await.expect("build test mint");
    let router = build_router(Arc::new(mint)).await.expect("build mint router");

    let serve = tokio::spawn(async move {
        // Serve until the task is aborted (test tear-down).
        let _ = axum::serve(listener, router).await;
    });

    MockMint {
        base_url,
        _serve: serve,
    }
}

/// Mint `amount` sat from the loopback mint via `CdkWallet` and return the raw
/// unspent proofs (with mint-issued DLEQ).
async fn mint_proofs(base_url: &str, amount: u64) -> Vec<CashuProof> {
    let wallet = CdkWallet::new(base_url, rand::random())
        .await
        .expect("CdkWallet init");
    wallet
        .mint_test_tokens(amount)
        .await
        .expect("mint tokens from mock mint");
    let proofs_json = wallet.unspent_proofs_json().await.expect("unspent proofs");
    let proofs: Vec<CashuProof> =
        serde_json::from_str(&proofs_json).expect("parse cashu proofs");
    assert!(
        !proofs.is_empty(),
        "mock mint should have produced proofs"
    );
    // Every mock-mint proof must carry a DLEQ (required for Spilman channels).
    for (i, p) in proofs.iter().enumerate() {
        assert!(p.dleq.is_some(), "proof #{i} missing DLEQ");
    }
    proofs
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Open a Spilman channel against the loopback mock mint and run a couple of
/// balance updates through the [`SpilmanService`] wrapper.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spilman_open_channel_and_payments_against_mock_mint() {
    let mint = start_mock_mint().await;
    let base_url = mint.base_url.as_str();

    // Fund the buyer wallet, then carve out a ~1000-sat token.
    let proofs = mint_proofs(base_url, 2000).await;
    let mut selected = Vec::new();
    let mut selected_total = 0u64;
    for p in &proofs {
        if selected_total >= 1000 {
            break;
        }
        selected.push(p.clone());
        selected_total += u64::from(p.amount);
    }
    assert!(selected_total >= 1000, "need >=1000 sat, got {selected_total}");

    let mint_url = MintUrl::from_str(base_url).expect("parse mint url");
    let token = CashuToken::new(mint_url, selected, None, CurrencyUnit::Sat);
    let token_str = token.to_string();

    // Fetch the active sat keyset via the (rewritten) wrapper.
    let (keyset_info_json, keyset_info) = fetch_active_keyset_info(base_url)
        .await
        .expect("fetch keyset info from mock mint");
    assert!(keyset_info.input_fee_ppk <= 1000, "unexpected fee");

    // Buyer side.
    let sender_secret = SecretKey::generate();
    let svc = SpilmanService::new(base_url, sender_secret);
    let receiver_pubkey_hex = SecretKey::generate().public_key().to_hex();
    let net = ReqwestNetworking::new();

    let open = svc
        .open_channel(
            &token_str,
            &receiver_pubkey_hex,
            3600,
            &keyset_info_json,
            64,
            &net,
        )
        .await
        .expect("open channel against mock mint");

    assert!(!open.channel_id.is_empty(), "channel id must be non-empty");
    assert!(open.capacity > 0, "capacity must be positive");

    // Payment 1 (with funding) + payment 2 (balance only) via the wrapper.
    let p1 = svc
        .create_payment_with_funding(&open.channel_id, 10)
        .expect("create payment with funding");
    assert_eq!(p1.balance, 10);
    assert!(p1.has_funding(), "first payment must carry funding");

    let p2 = svc
        .create_payment(&open.channel_id, 25)
        .expect("create payment");
    assert_eq!(p2.balance, 25);
    assert!(!p2.has_funding(), "subsequent payment has no funding");

    let info = svc
        .get_channel_info(&open.channel_id)
        .expect("client channel info");
    assert_eq!(info.current_balance, 25);
    assert_eq!(info.payment_count, 2);

    mint._serve.abort();
}

/// `verify_settlement_proofs_dleq` must accept real mint-issued DLEQ proofs and
/// reject proofs whose DLEQ is missing or corrupted. Uses the loopback mock mint
/// only to obtain genuine DLEQ-bearing proofs — no channel/close required.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settlement_dleq_accepts_real_proofs_and_rejects_tampered() {
    let mint = start_mock_mint().await;
    let base_url = mint.base_url.as_str();

    let mut proofs = mint_proofs(base_url, 64).await;
    let (_, keyset_info) = fetch_active_keyset_info(base_url)
        .await
        .expect("fetch keyset info");

    // Positive: the mint-issued proofs all carry valid DLEQ.
    let valid_json = serde_json::to_string(&proofs).expect("serialize proofs");
    verify_settlement_proofs_dleq(&valid_json, &keyset_info)
        .expect("real mint proofs should pass DLEQ verification");

    // Negative 1: strip the DLEQ off the first proof.
    proofs[0].dleq = None;
    let stripped_json = serde_json::to_string(&proofs).expect("serialize proofs");
    let err = verify_settlement_proofs_dleq(&stripped_json, &keyset_info)
        .expect_err("missing DLEQ must fail verification");
    assert!(
        matches!(err, SpilmanError::DleqVerification(ref m) if m.contains("missing DLEQ")),
        "expected DleqVerification(missing DLEQ), got {err:?}"
    );

    // Negative 2: restore the DLEQ but corrupt every component (e, s, r).
    proofs[0].dleq = Some(cashu::nuts::ProofDleq::new(
        SecretKey::generate(),
        SecretKey::generate(),
        SecretKey::generate(),
    ));
    let corrupt_json = serde_json::to_string(&proofs).expect("serialize proofs");
    let err = verify_settlement_proofs_dleq(&corrupt_json, &keyset_info)
        .expect_err("corrupt DLEQ must fail verification");
    assert!(
        matches!(err, SpilmanError::DleqVerification(_)),
        "expected DleqVerification, got {err:?}"
    );

    mint._serve.abort();
}
