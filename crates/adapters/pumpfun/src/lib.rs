//! A real `ListingSource` for Solana, backed by PumpPortal's free
//! `subscribeNewToken` websocket feed (`wss://pumpportal.fun/api/data`).
//! No API key, no rate limit, sub-second delivery of every pump.fun
//! token creation - see the README for why this was chosen over paginated
//! aggregator APIs (DexScreener/GeckoTerminal) for detection.
//!
//! **Message schema caveat:** the field names parsed below (`txType`,
//! `mint`, `symbol`, `name`) are based on PumpPortal's publicly
//! documented `create` event shape at the time this was written, not a
//! guarantee pinned against their current live schema. If detection
//! silently stops working, this parser is the first place to check
//! against PumpPortal's current docs - `PumpPortalEvent` fails soft
//! (`#[serde(default)]` on every field) specifically so a schema drift
//! shows up as "fewer listings than expected" rather than a hard crash.
//!
//! **Scope of this adapter:** detection only. `MetricsProvider` and
//! `TokenSafetyChecker` here always return `None` - which
//! `AcquisitionEngine` correctly treats as "not enough information to
//! buy." That's a deliberate safe default, not a placeholder we forgot
//! to fill in: a token minted seconds ago has no meaningful 24h volume
//! yet, and pump.fun-specific honeypot signals (mint authority, freeze
//! authority) need real on-chain reads this crate doesn't do. See the
//! README's "not yet implemented" list.

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
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

pub mod exchange_client;
pub mod execution;
pub mod metrics_provider;
pub mod price_feed;
pub mod retry;
pub mod safety_checker;
pub use exchange_client::PumpPortalExchangeClient;
pub use execution::{execute_trade, load_wallet, wallet_pubkey_string, TradeAction, TradeRequest};
pub use metrics_provider::DexScreenerMetricsProvider;
pub use safety_checker::RugCheckSafetyChecker;

pub const DEFAULT_WS_URL: &str = "wss://pumpportal.fun/api/data";

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PumpPortalEvent {
    #[serde(default)]
    tx_type: Option<String>,
    #[serde(default)]
    mint: Option<String>,
}

pub struct PumpPortalSource {
    venue: Venue,
    receiver: Mutex<mpsc::UnboundedReceiver<Listing>>,
}

impl PumpPortalSource {
    /// Spawns a background task that maintains the websocket connection
    /// and forwards every `create` event as a `Listing`. Returns
    /// immediately - the connection happens in the background, so the
    /// first few `poll()` calls may return nothing while it establishes.
    pub fn spawn(ws_url: impl Into<String>) -> Result<Self, DomainError> {
        let venue = Venue::new(VenueKind::Dex, "pumpfun")?;
        let chain = Chain::new("solana")?;
        let (tx, rx) = mpsc::unbounded_channel();

        let url = ws_url.into();
        let task_venue = venue.clone();
        tokio::spawn(async move {
            run(url, tx, task_venue, chain).await;
        });

        Ok(Self {
            venue,
            receiver: Mutex::new(rx),
        })
    }
}

async fn run(url: String, tx: mpsc::UnboundedSender<Listing>, venue: Venue, chain: Chain) {
    loop {
        let mut stream = connect_with_backoff(&url).await;

        let subscribe = serde_json::json!({ "method": "subscribeNewToken" }).to_string();
        if let Err(e) = stream.send(Message::Text(subscribe)).await {
            warn!(error = %e, "failed to send pumpportal subscription, reconnecting");
            continue;
        }

        loop {
            let message = match stream.next().await {
                Some(Ok(m)) => m,
                Some(Err(e)) => {
                    warn!(error = %e, "pumpportal websocket error, reconnecting");
                    break;
                }
                None => {
                    warn!("pumpportal connection closed, reconnecting");
                    break;
                }
            };

            let Message::Text(text) = message else { continue };

            let event: PumpPortalEvent = match serde_json::from_str(&text) {
                Ok(e) => e,
                Err(e) => {
                    debug!(error = %e, "unrecognised pumpportal message, skipping");
                    continue;
                }
            };

            if event.tx_type.as_deref() != Some("create") {
                continue;
            }

            let Some(mint) = event.mint else { continue };
            let Ok(symbol) = Symbol::new(mint.to_lowercase()) else { continue };

            let listing = Listing::new(symbol, venue.clone(), chain.clone(), OffsetDateTime::now_utc());

            if tx.send(listing).is_err() {
                // Receiver dropped - the ListingSource itself was
                // dropped, so nothing is left to deliver to. Stop the
                // background task instead of spinning forever.
                return;
            }
        }
    }
}

#[async_trait]
impl ListingSource for PumpPortalSource {
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

/// Always-`None` `MetricsProvider` - the safe default until a real
/// Solana volume source is wired in. See the module docs.
pub struct NotYetImplementedMetrics;

#[async_trait]
impl MetricsProvider for NotYetImplementedMetrics {
    async fn metrics(&self, _symbol: &Symbol) -> Result<Option<ListingMetrics>, PortError> {
        Ok(None)
    }
}

/// Always-`None` `TokenSafetyChecker` - the safe default until real
/// pump.fun contract/authority checks are wired in. See the module docs.
pub struct NotYetImplementedSafetyChecker;

#[async_trait]
impl TokenSafetyChecker for NotYetImplementedSafetyChecker {
    async fn assess(&self, _symbol: &Symbol) -> Result<Option<SafetyReport>, PortError> {
        Ok(None)
    }
}

/// `ExchangeClient` fallback used when no wallet is configured
/// (`SOLANA_PRIVATE_KEY` unset) - see `execution::load_wallet`. Buy and
/// sell genuinely work via `PumpPortalExchangeClient` once a wallet is
/// present; this type exists so the bot can still run in
/// detection-only mode without one, rather than refusing to start.
/// `current_price` errors regardless of wallet configuration - live
/// Solana price monitoring isn't built yet (see the crate/README docs),
/// so this method is honest either way.
pub struct NoWalletExchange;

#[async_trait]
impl ExchangeClient for NoWalletExchange {
    fn venue_name(&self) -> &str {
        "pumpfun"
    }

    async fn current_price(&self, _symbol: &Symbol) -> Result<Decimal, PortError> {
        Err(PortError::Rejected(
            "real-time Solana price monitoring is not yet implemented - see README".to_string(),
        ))
    }

    async fn submit_buy_by_amount(&self, _symbol: &Symbol, _quote_amount: Decimal) -> Result<FilledBuy, PortError> {
        Err(PortError::Rejected(
            "no wallet configured (SOLANA_PRIVATE_KEY not set) - see execution module docs".to_string(),
        ))
    }

    async fn submit_order(&self, _order: Order) -> Result<Order, PortError> {
        Err(PortError::Rejected(
            "no wallet configured (SOLANA_PRIVATE_KEY not set) - see execution module docs".to_string(),
        ))
    }
}
