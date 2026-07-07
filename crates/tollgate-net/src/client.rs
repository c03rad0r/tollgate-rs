use std::sync::Arc;
use std::time::Duration;

use tollgate_core::bootstrap::ExhaustionConfig;
use tollgate_core::protocol::{
    Accept, BootstrapStatus, BootstrapToken, Disconnect, Hash32, IntervalRange, Message,
    MessageType, MeteringReport, PubKey, ReasonCode,
};
use tollgate_core::session::{PeerSession, SessionConfig};
use tollgate_core::types::Amount;
use tollgate_core::wallet::Wallet;

use crate::mock::MockAdapter;

#[cfg(feature = "spilman")]
use {
    cdk_spilman::ClientStorage,
    crate::spilman_service::{ReqwestNetworking, SpilmanService},
    cashu::mint_url::MintUrl,
    cashu::nuts::{CurrencyUnit, Proof as CashuProof, Token as CashuToken},
    serde_json,
    std::str::FromStr,
    tollgate_core::protocol::{BalanceUpdate, ChannelClose, CloseReason, Signature as TgSignature},
};

#[cfg(feature = "spilman")]
use crate::spilman_wallet::fetch_active_keyset_info;

#[cfg(feature = "spilman")]
fn decode_hex<const N: usize>(hex: &str) -> Option<[u8; N]> {
    let hex = hex.strip_prefix("0x").unwrap_or(hex);
    if hex.len() != N * 2 {
        return None;
    }
    let mut bytes = [0u8; N];
    for i in 0..N {
        bytes[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

fn client_pubkey() -> PubKey {
    PubKey([0x02; 33])
}

pub fn client_config() -> SessionConfig {
    SessionConfig {
        pubkey: client_pubkey(),
        protocol_version: 1,
        unit: "bytes".to_owned(),
        capabilities: 0x01,
        products: vec![],
        interval_ms: 5000,
        min_checkin_ms: 1000,
        max_interval_ms: 10000,
        exhaustion: ExhaustionConfig::default(),
    }
}

async fn send_message(client: &reqwest::Client, peer_url: &str, msg: &Message) -> Vec<Message> {
    let body = minicbor::to_vec(msg).expect("encode message");
    let resp = client
        .post(format!("{peer_url}/tollgate/message"))
        .header("content-type", "application/cbor")
        .body(body)
        .send()
        .await
        .expect("send message to provider");
    let bytes = resp.bytes().await.expect("read response body");
    if bytes.is_empty() {
        return vec![];
    }
    minicbor::decode(&bytes).expect("decode response")
}

fn extract_product_info(responses: &[Message]) -> Option<(Hash32, Hash32)> {
    for msg in responses {
        if let Message::PriceSheet(sheet) = msg {
            if let Some(product) = sheet.products.first() {
                if let Some(mint) = product.mint_options.first() {
                    return Some((product.product_id.clone(), mint.option_id.clone()));
                }
            }
        }
    }
    None
}

#[allow(clippy::missing_panics_doc, clippy::too_many_lines)]
pub async fn run_mock(peer_url: &str, intervals: u32, interval_secs: u64, initial_balance: u64) {
    let wallet = Arc::new(crate::mock::MockWallet::new(initial_balance));
    let adapter = Arc::new(MockAdapter::new());
    let config = client_config();
    let mut session = PeerSession::new(wallet.clone(), adapter, config);
    let http = reqwest::Client::new();
    let mint_url = "https://mint.example.com";

    tracing::info!("Connecting to {peer_url}");
    let announce = session.create_announce();
    let responses = send_message(&http, peer_url, &announce).await;
    tracing::info!("Sent Announce");

    for msg in &responses {
        session.handle_message(msg.clone()).await;
    }

    let Some((product_id, option_id)) = extract_product_info(&responses) else {
        tracing::error!("No product found in provider PriceSheet");
        return;
    };
    tracing::info!("Received Announce + PriceSheet from provider");

    let accept = Message::Accept(Accept {
        msg_type: MessageType::Accept as u8,
        product_id,
        option_id,
        interval_range: IntervalRange([2500, 10000]),
        channel_funding: vec![],
    });
    let _ = send_message(&http, peer_url, &accept).await;
    tracing::info!("Selected product, sent Accept");

    let token_bytes = wallet
        .create_token(Amount(100), mint_url)
        .await
        .expect("create token");
    let bootstrap_token = Message::BootstrapToken(BootstrapToken {
        msg_type: MessageType::BootstrapToken as u8,
        token: token_bytes,
    });
    let responses = send_message(&http, peer_url, &bootstrap_token).await;
    for msg in &responses {
        session.handle_message(msg.clone()).await;
    }

    let accepted = responses.iter().any(|m| {
        matches!(
            m,
            Message::BootstrapAck(ack) if ack.status == BootstrapStatus::Accepted
        )
    });
    if !accepted {
        tracing::error!("Bootstrap token rejected by provider");
        return;
    }
    tracing::info!("Sent BootstrapToken (100 sats) — accepted! Session active.");

    let mut elapsed_ms: u64 = 0;
    let mut delivered: u64 = 0;

    for i in 1..=intervals {
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;

        elapsed_ms += interval_secs * 1000;
        delivered += 1000;

        let report = Message::MeteringReport(MeteringReport {
            msg_type: MessageType::MeteringReport as u8,
            elapsed_ms,
            delivered,
            received: delivered / 2,
            new_product_id: None,
            new_pricing: None,
        });

        let responses = send_message(&http, peer_url, &report).await;

        let mut balance_exhausted = false;
        for msg in &responses {
            if let Message::Reject(r) = msg {
                if r.reason_text.as_deref() == Some("balance exhausted") {
                    balance_exhausted = true;
                }
            }
            session.handle_message(msg.clone()).await;
        }

        if balance_exhausted {
            tracing::info!("[Interval {i}] BALANCE EXHAUSTED! Sending top-up...");
            let top_up_bytes = wallet
                .create_token(Amount(50), mint_url)
                .await
                .expect("create top-up token");
            let top_up = Message::BootstrapToken(BootstrapToken {
                msg_type: MessageType::BootstrapToken as u8,
                token: top_up_bytes,
            });
            let top_up_responses = send_message(&http, peer_url, &top_up).await;
            for msg in &top_up_responses {
                session.handle_message(msg.clone()).await;
            }
            tracing::info!("Top-up accepted (50 sats), access restored");
        } else {
            tracing::info!("[Interval {i}] delivered={delivered} units, balance OK");
        }
    }

    let disconnect = Message::Disconnect(Disconnect {
        msg_type: MessageType::Disconnect as u8,
        reason_code: ReasonCode::Other,
    });
    let _ = send_message(&http, peer_url, &disconnect).await;
    tracing::info!("Disconnecting. Session complete.");
}

#[allow(clippy::missing_panics_doc)]
pub async fn run_cdk<W: Wallet + 'static>(
    peer_url: &str,
    intervals: u32,
    interval_secs: u64,
    wallet: Arc<W>,
    mint_url: &str,
) {
    let adapter = Arc::new(MockAdapter::new());
    let config = client_config();
    let mut session = PeerSession::new(wallet.clone(), adapter, config);
    let http = reqwest::Client::new();

    tracing::info!("Connecting to {peer_url}");
    let announce = session.create_announce();
    let responses = send_message(&http, peer_url, &announce).await;
    tracing::info!("Sent Announce");

    for msg in &responses {
        session.handle_message(msg.clone()).await;
    }

    let Some((product_id, option_id)) = extract_product_info(&responses) else {
        tracing::error!("No product found in provider PriceSheet");
        return;
    };
    tracing::info!("Received Announce + PriceSheet from provider");

    let accept = Message::Accept(Accept {
        msg_type: MessageType::Accept as u8,
        product_id,
        option_id,
        interval_range: IntervalRange([2500, 10000]),
        channel_funding: vec![],
    });
    let _ = send_message(&http, peer_url, &accept).await;
    tracing::info!("Selected product, sent Accept");

    let token_bytes = wallet
        .create_token(Amount(100), mint_url)
        .await
        .expect("create token");
    let bootstrap_token = Message::BootstrapToken(BootstrapToken {
        msg_type: MessageType::BootstrapToken as u8,
        token: token_bytes,
    });
    let responses = send_message(&http, peer_url, &bootstrap_token).await;
    for msg in &responses {
        session.handle_message(msg.clone()).await;
    }

    let accepted = responses.iter().any(|m| {
        matches!(
            m,
            Message::BootstrapAck(ack) if ack.status == BootstrapStatus::Accepted
        )
    });
    if !accepted {
        tracing::error!("Bootstrap token rejected by provider");
        return;
    }
    tracing::info!("Sent BootstrapToken (100 sats) — accepted! Session active.");

    let mut elapsed_ms: u64 = 0;
    let mut delivered: u64 = 0;

    for i in 1..=intervals {
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;

        elapsed_ms += interval_secs * 1000;
        delivered += 1000;

        let report = Message::MeteringReport(MeteringReport {
            msg_type: MessageType::MeteringReport as u8,
            elapsed_ms,
            delivered,
            received: delivered / 2,
            new_product_id: None,
            new_pricing: None,
        });

        let responses = send_message(&http, peer_url, &report).await;

        let mut balance_exhausted = false;
        for msg in &responses {
            if let Message::Reject(r) = msg {
                if r.reason_text.as_deref() == Some("balance exhausted") {
                    balance_exhausted = true;
                }
            }
            session.handle_message(msg.clone()).await;
        }

        if balance_exhausted {
            tracing::info!("[Interval {i}] BALANCE EXHAUSTED! Sending top-up...");
            let top_up_bytes = wallet
                .create_token(Amount(50), mint_url)
                .await
                .expect("create top-up token");
            let top_up = Message::BootstrapToken(BootstrapToken {
                msg_type: MessageType::BootstrapToken as u8,
                token: top_up_bytes,
            });
            let top_up_responses = send_message(&http, peer_url, &top_up).await;
            for msg in &top_up_responses {
                session.handle_message(msg.clone()).await;
            }
            tracing::info!("Top-up accepted (50 sats), access restored");
        } else {
            tracing::info!("[Interval {i}] delivered={delivered} units, balance OK");
        }
    }

    let disconnect = Message::Disconnect(Disconnect {
        msg_type: MessageType::Disconnect as u8,
        reason_code: ReasonCode::Other,
    });
    let _ = send_message(&http, peer_url, &disconnect).await;
    tracing::info!("Disconnecting. Session complete.");
}

// ---------------------------------------------------------------------------
// Spilman payment channel client (requires --features spilman)
// ---------------------------------------------------------------------------

#[cfg(feature = "spilman")]
async fn spilman_open_channel(
    wallet: &crate::cdk_wallet::CdkWallet,
    spilman: &SpilmanService<impl ClientStorage + 'static>,
    receiver_pubkey_hex: &str,
    mint_url: &str,
) -> String {
    let proofs_json = wallet.unspent_proofs_json().await.expect("get proofs");
    let all_proofs: Vec<CashuProof> = serde_json::from_str(&proofs_json).expect("parse proofs");

    let mut selected_proofs = Vec::new();
    let mut selected_total = 0u64;
    for proof in &all_proofs {
        if selected_total >= 1000 {
            break;
        }
        selected_proofs.push(proof.clone());
        selected_total += u64::from(proof.amount);
    }
    tracing::info!(
        "[spilman] Selected {selected_total} sat from {} proofs for channel funding",
        selected_proofs.len()
    );

    let mint_url_obj = MintUrl::from_str(mint_url).expect("parse mint URL");
    let token = CashuToken::new(mint_url_obj, selected_proofs, None, CurrencyUnit::Sat);
    let token_str = token.to_string();

    let (keyset_info_json, _) = fetch_active_keyset_info(mint_url)
        .await
        .expect("fetch keyset");

    let net = ReqwestNetworking::new();
    let open_result = spilman
        .open_channel(
            &token_str,
            receiver_pubkey_hex,
            3600,
            &keyset_info_json,
            64,
            &net,
        )
        .await
        .expect("open channel");

    tracing::info!(
        "[spilman] Channel opened: id={} capacity={} sat",
        open_result.channel_id,
        open_result.capacity
    );
    open_result.channel_id
}

#[cfg(feature = "spilman")]
#[allow(clippy::too_many_arguments)]
async fn spilman_send_payment(
    http: &reqwest::Client,
    peer_url: &str,
    session: &mut PeerSession<crate::cdk_wallet::CdkWallet, MockAdapter>,
    spilman: &SpilmanService<impl ClientStorage + 'static>,
    channel_id: &str,
    interval_index: u32,
    current_balance: u64,
    elapsed_ms: u64,
    delivered: u64,
    payment_per_interval: u64,
) {
    let payment = if interval_index == 1 {
        spilman.create_payment_with_funding(channel_id, current_balance)
    } else {
        spilman.create_payment(channel_id, current_balance)
    }
    .expect("create spilman payment");

    let (params_bytes, proofs_bytes) = if interval_index == 1 {
        let p = serde_json::to_vec(
            payment
                .params
                .as_ref()
                .expect("payment with funding must have params"),
        )
        .expect("serialize params");
        let f = serde_json::to_vec(
            payment
                .funding_proofs
                .as_ref()
                .expect("payment with funding must have proofs"),
        )
        .expect("serialize proofs");
        (Some(p), Some(f))
    } else {
        (None, None)
    };

    let report = Message::MeteringReport(MeteringReport {
        msg_type: MessageType::MeteringReport as u8,
        elapsed_ms,
        delivered,
        received: delivered / 2,
        new_product_id: None,
        new_pricing: None,
    });

    let channel_bytes: [u8; 32] = decode_hex(&payment.channel_id).expect("decode channel_id");
    let sig_bytes: [u8; 64] = decode_hex(&payment.signature).expect("decode signature");

    let balance_update = Message::BalanceUpdate(BalanceUpdate {
        msg_type: MessageType::BalanceUpdate as u8,
        channel_id: Hash32(channel_bytes),
        cumulative_balance: payment.balance,
        balance_signature: TgSignature(sig_bytes),
        net_amount: payment_per_interval,
        channel_params_json: params_bytes,
        funding_proofs_json: proofs_bytes,
    });

    let report_responses = send_message(http, peer_url, &report).await;
    let balance_responses = send_message(http, peer_url, &balance_update).await;

    for msg in &report_responses {
        session.handle_message(msg.clone()).await;
    }
    for msg in &balance_responses {
        if let Message::BalanceAck(ack) = msg {
            tracing::info!(
                "[spilman] Interval {interval_index}: BalanceAck accepted_balance={}",
                ack.accepted_balance
            );
        }
        session.handle_message(msg.clone()).await;
    }
    tracing::info!(
        "[spilman] Interval {interval_index}: delivered={delivered} spilman_balance={current_balance}"
    );
}

#[cfg(feature = "spilman")]
#[allow(
    clippy::missing_panics_doc,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    deprecated
)]
pub async fn run_spilman(
    peer_url: &str,
    intervals: u32,
    interval_secs: u64,
    wallet: Arc<crate::cdk_wallet::CdkWallet>,
    spilman: Arc<SpilmanService<impl ClientStorage + 'static>>,
    receiver_pubkey_hex: &str,
    mint_url: &str,
    no_close: bool,
) {
    let adapter = Arc::new(MockAdapter::new());
    let config = client_config();
    let mut session = PeerSession::new(wallet.clone(), adapter, config);
    let http = reqwest::Client::new();

    tracing::info!("[spilman] Connecting to {peer_url}");
    tracing::info!("[spilman] Minting tokens from {mint_url}...");
    wallet
        .mint_test_tokens(2000)
        .await
        .expect("mint test tokens");
    let bal = wallet.total_balance().await.expect("balance check");
    tracing::info!("[spilman] Wallet balance: {bal} sat");

    let announce = session.create_announce();
    let responses = send_message(&http, peer_url, &announce).await;

    for msg in &responses {
        session.handle_message(msg.clone()).await;
    }

    let Some((product_id, option_id)) = extract_product_info(&responses) else {
        tracing::error!("[spilman] No product found in provider PriceSheet");
        return;
    };

    let accept = Message::Accept(Accept {
        msg_type: MessageType::Accept as u8,
        product_id,
        option_id,
        interval_range: IntervalRange([2500, 10000]),
        channel_funding: vec![],
    });
    let _ = send_message(&http, peer_url, &accept).await;

    let token_bytes = wallet
        .create_token(Amount(100), mint_url)
        .await
        .expect("create bootstrap token");
    let bootstrap = Message::BootstrapToken(BootstrapToken {
        msg_type: MessageType::BootstrapToken as u8,
        token: token_bytes,
    });
    let responses = send_message(&http, peer_url, &bootstrap).await;
    for msg in &responses {
        session.handle_message(msg.clone()).await;
    }

    let accepted = responses.iter().any(
        |m| matches!(m, Message::BootstrapAck(ack) if ack.status == BootstrapStatus::Accepted),
    );
    if !accepted {
        tracing::error!("[spilman] Bootstrap token rejected");
        return;
    }
    tracing::info!("[spilman] Bootstrap accepted. Opening Spilman channel...");

    let channel_id = spilman_open_channel(&wallet, &spilman, receiver_pubkey_hex, mint_url).await;

    let mut elapsed_ms: u64 = 0;
    let mut delivered: u64 = 0;
    let mut current_balance: u64 = 0;
    let payment_per_interval: u64 = 10;

    for i in 1..=intervals {
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
        elapsed_ms += interval_secs * 1000;
        delivered += 1000;
        current_balance += payment_per_interval;
        spilman_send_payment(
            &http,
            peer_url,
            &mut session,
            &spilman,
            &channel_id,
            i,
            current_balance,
            elapsed_ms,
            delivered,
            payment_per_interval,
        )
        .await;
    }

    if no_close {
        tracing::info!(
            "[spilman] --no-close: skipping cooperative close, channel={channel_id} balance={current_balance}"
        );
        return;
    }

    tracing::info!("[spilman] Requesting cooperative close at balance={current_balance}...");
    let close_payment = spilman
        .request_cooperative_close(&channel_id, current_balance)
        .expect("create close request");

    let close_ch: [u8; 32] = decode_hex(&close_payment.channel_id).unwrap_or([0u8; 32]);
    let close_sig: [u8; 64] = decode_hex(&close_payment.signature).unwrap_or([0u8; 64]);

    let (close_params_json, close_proofs_json) = spilman
        .create_payment_with_funding(&channel_id, current_balance)
        .ok()
        .map_or((None, None), |fp| {
            let p = fp
                .params
                .map(|v| serde_json::to_vec(&v).expect("serialize params"));
            let f = fp
                .funding_proofs
                .map(|proofs| serde_json::to_vec(&proofs).expect("serialize proofs"));
            (p, f)
        });

    let channel_close = Message::ChannelClose(ChannelClose {
        msg_type: MessageType::ChannelClose as u8,
        channel_id: Hash32(close_ch),
        final_balance: close_payment.balance,
        final_signature: TgSignature(close_sig),
        reason: CloseReason::Normal,
        channel_params_json: close_params_json,
        funding_proofs_json: close_proofs_json,
    });

    let close_responses = send_message(&http, peer_url, &channel_close).await;
    for msg in &close_responses {
        if let Message::CloseAck(ack) = msg {
            tracing::info!(
                "[spilman] CloseAck: accepted_balance={}",
                ack.accepted_balance
            );
        }
        session.handle_message(msg.clone()).await;
    }

    let disconnect = Message::Disconnect(Disconnect {
        msg_type: MessageType::Disconnect as u8,
        reason_code: ReasonCode::Other,
    });
    let _ = send_message(&http, peer_url, &disconnect).await;
    tracing::info!("[spilman] Session complete. Channel closed cooperatively.");
}
