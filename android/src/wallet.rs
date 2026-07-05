//! TollGate wallet: Nostr identity + Cashu NIP-60 wallet, exposed as a UniFFI
//! Object to Kotlin.
//!
//! NIP-60 ("Cashu Wallets", <https://github.com/nostr-protocol/nips/blob/master/60.md>)
//! stores spendable Cashu proofs as encrypted Nostr events:
//!   * kind 7374 — wallet info (mints/relays);
//!   * kind 7375 — proofs (NIP-44-encrypted JSON blob).
//!
//! This module hand-rolls the small subset the app needs (sync balance, deposit
//! a token, verify a token against its mint) on top of `nostr-sdk` + the
//! workspace `cashu` crate, rather than pulling a heavy NIP-60 dependency.

use std::sync::Mutex;

use nostr_sdk::nips::nip19::ToBech32;
use nostr_sdk::prelude::*;
use tokio::runtime::Runtime;

use crate::error::TollgateError;
use crate::keys::KeyPair;

/// NIP-60 kind: proofs event.
const KIND_PROOFS: u16 = 7375;

/// The wallet. Constructed cheaply (no I/O); attach an identity with
/// [`Wallet::set_identity`] and then `connect_and_sync`.
#[derive(uniffi::Object)]
pub struct Wallet {
    rt: Runtime,
    state: Mutex<WalletState>,
}

struct WalletState {
    keys: Option<Keys>,
    client: Option<Client>,
    relays: Vec<String>,
    mints: Vec<String>,
    /// Cached spendable balance in sats, refreshed by the last `connect_and_sync`.
    balance_sats: u64,
}

#[uniffi::export]
impl Wallet {
    /// `relays` — Nostr relays for NIP-60 wallet events (e.g. wss://relay.example).
    /// `mints`  — Cashu mints the wallet will accept tokens from.
    #[uniffi::constructor]
    pub fn new(relays: Vec<String>, mints: Vec<String>) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime builds");
        Wallet {
            rt,
            state: Mutex::new(WalletState {
                keys: None,
                client: None,
                relays,
                mints,
                balance_sats: 0,
            }),
        }
    }

    /// Attach / replace the Nostr identity and build the relay client.
    pub fn set_identity(&self, kp: KeyPair) -> Result<(), TollgateError> {
        let keys = kp.to_keys()?;
        let client = Client::builder().signer(keys.clone()).build();
        let mut st = self
            .state
            .lock()
            .map_err(|e| TollgateError::internal_locked(&e))?;
        st.keys = Some(keys);
        st.client = Some(client);
        Ok(())
    }

    /// Bech32 npub of the current identity, or empty string if none.
    pub fn npub(&self) -> String {
        self.state
            .lock()
            .map(|st| {
                st.keys
                    .as_ref()
                    .map(|k| k.public_key().to_bech32().unwrap_or_default())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// Connect to configured relays and sync the NIP-60 wallet: fetch every
    /// kind-7375 proofs event authored by us, decrypt each, sum spendable
    /// proofs. Returns the refreshed balance in sats.
    pub fn connect_and_sync(&self) -> Result<u64, TollgateError> {
        // Snapshot what we need under the lock, run I/O outside it.
        let (client, keys, relays) = {
            let st = self
                .state
                .lock()
                .map_err(|e| TollgateError::internal_locked(&e))?;
            let client = st
                .client
                .clone()
                .ok_or_else(|| TollgateError::not_ready("no identity — call set_identity first"))?;
            let keys = st
                .keys
                .clone()
                .ok_or_else(|| TollgateError::not_ready("no identity"))?;
            (client, keys, st.relays.clone())
        };

        let pubkey = keys.public_key();
        let balance = self.rt.block_on(async move {
            for url in &relays {
                let _ = client.add_relay(url).await;
            }
            client.connect().await;

            // Ask every connected relay for our proofs events, wait briefly.
            let filter = Filter::new()
                .author(pubkey)
                .kind(Kind::Custom(KIND_PROOFS));
            let events = client
                .fetch_events(filter, std::time::Duration::from_secs(8))
                .await
                .map_err(|e| TollgateError::nostr(format!("fetch events: {e}")))?;

            let mut total: u64 = 0;
            for ev in events.into_iter() {
                // Each 7375 content is NIP-44-encrypted JSON of proofs.
                let plaintext = match decrypt_nip60(&keys, &ev) {
                    Ok(t) => t,
                    Err(_) => continue, // not for us / malformed — skip
                };
                total += sum_proofs_sats(&plaintext);
            }
            Ok::<u64, TollgateError>(total)
        })?;

        // Persist the refreshed balance.
        {
            let mut st = self
                .state
                .lock()
                .map_err(|e| TollgateError::internal_locked(&e))?;
            st.balance_sats = balance;
        }
        Ok(balance)
    }

    /// Last-known spendable balance in sats (from the most recent `connect_and_sync`).
    pub fn balance_sats(&self) -> u64 {
        self.state
            .lock()
            .map(|st| st.balance_sats)
            .unwrap_or(0)
    }

    /// Verify a Cashu token against its mint (NUT-07 check-state) and return its
    /// value in sats. Reuses the same `cashu` crate + checkstate flow as
    /// `tollgate-net::wallet::BootstrapWallet`. Does not move funds.
    pub fn verify_token(&self, token_str: String) -> Result<u64, TollgateError> {
        let mints = self
            .state
            .lock()
            .map_err(|e| TollgateError::internal_locked(&e))?
            .mints
            .clone();
        self.rt.block_on(async move { verify_cashu_token(&token_str, &mints).await })
    }

    /// Deposit a Cashu token into this NIP-60 wallet: parse it, encrypt its
    /// proofs, publish a kind-7375 event to our relays, and bump the balance.
    /// Returns the new total balance.
    pub fn deposit_token(&self, token_str: String) -> Result<u64, TollgateError> {
        let (client, keys) = {
            let st = self
                .state
                .lock()
                .map_err(|e| TollgateError::internal_locked(&e))?;
            let client = st
                .client
                .clone()
                .ok_or_else(|| TollgateError::not_ready("no identity — call set_identity first"))?;
            let keys = st
                .keys
                .clone()
                .ok_or_else(|| TollgateError::not_ready("no identity"))?;
            (client, keys)
        };

        let added = self.rt.block_on(async move {
            let token: cashu::Token = token_str
                .parse()
                .map_err(|e| TollgateError::invalid(format!("invalid Cashu token: {e}")))?;
            let amount_sat: u64 = token
                .value()
                .map_err(|e| TollgateError::invalid(format!("token has no value: {e}")))?
                .into();

            // NIP-60 proofs payload: the token's proofs serialised to JSON,
            // encrypted with NIP-44 to ourselves.
            let payload = serde_json::to_string(&token)
                .map_err(|e| TollgateError::internal(format!("serialise token: {e}")))?;
            let encrypted = nip44::encrypt(
                keys.secret_key(),
                &keys.public_key(),
                &payload,
                nip44::Version::V2,
            )
            .map_err(|e| TollgateError::nostr(format!("nip44 encrypt: {e}")))?;

            let builder = EventBuilder::new(Kind::Custom(KIND_PROOFS), encrypted);
            client
                .send_event_builder(builder)
                .await
                .map_err(|e| TollgateError::nostr(format!("publish proofs event: {e}")))?;
            Ok::<u64, TollgateError>(amount_sat)
        })?;

        let mut st = self
            .state
            .lock()
            .map_err(|e| TollgateError::internal_locked(&e))?;
        st.balance_sats += added;
        Ok(st.balance_sats)
    }
}

// ---------------------------------------------------------------------------
// NIP-60 + Cashu helpers (no FFI surface).
// ---------------------------------------------------------------------------

/// Decrypt a kind-7375 event's content with our NIP-44 key. Returns the
/// plaintext on success.
fn decrypt_nip60(keys: &Keys, ev: &Event) -> Result<String, TollgateError> {
    nip44::decrypt(keys.secret_key(), &ev.pubkey, &ev.content)
        .map_err(|e| TollgateError::nostr(format!("nip44 decrypt: {e}")))
}

/// Sum the `amount` field of every proof object in a NIP-60 proofs JSON blob.
fn sum_proofs_sats(plaintext: &str) -> u64 {
    let v: serde_json::Value = match serde_json::from_str(plaintext) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    // NIP-60 proofs payloads come in a few shapes; sum anything that looks like
    // {"proofs":[{"amount":N,...}]} or a token's token[].proofs[].
    fn walk(node: &serde_json::Value, acc: &mut u64) {
        match node {
            serde_json::Value::Object(map) => {
                if let Some(amount) = map.get("amount").and_then(|a| a.as_u64()) {
                    *acc += amount;
                }
                for (_, v) in map {
                    walk(v, acc);
                }
            }
            serde_json::Value::Array(arr) => {
                for v in arr {
                    walk(v, acc);
                }
            }
            _ => {}
        }
    }
    let mut total = 0u64;
    walk(&v, &mut total);
    total
}

/// Parse + NUT-07-verify a Cashu token. Mirrors `tollgate-net::wallet`.
async fn verify_cashu_token(token_str: &str, accepted_mints: &[String]) -> Result<u64, TollgateError> {
    use std::collections::HashSet;

    let token: cashu::Token = token_str
        .parse()
        .map_err(|e| TollgateError::invalid(format!("invalid Cashu token: {e}")))?;

    let mint_url = token
        .mint_url()
        .map_err(|e| TollgateError::invalid(format!("token has no mint URL: {e}")))?
        .to_string();
    let mint_base = mint_url.trim_end_matches('/');

    if !accepted_mints.is_empty() {
        let allow: HashSet<&str> = accepted_mints
            .iter()
            .map(|s| s.trim_end_matches('/'))
            .collect();
        if !allow.contains(mint_base) {
            return Err(TollgateError::mint(format!("mint {mint_url} not accepted")));
        }
    }

    let amount_sat: u64 = token
        .value()
        .map_err(|e| TollgateError::invalid(format!("could not sum token value: {e}")))?
        .into();

    // Y-values (compressed blinded-secret pubkeys) for NUT-07 check-state.
    let ys = token_proof_ys(&token);
    if ys.is_empty() {
        return Err(TollgateError::invalid("token contains no proofs"));
    }

    let client = reqwest::Client::new();
    let resp: serde_json::Value = client
        .post(format!("{mint_base}/v1/checkstate"))
        .json(&serde_json::json!({ "Ys": ys }))
        .send()
        .await
        .map_err(|e| TollgateError::mint(format!("checkstate request: {e}")))?
        .error_for_status()
        .map_err(|e| TollgateError::mint(format!("mint error: {e}")))?
        .json()
        .await
        .map_err(|e| TollgateError::mint(format!("bad mint response: {e}")))?;

    let states = resp["states"]
        .as_array()
        .ok_or_else(|| TollgateError::mint("mint response missing 'states'"))?;
    for state in states {
        let s = state["state"].as_str().unwrap_or("");
        if s != "UNSPENT" {
            return Err(TollgateError::mint(format!("proof already spent (state: {s})")));
        }
    }
    Ok(amount_sat)
}

/// Extract the `C` (blinded secret pubkey) hex strings from a token's proofs —
/// the Y-values the mint uses for NUT-07 state lookup. Same shape as
/// `tollgate-net::wallet::token_proof_ys`.
fn token_proof_ys(token: &cashu::Token) -> Vec<String> {
    match token {
        cashu::Token::TokenV3(t) => t
            .token
            .iter()
            .flat_map(|entry| entry.proofs.iter().map(|p| p.c.to_string()))
            .collect(),
        cashu::Token::TokenV4(t) => t
            .token
            .iter()
            .flat_map(|entry| entry.proofs.iter().map(|p| p.c.to_string()))
            .collect(),
    }
}

impl TollgateError {
    fn internal_locked<T>(e: &std::sync::PoisonError<T>) -> Self {
        TollgateError::Internal {
            detail: format!("state lock poisoned: {e}"),
        }
    }
}
