//! Adapter wrapping `MerchantProvider` so the CLI server can call wallet operations
//! via the dyn-compatible `CliWallet` trait.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use super::commands::CliWallet;
use crate::v1::server::MerchantProvider;

/// Wraps `Arc<MerchantProvider>` + the list of configured mint URLs so the CLI
/// server can call wallet operations.
///
/// `MerchantProvider` exposes `get()` returning the current `Arc<dyn Wallet>`,
/// which means this adapter correctly follows wallet swaps (e.g. when degraded
/// mode recovers to a real CDK wallet).
///
/// `get_mint_balances` returns one entry per configured mint URL. For single-mint
/// CDK wallets (the current production case), the first entry receives the full
/// balance and the rest receive 0. This matches what we can determine without
/// per-mint accounting in the underlying `Wallet` trait.
pub struct MerchantWalletAdapter {
    merchant: Arc<MerchantProvider>,
    mint_urls: Vec<String>,
}

impl MerchantWalletAdapter {
    pub fn new(merchant: Arc<MerchantProvider>, mint_urls: Vec<String>) -> Self {
        Self {
            merchant,
            mint_urls,
        }
    }
}

#[async_trait]
impl CliWallet for MerchantWalletAdapter {
    async fn balance(&self) -> Result<u64, String> {
        let wallet = self.merchant.get();
        wallet
            .balance()
            .await
            .map(|a| a.0)
            .map_err(|e| e.to_string())
    }

    async fn receive_token(&self, token: &str) -> Result<u64, String> {
        let wallet = self.merchant.get();
        wallet
            .receive_token(token.as_bytes())
            .await
            .map(|a| a.0)
            .map_err(|e| e.to_string())
    }

    async fn create_token(&self, amount: u64, mint_url: &str) -> Result<String, String> {
        let wallet = self.merchant.get();
        let bytes = wallet
            .create_token(tollgate_core::types::Amount(amount), mint_url)
            .await
            .map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&bytes).to_string())
    }

    async fn get_mint_balances(&self) -> HashMap<String, u64> {
        let wallet = self.merchant.get();
        let total = wallet.balance().await.map_or(0, |a| a.0);
        let mut result = HashMap::new();
        for (i, url) in self.mint_urls.iter().enumerate() {
            result.insert(url.clone(), if i == 0 { total } else { 0 });
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;
    use tollgate_core::error::WalletError;
    use tollgate_core::types::Amount;
    use tollgate_core::wallet::Wallet;

    struct MockWallet {
        balance: Mutex<u64>,
        last_token: Mutex<Option<Vec<u8>>>,
        fail_balance: bool,
    }

    impl MockWallet {
        fn new(balance: u64) -> Self {
            Self {
                balance: Mutex::new(balance),
                last_token: Mutex::new(None),
                fail_balance: false,
            }
        }
        fn failing() -> Self {
            Self {
                balance: Mutex::new(0),
                last_token: Mutex::new(None),
                fail_balance: true,
            }
        }
    }

    impl Wallet for MockWallet {
        fn receive_token(
            &self,
            token: &[u8],
        ) -> Pin<Box<dyn Future<Output = Result<Amount, WalletError>> + Send + '_>> {
            let bytes = token.to_vec();
            Box::pin(async move {
                *self.last_token.lock().unwrap() = Some(bytes.clone());
                let amt = bytes.len() as u64;
                *self.balance.lock().unwrap() += amt;
                Ok(Amount(amt))
            })
        }

        fn create_token(
            &self,
            amount: Amount,
            _mint_url: &str,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, WalletError>> + Send + '_>> {
            Box::pin(async move { Ok(format!("token_{}", amount.0).into_bytes()) })
        }

        fn mint_reachable(
            &self,
            _mint_url: &str,
        ) -> Pin<Box<dyn Future<Output = Result<bool, WalletError>> + Send + '_>> {
            Box::pin(async move { Ok(true) })
        }

        fn balance(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<Amount, WalletError>> + Send + '_>> {
            let fail = self.fail_balance;
            let val = *self.balance.lock().unwrap();
            Box::pin(async move {
                if fail {
                    Err(WalletError::Internal("mock failure".into()))
                } else {
                    Ok(Amount(val))
                }
            })
        }
    }

    fn make_adapter(balance: u64, mints: &[&str]) -> MerchantWalletAdapter {
        let wallet: Arc<dyn Wallet> = Arc::new(MockWallet::new(balance));
        let merchant = Arc::new(MerchantProvider::new(wallet));
        MerchantWalletAdapter::new(merchant, mints.iter().map(|s| s.to_string()).collect())
    }

    #[tokio::test]
    async fn balance_forwards_through_merchant() {
        let adapter = make_adapter(500, &["https://mint.example"]);
        assert_eq!(adapter.balance().await.unwrap(), 500);
    }

    #[tokio::test]
    async fn balance_error_converts_to_string() {
        let wallet: Arc<dyn Wallet> = Arc::new(MockWallet::failing());
        let merchant = Arc::new(MerchantProvider::new(wallet));
        let adapter = MerchantWalletAdapter::new(merchant, vec!["m".into()]);
        let err = adapter.balance().await.unwrap_err();
        assert!(err.contains("mock failure"));
    }

    #[tokio::test]
    async fn receive_token_forwards_bytes() {
        let adapter = make_adapter(0, &["m"]);
        let amount = adapter.receive_token("hello").await.unwrap();
        assert_eq!(amount, 5); // mock returns length
    }

    #[tokio::test]
    async fn create_token_returns_utf8_string() {
        let adapter = make_adapter(0, &["m"]);
        let token = adapter
            .create_token(100, "https://mint.example")
            .await
            .unwrap();
        assert_eq!(token, "token_100");
    }

    #[tokio::test]
    async fn get_mint_balances_assigns_total_to_first_mint() {
        let adapter = make_adapter(750, &["https://mint-a", "https://mint-b"]);
        let balances = adapter.get_mint_balances().await;
        assert_eq!(balances.len(), 2);
        assert_eq!(balances["https://mint-a"], 750);
        assert_eq!(balances["https://mint-b"], 0);
    }

    #[tokio::test]
    async fn merchant_swap_is_visible_to_adapter() {
        let initial_wallet: Arc<dyn Wallet> = Arc::new(MockWallet::new(100));
        let merchant = Arc::new(MerchantProvider::new(initial_wallet));
        let adapter = MerchantWalletAdapter::new(merchant.clone(), vec!["m".into()]);
        assert_eq!(adapter.balance().await.unwrap(), 100);

        let new_wallet: Arc<dyn Wallet> = Arc::new(MockWallet::new(900));
        merchant.swap(new_wallet);
        assert_eq!(adapter.balance().await.unwrap(), 900);
    }
}
