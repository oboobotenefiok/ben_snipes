//! A real `ExchangeClient` for PumpPortal. Buy and sell execution both
//! work through `execution::execute_trade` - a buy spends a SOL amount,
//! a sell offloads a known token quantity, and neither needs us to know
//! a price beforehand, since PumpPortal's bonding-curve math happens on
//! their side. See `execution`'s module doc comment for the signing-code
//! verification caveat before running this with real funds.
//!
//! `current_price` is backed by `price_feed::fetch_price` (Jupiter's
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

use crate::execution::{execute_trade, TradeAction, TradeRequest};
use crate::price_feed;
use async_trait::async_trait;
use ben_snipes_domain::{FilledBuy, Order, OrderSide, OrderStatus, Symbol};
use ben_snipes_ports::{ExchangeClient, PortError};
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

pub struct PumpPortalExchangeClient {
    http: reqwest::Client,
    wallet: Keypair,
    rpc_url: String,
    slippage_percent: u32,
    priority_fee_sol: Decimal,
}

impl PumpPortalExchangeClient {
    pub fn new(wallet: Keypair, rpc_url: impl Into<String>, slippage_percent: u32, priority_fee_sol: Decimal) -> Self {
        Self {
            http: reqwest::Client::new(),
            wallet,
            rpc_url: rpc_url.into(),
            slippage_percent,
            priority_fee_sol,
        }
    }

    async fn wait_for_confirmation(&self, signature: &str) -> Result<(), PortError> {
        for _ in 0..CONFIRMATION_ATTEMPTS {
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getSignatureStatuses",
                "params": [[signature], { "searchTransactionHistory": true }],
            });

            let response = self
                .http
                .post(&self.rpc_url)
                .header("Content-Type", "application/json")
                .body(body.to_string())
                .send()
                .await
                .map_err(|e| PortError::Rejected(format!("confirmation check failed: {e}")))?;

            let json: serde_json::Value = response
                .json()
                .await
                .map_err(|e| PortError::Rejected(format!("failed to parse confirmation response: {e}")))?;

            match json.pointer("/result/value/0") {
                Some(status) if !status.is_null() => {
                    if let Some(err) = status.get("err") {
                        if !err.is_null() {
                            return Err(PortError::Rejected(format!("transaction failed on-chain: {err}")));
                        }
                    }
                    return Ok(());
                }
                _ => {
                    // Not yet visible to this RPC node - keep polling.
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
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getTokenAccountsByOwner",
            "params": [
                self.wallet.pubkey().to_string(),
                { "mint": mint },
                { "encoding": "jsonParsed" },
            ],
        });

        let response = self
            .http
            .post(&self.rpc_url)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| PortError::Rejected(format!("balance check failed: {e}")))?;

        let json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| PortError::Rejected(format!("failed to parse balance response: {e}")))?;

        let token_amount = json.pointer("/result/value/0/account/data/parsed/info/tokenAmount");

        if let Some(s) = token_amount.and_then(|v| v.get("uiAmountString")).and_then(|v| v.as_str()) {
            return s
                .parse::<Decimal>()
                .map_err(|e| PortError::Rejected(format!("could not parse token balance '{s}': {e}")));
        }

        let ui_amount = token_amount
            .and_then(|v| v.get("uiAmount"))
            .and_then(|v| v.as_f64())
            .ok_or_else(|| {
                PortError::Rejected(
                    "no token account balance found - the buy may not have landed yet".to_string(),
                )
            })?;

        Decimal::try_from(ui_amount).map_err(|e| PortError::Rejected(format!("balance value was not a valid decimal: {e}")))
    }
}

#[async_trait]
impl ExchangeClient for PumpPortalExchangeClient {
    fn venue_name(&self) -> &str {
        "pumpfun"
    }

    async fn current_price(&self, symbol: &Symbol) -> Result<Decimal, PortError> {
        price_feed::fetch_price(&self.http, symbol.as_str())
            .await
            .map_err(PortError::Rejected)
    }

    async fn submit_buy_by_amount(&self, symbol: &Symbol, quote_amount: Decimal) -> Result<FilledBuy, PortError> {
        let request = TradeRequest {
            action: TradeAction::Buy,
            mint: symbol.as_str().to_string(),
            amount: quote_amount.to_string(),
            slippage_percent: self.slippage_percent,
            priority_fee_sol: self.priority_fee_sol,
        };

        let signature = execute_trade(&self.http, &self.wallet, &self.rpc_url, &request)
            .await
            .map_err(PortError::Rejected)?;

        self.wait_for_confirmation(&signature).await?;

        let quantity = self.token_balance(symbol.as_str()).await?;
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

    async fn submit_order(&self, order: Order) -> Result<Order, PortError> {
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

        let signature = execute_trade(&self.http, &self.wallet, &self.rpc_url, &request)
            .await
            .map_err(PortError::Rejected)?;

        self.wait_for_confirmation(&signature).await?;

        let mut filled = order;
        filled.status = OrderStatus::Filled;
        Ok(filled)
    }
}
