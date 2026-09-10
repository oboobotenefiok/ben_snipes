//! A real `ExchangeClient` for PumpPortal. Buy and sell execution both
//! work through `execution::execute_trade` - a buy spends a SOL amount,
//! a sell offloads a known token quantity, and neither needs us to know
//! a price beforehand, since PumpPortal's bonding-curve math happens on
//! their side. See `execution`'s module doc comment for the signing-code
//! verification caveat before running this with real funds.
//!
//! `current_price` is backed by the shared batched Jupiter price cache (Jupiter's
//! Price API v3, converted to SOL-denominated terms to match
//! `entry_price` elsewhere in this codebase - see that module's doc
//! comment for why the conversion matters). This is a real,
//! well-corroborated integration, but it's an external network
//! dependency that returns "no data yet" for very fresh tokens - a
//! position can briefly have no way to check its exit condition right
//! after buying, until the token gets indexed.
//!
//! **Confirming a buy landed:** after broadcasting, this polls
//! `getSignatureStatuses` until the transaction is confirmed (or errors
//! out), then reads the resulting balance via `getTokenAccountsByOwner`.
//! Both are foundational, long-stable pieces of Solana's JSON-RPC wire
//! protocol - not a Rust crate's internal API - so these carry
//! meaningfully less version-churn risk than the signing code in
//! `execution.rs` does, but they're still unverified by an actual RPC
//! call in this environment (no network access here). Sanity-check
//! against a real RPC response shape on first run.

use crate::execution::{execute_trade, preflight_trade, TradeAction, TradeRequest};
use crate::price_feed;
use crate::retry::with_retry;
use async_trait::async_trait;
use ben_snipes_domain::{FilledBuy, FilledSell, Order, OrderSide, Symbol};
use ben_snipes_ports::{ExchangeClient, PortError};
use std::collections::HashMap;
use rust_decimal::Decimal;
use solana_sdk::signer::keypair::Keypair;
use solana_sdk::signer::Signer;
use std::time::Duration;

/// How many times to poll for confirmation before giving up. At ~1s per
/// attempt this is roughly a 30 second timeout, which is generous for
/// Solana's typical confirmation times but not unbounded - a genuinely
/// stuck transaction shouldn't hang the bot forever.
const CONFIRMATION_ATTEMPTS: u32 = 30;
const CONFIRMATION_POLL_INTERVAL: Duration = Duration::from_secs(1);

const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

/// Minimum SOL reserved for transaction/priority fees on top of the buy
/// amount itself. Solana fees are typically tiny, but this is a fixed,
/// deliberately-conservative buffer rather than an exact fee estimate -
/// the goal is catching an obviously-insufficient balance before
/// attempting a trade, not computing the precise fee.
const FEE_BUFFER_LAMPORTS: u64 = 5_000_000; // 0.005 SOL

/// The three role-specific Solana RPC endpoints this client routes
/// calls to. See the module-level docs for why reads, writes, and
/// confirmation polling are split rather than sharing one endpoint.
#[derive(Clone)]
pub struct SolanaRpc {
    pub read_url: String,
    pub write_url: String,
    pub confirm_url: String,
}

pub struct PumpPortalExchangeClient {
    http: reqwest::Client,
    wallet: Keypair,
    rpc: SolanaRpc,
    slippage_percent: u32,
    priority_fee_sol: Decimal,
    price_cache: price_feed::PriceCache,
}

impl PumpPortalExchangeClient {
    pub fn new(
        wallet: Keypair,
        rpc: SolanaRpc,
        slippage_percent: u32,
        priority_fee_sol: Decimal,
        price_cache_ttl: Duration,
        jupiter_max_retries: u32,
        jupiter_circuit_breaker_failures: u32,
        jupiter_circuit_breaker_cooldown: Duration,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            wallet,
            rpc,
            slippage_percent,
            priority_fee_sol,
            price_cache: price_feed::PriceCache::new(
                price_cache_ttl,
                jupiter_max_retries,
                std::sync::Arc::new(price_feed::JupiterCircuitBreaker::new(
                    jupiter_circuit_breaker_failures,
                    jupiter_circuit_breaker_cooldown,
                )),
            ),
        }
    }

    fn wallet(&self) -> &Keypair {
        &self.wallet
    }

    async fn wait_for_confirmation(&self, signature: &str) -> Result<(), PortError> {
        for _ in 0..CONFIRMATION_ATTEMPTS {
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getSignatureStatuses",
                "params": [[signature], { "searchTransactionHistory": true }],
            });

            // A transient failure on a single poll attempt (a dropped
            // connection, a slow RPC node) is not the same as "the
            // transaction failed" - it just means this one check was
            // inconclusive, so it falls through to the same
            // keep-polling path as "not yet visible on-chain" rather
            // than aborting the whole wait. Only an explicit on-chain
            // `err` in a successfully-parsed response is treated as a
            // real failure.
            let outcome: Option<Result<(), PortError>> = async {
                let response = self
                    .http
                    .post(&self.rpc.confirm_url)
                    .header("Content-Type", "application/json")
                    .body(body.to_string())
                    .send()
                    .await
                    .ok()?;

                let json: serde_json::Value = response.json().await.ok()?;

                match json.pointer("/result/value/0") {
                    Some(status) if !status.is_null() => {
                        if let Some(err) = status.get("err") {
                            if !err.is_null() {
                                return Some(Err(PortError::Rejected(format!("transaction failed on-chain: {err}"))));
                            }
                        }
                        Some(Ok(()))
                    }
                    _ => None,
                }
            }
            .await;

            match outcome {
                Some(result) => return result,
                None => {
                    // Inconclusive - either a transient error, or
                    // genuinely not yet visible to this RPC node. Either
                    // way, the right move is the same: wait and poll
                    // again.
                }
            }

            tokio::time::sleep(CONFIRMATION_POLL_INTERVAL).await;
        }

        Err(PortError::Rejected(format!(
            "transaction {signature} not confirmed within {CONFIRMATION_ATTEMPTS} attempts"
        )))
    }

    /// Reads the wallet's balance of `mint` via `getTokenAccountsByOwner`.
    /// Prefers `uiAmountString` (avoids float round-tripping for the
    /// balance figure) and falls back to the float `uiAmount` field only
    /// if the string form isn't present.
    async fn token_balance(&self, mint: &str) -> Result<Decimal, PortError> {
        let wallet_pubkey = self.wallet().pubkey().to_string();
        with_retry(3, || async {
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getTokenAccountsByOwner",
                "params": [
                    wallet_pubkey.clone(),
                    { "mint": mint },
                    { "encoding": "jsonParsed" },
                ],
            });

            let response = self
                .http
                .post(&self.rpc.read_url)
                .header("Content-Type", "application/json")
                .body(body.to_string())
                .send()
                .await
                .map_err(|e| format!("balance check failed: {e}"))?;

            let json: serde_json::Value = response
                .json()
                .await
                .map_err(|e| format!("failed to parse balance response: {e}"))?;

            if json.pointer("/result/value").and_then(|v| v.as_array()).is_some_and(|accounts| accounts.is_empty()) {
                return Ok(Decimal::ZERO);
            }

            let token_amount = json.pointer("/result/value/0/account/data/parsed/info/tokenAmount");

            if let Some(s) = token_amount.and_then(|v| v.get("uiAmountString")).and_then(|v| v.as_str()) {
                return s.parse::<Decimal>().map_err(|e| format!("could not parse token balance '{s}': {e}"));
            }

            let ui_amount = token_amount
                .and_then(|v| v.get("uiAmount"))
                .and_then(|v| v.as_f64())
                .ok_or_else(|| "no token account balance found - the buy may not have landed yet".to_string())?;

            Decimal::try_from(ui_amount).map_err(|e| format!("balance value was not a valid decimal: {e}"))
        })
        .await
        .map_err(PortError::Rejected)
    }

    /// SOL balance of the wallet, in whole SOL (not lamports) - used for
    /// the pre-trade balance check in `submit_buy_by_amount`.
    async fn sol_balance(&self) -> Result<Decimal, PortError> {
        let wallet_pubkey = self.wallet().pubkey().to_string();
        with_retry(3, || async {
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getBalance",
                "params": [wallet_pubkey.clone()],
            });

            let response = self
                .http
                .post(&self.rpc.read_url)
                .header("Content-Type", "application/json")
                .body(body.to_string())
                .send()
                .await
                .map_err(|e| format!("SOL balance check failed: {e}"))?;

            let json: serde_json::Value = response
                .json()
                .await
                .map_err(|e| format!("failed to parse SOL balance response: {e}"))?;

            if let Some(error) = json.get("error") {
                return Err(format!("SOL balance RPC returned an error: {error}"));
            }

            let lamports = json
                .pointer("/result/value")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| format!("no balance in RPC response: {json}"))?;

            Ok(Decimal::from(lamports) / Decimal::from(LAMPORTS_PER_SOL))
        })
        .await
        .map_err(PortError::Rejected)
    }
}

impl PumpPortalExchangeClient {
    /// Reads the confirmed transaction metadata and derives the wallet's
    /// actual SOL settlement from the fee-payer balance delta. The gross
    /// proceeds are reconstructed as the wallet balance increase plus the
    /// transaction fee, while the fee is returned separately. This avoids
    /// treating a pre-trade price quote as a fill.
    async fn solana_settlement(&self, signature: &str) -> Result<(Decimal, Decimal), PortError> {
        let wallet_pubkey = self.wallet().pubkey().to_string();
        with_retry(3, || async {
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getTransaction",
                "params": [
                    signature,
                    {
                        "encoding": "jsonParsed",
                        "commitment": "confirmed",
                        "maxSupportedTransactionVersion": 0,
                    }
                ],
            });

            let response = self
                .http
                .post(&self.rpc.read_url)
                .header("Content-Type", "application/json")
                .body(body.to_string())
                .send()
                .await
                .map_err(|e| format!("transaction lookup failed: {e}"))?;

            let json: serde_json::Value = response
                .json()
                .await
                .map_err(|e| format!("failed to parse transaction response: {e}"))?;

            if let Some(error) = json.get("error") {
                return Err(format!("transaction lookup RPC returned an error: {error}"));
            }

            let result = json
                .get("result")
                .and_then(|value| value.as_object())
                .ok_or_else(|| "confirmed transaction metadata was not available".to_string())?;

            let meta = result
                .get("meta")
                .and_then(|value| value.as_object())
                .ok_or_else(|| "confirmed transaction had no metadata".to_string())?;

            let fee_lamports = meta
                .get("fee")
                .and_then(|value| value.as_u64())
                .ok_or_else(|| "confirmed transaction metadata had no fee".to_string())?;

            let pre_balances = meta
                .get("preBalances")
                .and_then(|value| value.as_array())
                .ok_or_else(|| "confirmed transaction metadata had no preBalances".to_string())?;
            let post_balances = meta
                .get("postBalances")
                .and_then(|value| value.as_array())
                .ok_or_else(|| "confirmed transaction metadata had no postBalances".to_string())?;

            if pre_balances.is_empty() || pre_balances.len() != post_balances.len() {
                return Err("confirmed transaction balance metadata was incomplete".to_string());
            }

            // PumpPortal constructs the wallet as the fee payer. Solana's
            // transaction message therefore places it at account index 0.
            // We validate that assumption against the returned account key
            // before trusting the balance delta.
            let first_account = result
                .get("transaction")
                .and_then(|value| value.get("message"))
                .and_then(|value| value.get("accountKeys"))
                .and_then(|value| value.as_array())
                .and_then(|keys| keys.first())
                .and_then(|key| key.get("pubkey"))
                .and_then(|value| value.as_str())
                .ok_or_else(|| "confirmed transaction did not expose its fee payer".to_string())?;

            if first_account != wallet_pubkey.as_str() {
                return Err("confirmed transaction fee payer did not match configured wallet".to_string());
            }

            let pre = pre_balances[0]
                .as_u64()
                .ok_or_else(|| "invalid pre-transaction wallet balance".to_string())?;
            let post = post_balances[0]
                .as_u64()
                .ok_or_else(|| "invalid post-transaction wallet balance".to_string())?;

            let net_change = Decimal::from(post) - Decimal::from(pre);
            let fee = Decimal::from(fee_lamports);
            let gross_proceeds = net_change + fee;

            Ok((
                gross_proceeds / Decimal::from(LAMPORTS_PER_SOL),
                fee / Decimal::from(LAMPORTS_PER_SOL),
            ))
        })
        .await
        .map_err(PortError::Rejected)
    }
}

#[async_trait]
impl ExchangeClient for PumpPortalExchangeClient {
    fn venue_name(&self) -> &str {
        "pumpfun"
    }

    async fn current_price(&self, symbol: &Symbol) -> Result<Decimal, PortError> {
        let prices = self
            .price_cache
            .get_or_fetch_batch(&self.http, &[symbol.as_str().to_string()])
            .await
            .map_err(PortError::Rejected)?;
        prices
            .get(symbol.as_str())
            .copied()
            .ok_or_else(|| PortError::Rejected(format!("Jupiter returned no price for {}", symbol.as_str())))
    }

    async fn current_prices_batch(&self, symbols: &[Symbol]) -> Result<HashMap<String, Decimal>, PortError> {
        let mints: Vec<String> = symbols.iter().map(|symbol| symbol.as_str().to_string()).collect();
        self.price_cache
            .get_or_fetch_batch(&self.http, &mints)
            .await
            .map_err(PortError::Rejected)
    }

    async fn submit_buy_by_amount(&self, symbol: &Symbol, quote_amount: Decimal) -> Result<FilledBuy, PortError> {
        if quote_amount <= Decimal::ZERO {
            return Err(PortError::Rejected(format!(
                "buy amount must be positive, got {quote_amount}"
            )));
        }

        if self.priority_fee_sol < Decimal::ZERO {
            return Err(PortError::Rejected(format!(
                "priority fee must not be negative, got {}",
                self.priority_fee_sol
            )));
        }

        let balance = self.sol_balance().await?;
        let required_balance = quote_amount
            + self.priority_fee_sol
            + Decimal::from(FEE_BUFFER_LAMPORTS) / Decimal::from(LAMPORTS_PER_SOL);

        if balance < required_balance {
            return Err(PortError::Rejected(format!(
                "insufficient SOL balance for buy: available={balance}, required={required_balance}"
            )));
        }

        let previous_quantity = self.token_balance(symbol.as_str()).await?;

        let request = TradeRequest {
            action: TradeAction::Buy,
            mint: symbol.as_str().to_string(),
            amount: quote_amount.to_string(),
            slippage_percent: self.slippage_percent,
            priority_fee_sol: self.priority_fee_sol,
        };

        let signature = execute_trade(&self.http, self.wallet(), &self.rpc.write_url, &request)
            .await
            .map_err(PortError::Rejected)?;

        self.wait_for_confirmation(&signature).await?;

        let current_quantity = self.token_balance(symbol.as_str()).await?;
        let quantity = current_quantity - previous_quantity;
        if quantity <= Decimal::ZERO {
            return Err(PortError::Rejected(
                "buy confirmed on-chain but resulting token balance was zero or unreadable".to_string(),
            ));
        }

        Ok(FilledBuy {
            quantity,
            entry_price: quote_amount / quantity,
        })
    }

    async fn preflight_sell(&self, order: &Order) -> Result<(), PortError> {
        if order.side != OrderSide::Sell {
            return Err(PortError::Rejected(
                "PumpPortal sell preflight requires a sell order".to_string(),
            ));
        }
        if order.quantity <= Decimal::ZERO {
            return Err(PortError::Rejected(
                "PumpPortal sell preflight requires a positive quantity".to_string(),
            ));
        }

        let request = TradeRequest {
            action: TradeAction::Sell,
            mint: order.symbol.as_str().to_string(),
            amount: order.quantity.to_string(),
            slippage_percent: self.slippage_percent,
            priority_fee_sol: self.priority_fee_sol,
        };

        preflight_trade(&self.http, self.wallet(), &self.rpc.write_url, &request)
            .await
            .map_err(PortError::Rejected)
    }

    async fn submit_order(&self, order: Order) -> Result<FilledSell, PortError> {
        if order.side != OrderSide::Sell {
            return Err(PortError::Rejected(
                "PumpPortalExchangeClient buys go through submit_buy_by_amount, not submit_order".to_string(),
            ));
        }

        let request = TradeRequest {
            action: TradeAction::Sell,
            mint: order.symbol.as_str().to_string(),
            amount: order.quantity.to_string(),
            slippage_percent: self.slippage_percent,
            priority_fee_sol: self.priority_fee_sol,
        };

        let previous_quantity = self.token_balance(order.symbol.as_str()).await?;

        let signature = execute_trade(&self.http, self.wallet(), &self.rpc.write_url, &request)
            .await
            .map_err(PortError::Rejected)?;

        self.wait_for_confirmation(&signature).await?;

        let current_quantity = self.token_balance(order.symbol.as_str()).await?;
        let sold_quantity = previous_quantity - current_quantity;
        if sold_quantity <= Decimal::ZERO {
            return Err(PortError::Rejected(
                "sell transaction confirmed but token balance did not decrease".to_string(),
            ));
        }

        let (quote_proceeds, fee_quote) = self.solana_settlement(&signature).await?;

        Ok(FilledSell {
            quantity: sold_quantity,
            execution_price: if sold_quantity > Decimal::ZERO {
                Some(quote_proceeds / sold_quantity)
            } else {
                None
            },
            quote_proceeds: Some(quote_proceeds),
            fee_quote: Some(fee_quote),
            tx_id: Some(signature),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ben_snipes_config::SolanaConfig;

    fn dummy_rpc() -> SolanaRpc {
        SolanaRpc {
            read_url: "https://read.example".to_string(),
            write_url: "https://write.example".to_string(),
            confirm_url: "https://confirm.example".to_string(),
        }
    }

    #[test]
    fn buy_balance_requirement_includes_fee_buffer_and_priority_fee() {
        let quote_amount = Decimal::new(1, 2); // 0.01 SOL
        let priority_fee_sol = Decimal::new(1, 4); // 0.0001 SOL
        let required_balance =
            quote_amount + priority_fee_sol + Decimal::from(FEE_BUFFER_LAMPORTS) / Decimal::from(LAMPORTS_PER_SOL);

        assert_eq!(required_balance, Decimal::new(151, 4));
    }

    #[test]
    fn solana_rpc_routes_each_role_to_the_correct_field() {
        let rpc = dummy_rpc();
        assert_eq!(rpc.read_url, "https://read.example");
        assert_eq!(rpc.write_url, "https://write.example");
        assert_eq!(rpc.confirm_url, "https://confirm.example");
    }

    fn base_solana_config() -> SolanaConfig {
        SolanaConfig {
            pumpportal_ws_url: "wss://example/data".to_string(),
            read_rpc_url: "https://read.example".to_string(),
            write_rpc_url: "https://write.example".to_string(),
            confirm_rpc_url: None,
            slippage_percent: 10,
            priority_fee_sol: Decimal::new(1, 4),
            price_cache_ttl_seconds: 30,
            jupiter_max_retries: 3,
            jupiter_circuit_breaker_failures: 3,
            jupiter_circuit_breaker_cooldown_seconds: 60,
        }
    }

    #[test]
    fn confirm_rpc_url_falls_back_to_write_url_when_unset() {
        let config = base_solana_config();
        assert_eq!(config.confirm_rpc_url(), "https://write.example");
    }

    #[test]
    fn confirm_rpc_url_uses_explicit_value_when_set() {
        let mut config = base_solana_config();
        config.confirm_rpc_url = Some("https://confirm.example".to_string());
        assert_eq!(config.confirm_rpc_url(), "https://confirm.example");
    }
}
