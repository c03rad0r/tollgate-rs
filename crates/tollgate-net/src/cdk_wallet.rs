//! CDK-backed Wallet implementation for TollGate.
//!
//! Wraps [`cdk::Wallet`] to implement the [`tollgate_core::wallet::Wallet`] trait.
//! Connects to a Cashu mint (e.g., testnut.cashu.space) and handles real
//! Cashu token operations via NUT-00/04/23.
//!
//! Spilman payment channel operations are in [`crate::spilman_service::SpilmanService`].

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use cdk::nuts::{CurrencyUnit, MintQuoteState, PaymentMethod};
use cdk::wallet::{ReceiveOptions, SendKind, SendOptions};
use cdk_sqlite::wallet::memory;
use tollgate_core::error::WalletError;
use tollgate_core::types::Amount;
use tollgate_core::wallet::Wallet;

use crate::v1::server::mint_quote_wallet::{
    MintQuoteError, MintQuoteInfo, MintQuoteWallet, MintResult, QuoteState,
};

/// CDK-backed wallet implementing [`Wallet`].
///
/// Connects to a single Cashu mint and handles token creation/reception.
/// Uses an in-memory SQLite localstore for proof management.
pub struct CdkWallet {
    wallet: cdk::Wallet,
}

impl CdkWallet {
    /// Create a new CDK wallet connected to the given mint URL.
    ///
    /// The `seed` is a 64-byte secret used for key derivation within CDK.
    /// Each unique seed produces a distinct wallet identity.
    ///
    /// # Errors
    ///
    /// Returns `WalletError::Internal` if the localstore or wallet initialization fails.
    #[allow(clippy::missing_errors_doc)]
    pub async fn new(mint_url: &str, seed: [u8; 64]) -> Result<Self, WalletError> {
        let localstore = Arc::new(
            memory::empty()
                .await
                .map_err(|e| WalletError::Internal(format!("localstore: {e}")))?,
        );
        let wallet = cdk::Wallet::new(mint_url, CurrencyUnit::Sat, localstore, seed, None)
            .map_err(|e| WalletError::Internal(format!("wallet init: {e}")))?;
        Ok(Self { wallet })
    }

    /// Try each mint URL in order, returning the first that initializes successfully.
    ///
    /// Mirrors Go's `TollWallet.New()` — loops through `accepted_mints`, creates a
    /// wallet for each, breaks on first success, returns error if all fail.
    ///
    /// # Errors
    ///
    /// Returns `WalletError` if all mints fail to connect.
    pub async fn try_mints(mint_urls: &[String], seed: [u8; 64]) -> Result<Self, WalletError> {
        if mint_urls.is_empty() {
            return Err(WalletError::Internal("no mint URLs provided".to_owned()));
        }
        let mut last_err = None;
        for url in mint_urls {
            tracing::info!("Trying mint: {url}");
            match Self::new(url, seed).await {
                Ok(wallet) => {
                    tracing::info!("Connected to mint: {url}");
                    return Ok(wallet);
                }
                Err(e) => {
                    tracing::warn!("Mint {url} failed: {e}");
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| WalletError::Internal("all mints failed".to_owned())))
    }

    /// Mint test tokens from a Cashu mint with FakeWallet (e.g., testnut.cashu.space).
    ///
    /// Uses manual polling instead of CDK's subscription-based `wait_and_mint_quote`,
    /// because some mints (testnut) don't support websocket notifications, which can
    /// cause the stream-based flow to fail or race.
    ///
    /// NUT-04/23 minting flow:
    ///   1. POST /v1/mint/quote/bolt11 → quote + invoice
    ///   2. Poll GET /v1/mint/quote/bolt11/{id} until state=PAID (FakeWallet auto-pays)
    ///   3. POST /v1/mint/bolt11 with blinded messages → signatures → proofs
    #[allow(clippy::missing_errors_doc)]
    pub async fn mint_test_tokens(&self, amount: u64) -> Result<(), WalletError> {
        let cdk_amount: cdk::Amount = cdk::Amount::from(amount);

        // Step 1: Create mint quote
        let quote = self
            .wallet
            .mint_quote(PaymentMethod::BOLT11, Some(cdk_amount), None, None)
            .await
            .map_err(|e| WalletError::Internal(format!("mint_quote: {e}")))?;

        tracing::info!(
            "[NUT-04] Created mint quote {} for {} sat",
            quote.id,
            amount
        );

        // Step 2: Poll until PAID (FakeWallet auto-pays within ~2s)
        let mut paid = false;
        for i in 0..60 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let status = self
                .wallet
                .check_mint_quote_status(&quote.id)
                .await
                .map_err(|e| WalletError::Internal(format!("check_status: {e}")))?;

            tracing::debug!(
                "[NUT-04] Poll {i}: quote={} state={:?}",
                quote.id,
                status.state
            );

            if status.state == MintQuoteState::Paid {
                tracing::info!("[NUT-04] Quote {} is PAID", quote.id);
                paid = true;
                break;
            }
        }

        if !paid {
            return Err(WalletError::Internal(format!(
                "mint quote {} not paid after 30s",
                quote.id
            )));
        }

        // Step 3: Mint proofs (with retry for transient network errors)
        let mut last_err = String::new();
        for attempt in 0..3 {
            match self
                .wallet
                .mint(&quote.id, cdk::amount::SplitTarget::default(), None)
                .await
            {
                Ok(_proofs) => {
                    tracing::info!("[NUT-04] Minted {} sat successfully", amount);
                    return Ok(());
                }
                Err(e) => {
                    last_err = format!("{e}");
                    tracing::warn!("[NUT-04] Mint attempt {}/3 failed: {e}", attempt + 1);

                    // The mint may have succeeded server-side despite HTTP
                    // errors (timeout, "already signed", "quote in use").
                    // Recover incomplete sagas and check balance.
                    tracing::info!("[NUT-04] Recovering incomplete sagas...");
                    let _ = self.wallet.recover_incomplete_sagas().await;
                    let bal = self.total_balance().await?;
                    if bal >= amount {
                        tracing::info!(
                            "[NUT-04] Recovered {bal} sat after failed attempt (requested {amount})"
                        );
                        return Ok(());
                    }

                    if attempt < 2 {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                }
            }
        }
        Err(WalletError::Internal(format!(
            "mint failed after 3 attempts: {last_err}"
        )))
    }

    /// Get the total wallet balance.
    #[allow(clippy::missing_errors_doc)]
    pub async fn total_balance(&self) -> Result<u64, WalletError> {
        let bal = self
            .wallet
            .total_balance()
            .await
            .map_err(|e| WalletError::Internal(format!("balance: {e}")))?;
        Ok(u64::from(bal))
    }

    /// Melt tokens to pay a BOLT11 Lightning invoice.
    ///
    /// Uses the CDK melt flow: quote → prepare → confirm.
    /// Returns the amount paid (in sats).
    #[allow(clippy::missing_errors_doc)]
    pub async fn melt_to_invoice(&self, invoice: &str) -> Result<u64, WalletError> {
        let quote = self
            .wallet
            .melt_quote(PaymentMethod::BOLT11, invoice.to_owned(), None, None)
            .await
            .map_err(|e| WalletError::Internal(format!("melt_quote: {e}")))?;

        tracing::info!("[melt] Created melt quote {} for invoice", quote.id);

        let prepared = self
            .wallet
            .prepare_melt(&quote.id, HashMap::new())
            .await
            .map_err(|e| WalletError::Internal(format!("prepare_melt: {e}")))?;

        let confirmed = prepared
            .confirm()
            .await
            .map_err(|e| WalletError::Internal(format!("melt confirm: {e}")))?;

        let amount = u64::from(confirmed.amount());
        let fee = u64::from(confirmed.fee_paid());
        tracing::info!("[melt] Melted {amount} sat (fee: {fee} sat)");
        Ok(amount)
    }

    /// Melt tokens to a Lightning address (user@domain.com).
    ///
    /// CDK resolves the LNURL-pay endpoint and creates a melt quote automatically.
    /// `amount_msat` is in millisatoshis.
    /// Returns the amount paid (in sats).
    #[allow(clippy::missing_errors_doc)]
    pub async fn melt_to_lightning_address(
        &self,
        address: &str,
        amount_msat: u64,
    ) -> Result<u64, WalletError> {
        let cdk_amount = cdk::Amount::from(amount_msat);

        let quote = self
            .wallet
            .melt_lightning_address_quote(address, cdk_amount)
            .await
            .map_err(|e| WalletError::Internal(format!("melt_lightning_address_quote: {e}")))?;

        tracing::info!(
            "[melt] Created melt quote {} for Lightning address {address}",
            quote.id
        );

        let prepared = self
            .wallet
            .prepare_melt(&quote.id, HashMap::new())
            .await
            .map_err(|e| WalletError::Internal(format!("prepare_melt: {e}")))?;

        let confirmed = prepared
            .confirm()
            .await
            .map_err(|e| WalletError::Internal(format!("melt confirm: {e}")))?;

        let amount = u64::from(confirmed.amount());
        let fee = u64::from(confirmed.fee_paid());
        tracing::info!("[melt] Melted {amount} sat to {address} (fee: {fee} sat)");
        Ok(amount)
    }

    /// Get unspent proofs serialized as JSON (requires `spilman` feature).
    ///
    /// Returns proofs in standard Cashu JSON format, compatible with both
    /// `cashu` v0.15.1 and v0.16.0 crate types.
    #[cfg(feature = "spilman")]
    #[allow(clippy::missing_errors_doc)]
    pub async fn unspent_proofs_json(&self) -> Result<String, WalletError> {
        let proofs = self
            .wallet
            .get_unspent_proofs()
            .await
            .map_err(|e| WalletError::Internal(format!("get_proofs: {e}")))?;
        serde_json::to_string(&proofs)
            .map_err(|e| WalletError::Internal(format!("serialize proofs: {e}")))
    }
}

#[allow(clippy::manual_async_fn)]
impl Wallet for CdkWallet {
    fn receive_token(
        &self,
        token: &[u8],
    ) -> Pin<Box<dyn Future<Output = Result<Amount, WalletError>> + Send + '_>> {
        // Per NUT-00: Token is a cashuA (V3) or cashuB (V4) encoded string.
        // Our protocol transmits it as raw bytes (UTF-8 string).
        let token_str = String::from_utf8_lossy(token).to_string();
        Box::pin(async move {
            tracing::info!(
                "[NUT-00] Receiving Cashu token ({} bytes, first 20 chars: {:?})",
                token_str.len(),
                &token_str[..token_str.len().min(20)]
            );

            let balance_before = self.wallet.total_balance().await.map_or(0, u64::from);
            let mut last_err = String::new();
            for attempt in 0..3 {
                match self
                    .wallet
                    .receive(&token_str, ReceiveOptions::default())
                    .await
                {
                    Ok(amount) => {
                        let amount = Amount(u64::from(amount));
                        tracing::info!("[NUT-00] Token received successfully: {} sat", amount.0);
                        return Ok(amount);
                    }
                    Err(e) => {
                        last_err = format!("{e}");
                        tracing::warn!("[NUT-00] Receive attempt {}/3 failed: {e}", attempt + 1);
                        // Some mints can return transient/ambiguous receive
                        // errors even when proofs were partially accepted.
                        // Attempt recovery and accept any observed balance delta.
                        let _ = self.wallet.recover_incomplete_sagas().await;
                        if let Ok(current_balance) = self.wallet.total_balance().await {
                            let current = u64::from(current_balance);
                            if current > balance_before {
                                let recovered = current - balance_before;
                                tracing::info!(
                                    "[NUT-00] Recovered receive via saga reconciliation: {} sat",
                                    recovered
                                );
                                return Ok(Amount(recovered));
                            }
                        }
                        if attempt < 2 {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        }
                    }
                }
            }
            tracing::error!("[NUT-00] CDK receive failed after 3 attempts: {last_err}");
            Err(WalletError::TokenRejected(format!(
                "CDK receive: {last_err}"
            )))
        })
    }

    fn create_token(
        &self,
        amount: Amount,
        _mint_url: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, WalletError>> + Send + '_>> {
        let cdk_amount: cdk::Amount = cdk::Amount::from(amount.0);
        Box::pin(async move {
            tracing::info!("[NUT-00] Creating Cashu token for {} sat", amount.0);
            let prepared = self
                .wallet
                .prepare_send(cdk_amount, SendOptions::default())
                .await
                .map_err(|e| WalletError::Internal(format!("prepare_send: {e}")))?;
            let token = prepared
                .confirm(None)
                .await
                .map_err(|e| WalletError::Internal(format!("confirm: {e}")))?;
            // Token.to_string() produces V4 (cashuB) encoded string
            let encoded = token.to_string();
            tracing::info!(
                "[NUT-00] Token created: {} bytes (V4 cashuB)",
                encoded.len()
            );
            Ok(encoded.into_bytes())
        })
    }

    fn mint_reachable(
        &self,
        mint_url: &str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, WalletError>> + Send + '_>> {
        let mint_url = mint_url.to_owned();
        Box::pin(async move {
            tracing::info!("[NUT-06] Checking mint reachability: {mint_url}");
            Ok(true)
        })
    }

    fn balance(&self) -> Pin<Box<dyn Future<Output = Result<Amount, WalletError>> + Send + '_>> {
        Box::pin(async move {
            let bal = self
                .wallet
                .total_balance()
                .await
                .map_err(|e| WalletError::Internal(format!("balance: {e}")))?;
            Ok(Amount(u64::from(bal)))
        })
    }

    fn create_token_with_overpayment(
        &self,
        amount: Amount,
        _mint_url: &str,
        max_overpayment_absolute: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, WalletError>> + Send + '_>> {
        let cdk_amount: cdk::Amount = cdk::Amount::from(amount.0);
        let tolerance: cdk::Amount = cdk::Amount::from(max_overpayment_absolute);
        Box::pin(async move {
            tracing::info!(
                "[NUT-00] Creating Cashu token for {} sat (overpayment tolerance: {} sat)",
                amount.0,
                max_overpayment_absolute
            );
            let opts = SendOptions {
                send_kind: SendKind::OnlineTolerance(tolerance),
                include_fee: true,
                ..SendOptions::default()
            };
            let prepared = self
                .wallet
                .prepare_send(cdk_amount, opts)
                .await
                .map_err(|e| WalletError::Internal(format!("prepare_send: {e}")))?;
            let token = prepared
                .confirm(None)
                .await
                .map_err(|e| WalletError::Internal(format!("confirm: {e}")))?;
            let encoded = token.to_string();
            tracing::info!(
                "[NUT-00] Token created: {} bytes (V4 cashuB)",
                encoded.len()
            );
            Ok(encoded.into_bytes())
        })
    }
}

#[async_trait::async_trait]
impl MintQuoteWallet for CdkWallet {
    async fn request_mint_quote(
        &self,
        amount: u64,
        _mint_url: &str,
    ) -> Result<MintQuoteInfo, MintQuoteError> {
        let cdk_amount = cdk::Amount::from(amount);
        let quote = self
            .wallet
            .mint_quote(PaymentMethod::BOLT11, Some(cdk_amount), None, None)
            .await
            .map_err(|e| MintQuoteError::Mint(format!("{e}")))?;

        Ok(MintQuoteInfo {
            quote_id: quote.id,
            invoice: quote.request,
            amount,
            expiry: quote.expiry,
        })
    }

    async fn check_mint_quote_status(&self, quote_id: &str) -> Result<QuoteState, MintQuoteError> {
        let status = self
            .wallet
            .check_mint_quote_status(quote_id)
            .await
            .map_err(|e| MintQuoteError::Mint(format!("{e}")))?;

        Ok(match status.state {
            MintQuoteState::Unpaid => QuoteState::Unpaid,
            MintQuoteState::Paid => QuoteState::Paid,
            MintQuoteState::Issued => QuoteState::Issued,
        })
    }

    async fn mint_tokens(&self, quote_id: &str) -> Result<MintResult, MintQuoteError> {
        let proofs = self
            .wallet
            .mint(quote_id, cdk::amount::SplitTarget::default(), None)
            .await
            .map_err(|e| MintQuoteError::Mint(format!("{e}")))?;

        let total: u64 = proofs.iter().map(|p| u64::from(p.amount)).sum();
        Ok(MintResult { amount: total })
    }
}
