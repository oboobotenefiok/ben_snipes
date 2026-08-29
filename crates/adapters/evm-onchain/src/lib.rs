//! A real `ListingSource` for EVM chains, subscribing directly to a DEX
//! factory contract's pair/pool-creation event logs over a websocket RPC
//! (`eth_subscribe`), rather than polling an indexer API. Push-based, no
//! pagination cap - see the README for why this was chosen over
//! DexScreener/GeckoTerminal for detection.
//!
//! Deliberately **not hardcoded to one chain or one factory**: which
//! chain, which factory contract, and which event signature to watch are
//! all supplied via `EvmFactoryConfig`, so the same adapter code can
//! watch Ethereum/Uniswap, Base/Aerodrome, or any other EVM
//! chain+factory pair by construction, not by forking the crate.
//!
//! **`topic0` is intentionally not hardcoded anywhere in this crate.**
//! It's the keccak256 hash of the creation event's signature (e.g.
//! `PairCreated(address,address,address,uint256)` for a Uniswap
//! V2-style factory), and different factory designs (V2 vs V3-style,
//! different DEXes) use different event shapes and therefore different
//! hashes. Compute it yourself against the target factory's actual ABI
//! (or look it up in a topic-signature database) rather than trusting a
//! constant baked into a financial bot - getting this wrong doesn't
//! error, it just silently watches for the wrong event.
//!
//! **Scope of this adapter:** detection only, same as `pumpfun`.
//! `MetricsProvider`, `TokenSafetyChecker`, and `ExchangeClient` are
//! placeholders that keep `AcquisitionEngine` from ever actually buying
//! anything through this source yet - see that crate's docs for why
//! that's a safe default, not a shortcut.

use async_trait::async_trait;
use ben_snipes_adapter_ws_support::connect_with_backoff;
use ben_snipes_domain::{
    Chain, DomainError, FilledBuy, Listing, ListingMetrics, Order, SafetyReport, Symbol, Venue,
    VenueKind,
};
use ben_snipes_ports::{
    ExchangeClient, ListingSnapshot, ListingSource, MetricsProvider, PortError, TokenSafetyChecker,
};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use std::collections::HashSet;
use time::OffsetDateTime;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

#[derive(Debug, Clone)]
pub struct EvmFactoryConfig {
    /// e.g. "ethereum", "base" - becomes both the `Chain` identity and
    /// part of this source's venue name.
    pub chain_name: String,
    /// A websocket RPC endpoint that supports `eth_subscribe` (Alchemy,
    /// QuickNode, or a self-hosted node). Include your own API key in
    /// the URL - this crate treats it as an opaque connection string.
    pub ws_rpc_url: String,
    /// The factory contract address to watch, e.g. Uniswap V2's
    /// `0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f` on Ethereum mainnet.
    pub factory_address: String,
    /// keccak256 topic hash of the creation event - see module docs.
    /// Not validated against the factory's actual ABI; get this wrong
    /// and the subscription just never fires, silently.
    pub topic0: String,
    /// Lowercased addresses of well-known base/quote assets on this
    /// chain (WETH, USDC, USDT, DAI, ...). Used to figure out which side
    /// of a newly created pair is "the new token" - see `run` below.
    pub base_assets: Vec<String>,
}

pub struct EvmFactoryLogSource {
    venue: Venue,
    receiver: Mutex<mpsc::UnboundedReceiver<Listing>>,
}

impl EvmFactoryLogSource {
    /// Spawns a background task that maintains the websocket
    /// subscription and forwards each decoded pair-creation log as one
    /// or two `Listing` candidates (see `run`). Returns immediately.
    pub fn spawn(config: EvmFactoryConfig) -> Result<Self, DomainError> {
        let venue = Venue::new(VenueKind::Dex, format!("{}-onchain", config.chain_name))?;
        let chain = Chain::new(config.chain_name.clone())?;
        let (tx, rx) = mpsc::unbounded_channel();

        let task_venue = venue.clone();
        tokio::spawn(async move {
            run(config, tx, task_venue, chain).await;
        });

        Ok(Self {
            venue,
            receiver: Mutex::new(rx),
        })
    }
}

async fn run(config: EvmFactoryConfig, tx: mpsc::UnboundedSender<Listing>, venue: Venue, chain: Chain) {
    let base_assets: HashSet<String> = config.base_assets.iter().map(|a| a.to_lowercase()).collect();

    loop {
        let mut stream = connect_with_backoff(&config.ws_rpc_url).await;

        let subscribe_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_subscribe",
            "params": ["logs", { "address": config.factory_address, "topics": [config.topic0] }],
        })
        .to_string();

        if let Err(e) = stream.send(Message::Text(subscribe_request)).await {
            warn!(chain = config.chain_name, error = %e, "failed to send eth_subscribe request, reconnecting");
            continue;
        }

        loop {
            let message = match stream.next().await {
                Some(Ok(m)) => m,
                Some(Err(e)) => {
                    warn!(chain = config.chain_name, error = %e, "evm websocket error, reconnecting");
                    break;
                }
                None => {
                    warn!(chain = config.chain_name, "evm websocket connection closed, reconnecting");
                    break;
                }
            };

            let Message::Text(text) = message else { continue };

            let parsed: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    debug!(error = %e, "unrecognised evm rpc message, skipping");
                    continue;
                }
            };

            // The subscription confirmation response ({"id":1,"result":
            // "0x..."}) has no "params" field - only ongoing
            // eth_subscription notifications do. Anything else, we skip.
            let Some(log) = parsed.get("params").and_then(|p| p.get("result")) else {
                continue;
            };

            let Some(topics) = log.get("topics").and_then(|t| t.as_array()) else {
                continue;
            };
            // topics[0] is the event signature itself; a
            // PairCreated-style event needs two more indexed addresses.
            if topics.len() < 3 {
                continue;
            }

            let Some(token0) = topics[1].as_str().and_then(extract_address) else { continue };
            let Some(token1) = topics[2].as_str().and_then(extract_address) else { continue };

            let candidates: Vec<String> =
                match (base_assets.contains(&token0), base_assets.contains(&token1)) {
                    // Exactly one side is a known base asset - the other
                    // side is confidently "the new token".
                    (true, false) => vec![token1],
                    (false, true) => vec![token0],
                    // Neither or both matched - we can't tell which side
                    // is new, so emit both as candidates and let
                    // AcquisitionCriteria/the safety gate filter out
                    // whichever one doesn't actually qualify downstream,
                    // rather than silently guessing wrong.
                    _ => vec![token0, token1],
                };

            for address in candidates {
                let Ok(symbol) = Symbol::new(address) else { continue };
                let listing = Listing::new(symbol, venue.clone(), chain.clone(), OffsetDateTime::now_utc());
                if tx.send(listing).is_err() {
                    return;
                }
            }
        }
    }
}

/// Extracts a 20-byte address from a 32-byte log topic - topics encode
/// addresses left-padded with zeros, so the address is the last 40 hex
/// characters.
fn extract_address(topic: &str) -> Option<String> {
    let hex = topic.strip_prefix("0x")?;
    if hex.len() != 64 {
        return None;
    }
    Some(format!("0x{}", &hex[24..]).to_lowercase())
}

#[async_trait]
impl ListingSource for EvmFactoryLogSource {
    fn source_id(&self) -> &str {
        self.venue.name()
    }

    async fn poll(&self, _cursor: Option<&str>) -> Result<ListingSnapshot, PortError> {
        let mut rx = self.receiver.lock().await;
        let mut new = Vec::new();
        while let Ok(listing) = rx.try_recv() {
            new.push(listing);
        }
        Ok(ListingSnapshot::Incremental { new, cursor: None })
    }
}

/// Always-`None` `MetricsProvider` - the safe default until a real EVM
/// volume source (e.g. a price/volume API or DEX subgraph) is wired in.
pub struct NotYetImplementedMetrics;

#[async_trait]
impl MetricsProvider for NotYetImplementedMetrics {
    async fn metrics(&self, _symbol: &Symbol) -> Result<Option<ListingMetrics>, PortError> {
        Ok(None)
    }
}

/// Always-`None` `TokenSafetyChecker` - the safe default until a real
/// sell-simulation/contract-read safety check is wired in.
pub struct NotYetImplementedSafetyChecker;

#[async_trait]
impl TokenSafetyChecker for NotYetImplementedSafetyChecker {
    async fn assess(&self, _symbol: &Symbol) -> Result<Option<SafetyReport>, PortError> {
        Ok(None)
    }
}

/// Always-erroring `ExchangeClient` placeholder - see `pumpfun`'s
/// equivalent type for why this is intentionally loud rather than a
/// silent no-op, and why it should be structurally unreachable given
/// `NotYetImplementedMetrics` always returning `None`.
pub struct NotYetImplementedExchange;

#[async_trait]
impl ExchangeClient for NotYetImplementedExchange {
    fn venue_name(&self) -> &str {
        "evm-onchain"
    }

    async fn current_price(&self, _symbol: &Symbol) -> Result<Decimal, PortError> {
        Err(PortError::Rejected(
            "real EVM trade execution is not yet implemented - see README".to_string(),
        ))
    }

    async fn submit_buy_by_amount(&self, _symbol: &Symbol, _quote_amount: Decimal) -> Result<FilledBuy, PortError> {
        Err(PortError::Rejected(
            "real EVM trade execution is not yet implemented - see README".to_string(),
        ))
    }

    async fn submit_order(&self, _order: Order) -> Result<Order, PortError> {
        Err(PortError::Rejected(
            "real EVM trade execution is not yet implemented - see README".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_address_from_a_left_padded_topic() {
        let topic = "0x000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
        assert_eq!(
            extract_address(topic),
            Some("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".to_string())
        );
    }

    #[test]
    fn rejects_malformed_topic() {
        assert_eq!(extract_address("0xnothex"), None);
        assert_eq!(extract_address("not even prefixed"), None);
    }
}
