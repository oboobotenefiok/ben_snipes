--- ./config/default.toml ---
[risk]
take_profit_percent = "10.0"
stop_loss_percent = "5.0"
poll_interval_seconds = 5
max_position_size = "25.0"
min_volume_24h = "50000"

[safety]
# 1000 bps = 10%. Anything taxing a sell higher than this is treated as
# a honeypot signal rather than an aggressive-but-legitimate tokenomics
# choice - tune down if that's too permissive for your risk tolerance.
max_sell_tax_bps = 1000

[storage]
state_dir = "state"

[solana]
# PumpPortal's public data feed - free, no API key required for
# subscribeNewToken. Only override this if you're pointing at a
# self-hosted relay or a different environment.
pumpportal_ws_url = "wss://pumpportal.fun/api/data"

# Zero or more EVM chains to watch. Each entry spawns one real,
# websocket-backed ListingSource. There is no usable default for
# ws_rpc_url or topic0 - both are deployment-specific (your own RPC
# provider key, and the actual event hash for the factory you're
# watching) - see ben_snipes-adapter-evm-onchain's crate docs before
# filling these in. Left empty by default so a fresh checkout doesn't
# silently try to connect to a placeholder URL.
# Example (uncomment and fill in to enable):
# [[evm_chains]]
# chain_name = "ethereum"
# ws_rpc_url = "wss://YOUR-PROVIDER-WS-ENDPOINT-WITH-API-KEY"
# factory_address = "0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f"  # Uniswap V2 factory, mainnet
# topic0 = "PUT-THE-VERIFIED-PairCreated-TOPIC-HASH-HERE"
# base_assets = [
#     "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",  # WETH
#     "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",  # USDC
#     "0xdac17f958d2ee523a2206206994597c13d831ec7",  # USDT
# ]
evm_chains = []

--- ./Cargo.toml ---
[workspace]
resolver = "2"
members = [
    "crates/domain",
    "crates/ports",
    "crates/application",
    "crates/config",
    "crates/adapters/statefile",
    "crates/adapters/dex-mock",
    "crates/adapters/ws-support",
    "crates/adapters/pumpfun",
    "crates/adapters/evm-onchain",
    "bin/runner",
]

# Centralising versions here means every crate in the workspace pulls the
# same version of a dependency, which keeps compile times sane and avoids
# the classic "two versions of tokio in one binary" problem.
[workspace.dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros", "time", "fs", "signal"] }
tokio-tungstenite = { version = "0.24", features = ["rustls-tls-webpki-roots"] }
futures-util = "0.3"
async-trait = "0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
time = { version = "0.3", features = ["serde", "macros"] }
config = "0.14"
rust_decimal = { version = "1", features = ["serde-str"] }

ben_snipes-domain = { path = "crates/domain" }
ben_snipes-ports = { path = "crates/ports" }
ben_snipes-application = { path = "crates/application" }
ben_snipes-config = { path = "crates/config" }
ben_snipes-adapter-statefile = { path = "crates/adapters/statefile" }
ben_snipes-adapter-dex-mock = { path = "crates/adapters/dex-mock" }
ben_snipes-adapter-ws-support = { path = "crates/adapters/ws-support" }
ben_snipes-adapter-pumpfun = { path = "crates/adapters/pumpfun" }
ben_snipes-adapter-evm-onchain = { path = "crates/adapters/evm-onchain" }

[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
panic = "abort"
strip = true

--- ./crates/config/src/lib.rs ---
//! Typed configuration for ben_snipes. Loads `config/default.toml` and
//! then lets environment variables prefixed `BEN_SNIPES_` override any
//! value, which is the layering you want for a bot: sane defaults in
//! version control, secrets and per-deployment tuning in the
//! environment, never the other way around.

use rust_decimal::Decimal;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to load configuration: {0}")]
    Load(#[from] config::ConfigError),
}

#[derive(Debug, Clone, Deserialize)]
pub struct RiskConfig {
    /// Take-profit target as a percentage above entry price, e.g. 10.0
    /// for the "+10%" strategy this bot is built around.
    pub take_profit_percent: Decimal,

    /// Stop-loss floor as a percentage below entry price, e.g. 5.0 to
    /// exit at -5%. Without this, a position that never reaches
    /// take-profit just sits open indefinitely.
    pub stop_loss_percent: Decimal,

    /// How often, in seconds, each listing source gets polled.
    pub poll_interval_seconds: u64,

    /// Maximum quote-currency amount to spend on a single new listing.
    /// This is the single most important number in the whole config for
    /// keeping a bad listing from being an expensive mistake.
    pub max_position_size: Decimal,

    /// A listing is only bought if its 24h volume is at or above this -
    /// the sole acquisition gate. Deliberately not paired with a market
    /// cap ceiling: a high-market-cap listing with genuinely active
    /// volume is just as tradeable as a low-cap one, so market cap isn't
    /// used to disqualify a listing either way.
    pub min_volume_24h: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SafetyConfig {
    /// Maximum acceptable sell tax, in basis points (100 = 1%), for a
    /// DEX listing to pass the honeypot/rug safety gate. Only applies to
    /// venues that have a `SafetyGate` configured - see the README.
    pub max_sell_tax_bps: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig {
    /// Directory where the statefile adapter keeps its per-source JSON
    /// snapshots, and where the acquisition ledger's file lives.
    pub state_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SolanaConfig {
    /// PumpPortal's data websocket URL. Defaults to their public free
    /// endpoint - no API key needed for `subscribeNewToken`.
    pub pumpportal_ws_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EvmChainConfig {
    /// e.g. "ethereum", "base" - becomes this source's chain identity.
    pub chain_name: String,
    /// A websocket RPC endpoint that supports `eth_subscribe`, with your
    /// own provider API key included. There is no usable default here -
    /// this must be supplied per deployment.
    pub ws_rpc_url: String,
    /// The DEX factory contract address to watch on this chain.
    pub factory_address: String,
    /// keccak256 topic hash of the pair/pool-creation event for this
    /// factory. Compute it yourself against the factory's actual ABI -
    /// see `ben_snipes-adapter-evm-onchain`'s docs for why this is never
    /// hardcoded as a default.
    pub topic0: String,
    /// Lowercased addresses of well-known base/quote assets on this
    /// chain (WETH, USDC, USDT, ...), used to identify which side of a
    /// new pair is the actual new listing.
    pub base_assets: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub risk: RiskConfig,
    pub safety: SafetyConfig,
    pub storage: StorageConfig,
    pub solana: SolanaConfig,
    /// Zero or more EVM chains to watch - one `EvmFactoryLogSource` gets
    /// spawned per entry. Empty by default; add entries in
    /// `config/default.toml` (or via env) per chain you want to watch.
    #[serde(default)]
    pub evm_chains: Vec<EvmChainConfig>,
}

impl AppConfig {
    /// Loads `config/default.toml` relative to the current working
    /// directory, then applies any `BEN_SNIPES_*` environment variable
    /// overrides (e.g. `BEN_SNIPES_RISK__MAX_POSITION_SIZE=50`).
    pub fn load() -> Result<Self, ConfigError> {
        let raw = config::Config::builder()
            .add_source(config::File::with_name("config/default"))
            .add_source(config::Environment::with_prefix("BEN_SNIPES").separator("__"))
            .build()?;

        Ok(raw.try_deserialize()?)
    }
}

--- ./crates/config/Cargo.toml ---
[package]
name = "ben_snipes-config"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "Typed application configuration loaded from config/default.toml, with environment variable overrides for secrets and per-environment tuning."

[dependencies]
serde = { workspace = true }
thiserror = { workspace = true }
config = { workspace = true }
rust_decimal = { workspace = true }

--- ./crates/application/src/position_manager.rs ---
use ben_snipes_domain::{ExitReason, Order, OrderSide, Position};
use ben_snipes_ports::{ExchangeClient, PortError};
use std::sync::Arc;
use tracing::info;

/// Watches a single open position and exits it once either the
/// take-profit target or the stop-loss floor is reached - see
/// `Position::exit_reason` for which one wins if both would somehow
/// trigger on the same price read.
pub struct PositionManager {
    exchange: Arc<dyn ExchangeClient>,
}

impl PositionManager {
    pub fn new(exchange: Arc<dyn ExchangeClient>) -> Self {
        Self { exchange }
    }

    /// Checks the current price against the position's take-profit and
    /// stop-loss. Returns `Some((order, reason))` if an exit order was
    /// submitted, `None` if neither threshold has been reached yet.
    pub async fn check_and_exit(
        &self,
        position: &Position,
    ) -> Result<Option<(Order, ExitReason)>, PortError> {
        let current_price = self.exchange.current_price(&position.symbol).await?;

        let Some(reason) = position.exit_reason(current_price) else {
            return Ok(None);
        };

        info!(
            symbol = position.symbol.as_str(),
            entry = %position.entry_price,
            current = %current_price,
            reason = ?reason,
            "exit threshold reached, submitting exit order"
        );

        let order = Order::new(
            position.venue.clone(),
            position.symbol.clone(),
            OrderSide::Sell,
            position.quantity,
        )?;

        let filled = self.exchange.submit_order(order).await?;
        Ok(Some((filled, reason)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ben_snipes_domain::{OrderStatus, ProfitTarget, StopLoss, Symbol, Venue, VenueKind};
    use rust_decimal::Decimal;

    struct StubExchange {
        price: Decimal,
    }

    #[async_trait]
    impl ExchangeClient for StubExchange {
        fn venue_name(&self) -> &str {
            "stub"
        }

        async fn current_price(&self, _symbol: &Symbol) -> Result<Decimal, PortError> {
            Ok(self.price)
        }

        async fn submit_order(&self, mut order: Order) -> Result<Order, PortError> {
            order.status = OrderStatus::Filled;
            Ok(order)
        }
    }

    fn sample_position() -> Position {
        let venue = Venue::new(VenueKind::Cex, "mexc").expect("literal venue is valid");
        let symbol = Symbol::new("PEPEUSDT").expect("literal symbol is valid");
        Position::new(
            venue,
            symbol,
            Decimal::ONE_HUNDRED,
            Decimal::TEN,
            ProfitTarget::from_percent(Decimal::TEN).expect("valid target"),
            StopLoss::from_percent(Decimal::from(5)).expect("valid stop-loss"),
        )
    }

    #[tokio::test]
    async fn holds_when_price_is_between_thresholds() {
        let manager = PositionManager::new(Arc::new(StubExchange {
            price: Decimal::from(102),
        }));

        let result = manager
            .check_and_exit(&sample_position())
            .await
            .expect("stub exchange cannot fail");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn exits_on_take_profit() {
        let manager = PositionManager::new(Arc::new(StubExchange {
            price: Decimal::from(115),
        }));

        let (_order, reason) = manager
            .check_and_exit(&sample_position())
            .await
            .expect("stub exchange cannot fail")
            .expect("price is above target, should exit");
        assert_eq!(reason, ExitReason::TakeProfit);
    }

    #[tokio::test]
    async fn exits_on_stop_loss() {
        let manager = PositionManager::new(Arc::new(StubExchange {
            price: Decimal::from(90),
        }));

        let (_order, reason) = manager
            .check_and_exit(&sample_position())
            .await
            .expect("stub exchange cannot fail")
            .expect("price is below floor, should exit");
        assert_eq!(reason, ExitReason::StopLoss);
    }
}

--- ./crates/application/src/new_listing_detector.rs ---
use ben_snipes_domain::Listing;
use ben_snipes_ports::{ListingSnapshot, ListingSource, ListingStateStore, PortError};
use std::sync::Arc;
use tracing::{debug, info};

/// Detects newly-appeared listings on a source, handling both strategies
/// a `ListingSource` can use:
///
/// - If the source supports incremental fetching (it returns
///   `ListingSnapshot::Incremental`), we trust it directly and just track
///   the cursor it hands back.
/// - If the source can only give us a full snapshot, we diff it against
///   the set of dedupe keys we saved last time and only surface what's
///   actually new.
///
/// Either way, callers get back exactly the same thing: a `Vec<Listing>`
/// of things they haven't seen before. Which strategy a given venue uses
/// is an adapter concern, invisible from here.
pub struct NewListingDetector {
    state_store: Arc<dyn ListingStateStore>,
}

impl NewListingDetector {
    pub fn new(state_store: Arc<dyn ListingStateStore>) -> Self {
        Self { state_store }
    }

    pub async fn poll(&self, source: &dyn ListingSource) -> Result<Vec<Listing>, PortError> {
        let source_id = source.source_id();
        let mut known = self.state_store.load(source_id).await?;

        let snapshot = source.poll(known.cursor.as_deref()).await?;

        let newly_seen = match snapshot {
            ListingSnapshot::Incremental { new, cursor } => {
                debug!(source_id, count = new.len(), "incremental poll");
                for listing in &new {
                    known.seen_keys.insert(listing.dedupe_key());
                }
                known.cursor = cursor;
                new
            }
            ListingSnapshot::Full(all) if !known.bootstrapped => {
                // First time we've ever seen this source's full symbol
                // list. Everything in it already existed before we
                // started watching - record it as the baseline and
                // report nothing as new. Buying "new listings" that were
                // actually already trading before the bot even started
                // is exactly the failure mode this branch exists to
                // prevent.
                info!(
                    source_id,
                    count = all.len(),
                    "establishing baseline snapshot, nothing reported as new"
                );
                for listing in &all {
                    known.seen_keys.insert(listing.dedupe_key());
                }
                known.bootstrapped = true;
                Vec::new()
            }
            ListingSnapshot::Full(all) => {
                debug!(source_id, count = all.len(), "full snapshot poll, diffing");
                let fresh: Vec<Listing> = all
                    .into_iter()
                    .filter(|listing| !known.seen_keys.contains(&listing.dedupe_key()))
                    .collect();
                for listing in &fresh {
                    known.seen_keys.insert(listing.dedupe_key());
                }
                fresh
            }
        };

        self.state_store.save(source_id, &known).await?;

        if !newly_seen.is_empty() {
            info!(source_id, count = newly_seen.len(), "new listings detected");
        }

        Ok(newly_seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ben_snipes_domain::{Chain, Symbol, Venue, VenueKind};
    use ben_snipes_ports::KnownListings;
    use std::sync::Mutex;
    use time::OffsetDateTime;

    /// An in-memory state store for tests, so we're not touching disk to
    /// verify diffing logic.
    struct InMemoryStateStore {
        state: Mutex<Option<KnownListings>>,
    }

    impl InMemoryStateStore {
        fn empty() -> Self {
            Self {
                state: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl ListingStateStore for InMemoryStateStore {
        async fn load(&self, _source_id: &str) -> Result<KnownListings, PortError> {
            let guard = self
                .state
                .lock()
                .expect("test-only mutex, poisoning here means a prior test panicked");
            Ok(guard.clone().unwrap_or_default())
        }

        async fn save(&self, _source_id: &str, new_state: &KnownListings) -> Result<(), PortError> {
            let mut guard = self
                .state
                .lock()
                .expect("test-only mutex, poisoning here means a prior test panicked");
            *guard = Some(new_state.clone());
            Ok(())
        }
    }

    struct FixedFullSnapshotSource {
        listings: Vec<Listing>,
    }

    #[async_trait]
    impl ListingSource for FixedFullSnapshotSource {
        fn source_id(&self) -> &str {
            "test-full"
        }

        async fn poll(&self, _cursor: Option<&str>) -> Result<ListingSnapshot, PortError> {
            Ok(ListingSnapshot::Full(self.listings.clone()))
        }
    }

    fn listing(symbol: &str) -> Listing {
        let venue = Venue::new(VenueKind::Dex, "raydium").expect("literal venue name is valid");
        let chain = Chain::new("solana").expect("literal chain is valid");
        let symbol = Symbol::new(symbol).expect("literal symbol is valid");
        Listing::new(symbol, venue, chain, OffsetDateTime::UNIX_EPOCH)
    }

    #[tokio::test]
    async fn first_poll_establishes_baseline_and_reports_nothing_new() {
        let store = Arc::new(InMemoryStateStore::empty());
        let detector = NewListingDetector::new(store);
        let source = FixedFullSnapshotSource {
            listings: vec![listing("AAAUSDT"), listing("BBBUSDT")],
        };

        // These symbols already existed before we started watching - the
        // very first poll must never report them as "new", or the bot
        // would try to buy every existing listing on startup.
        let result = detector.poll(&source).await.expect("in-memory store cannot fail");
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn second_poll_with_same_snapshot_returns_nothing_new() {
        let store = Arc::new(InMemoryStateStore::empty());
        let detector = NewListingDetector::new(store);
        let source = FixedFullSnapshotSource {
            listings: vec![listing("AAAUSDT")],
        };

        let first = detector.poll(&source).await.expect("in-memory store cannot fail");
        assert!(first.is_empty(), "first poll is the baseline, not new listings");

        let second = detector.poll(&source).await.expect("in-memory store cannot fail");
        assert!(second.is_empty());
    }

    #[tokio::test]
    async fn diff_only_surfaces_the_genuinely_new_symbol_after_baseline() {
        let store = Arc::new(InMemoryStateStore::empty());
        let detector = NewListingDetector::new(store);

        let first_source = FixedFullSnapshotSource {
            listings: vec![listing("AAAUSDT")],
        };
        let first = detector
            .poll(&first_source)
            .await
            .expect("in-memory store cannot fail");
        assert!(first.is_empty(), "first poll is the baseline, not new listings");

        let second_source = FixedFullSnapshotSource {
            listings: vec![listing("AAAUSDT"), listing("CCCUSDT")],
        };
        let second = detector
            .poll(&second_source)
            .await
            .expect("in-memory store cannot fail");

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].symbol.as_str(), "CCCUSDT");
    }
}

--- ./crates/application/src/lib.rs ---
//! `ben_snipes-application` is where the actual use cases live: services
//! that pull together one or more ports and apply domain rules to them.
//!
//! Nothing here knows what a "MEXC" or a "Raydium" is - it only knows
//! about `dyn ListingSource`, `dyn ListingStateStore`, and so on. That's
//! what lets the same `NewListingDetector` work identically whether it's
//! wired to a real exchange or a mock one in a test.

mod acquisition_engine;
mod new_listing_detector;
mod position_manager;

pub use acquisition_engine::{AcquisitionEngine, SafetyGate};
pub use new_listing_detector::NewListingDetector;
pub use position_manager::PositionManager;

--- ./crates/application/src/acquisition_engine.rs ---
use ben_snipes_domain::{
    AcquisitionCriteria, CanonicalTokenId, Listing, Order, OrderSide, Position, ProfitTarget,
    SafetyCriteria, StopLoss,
};
use ben_snipes_ports::{
    AcquisitionLedger, ExchangeClient, MetricsProvider, PortError, TokenSafetyChecker,
};
use rust_decimal::Decimal;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Bundles a `TokenSafetyChecker` with the `SafetyCriteria` it's judged
/// against. Kept as its own type (rather than two loose fields on
/// `AcquisitionEngine`) so the two can never be set independently of
/// each other - a checker with no criteria, or criteria with no
/// checker, isn't a state that should be representable.
///
/// Only construct this for venues where it's meaningful. A CEX venue
/// generally shouldn't have one at all - see the README.
pub struct SafetyGate {
    checker: Arc<dyn TokenSafetyChecker>,
    criteria: SafetyCriteria,
}

impl SafetyGate {
    pub fn new(checker: Arc<dyn TokenSafetyChecker>, criteria: SafetyCriteria) -> Self {
        Self { checker, criteria }
    }
}

/// Turns a detected `Listing` into an open `Position`, autonomously,
/// with no human in the loop.
///
/// The decision flow is deliberately linear and each step can bail out
/// cleanly with `Ok(None)`: no metrics yet, doesn't meet criteria, fails
/// the safety gate, or already reserved by another source, are all
/// expected outcomes, not failures - only genuine I/O errors come back
/// as `Err`. The `AcquisitionLedger` reservation happens last, right
/// before the buy, so a token only ever consumes a ledger slot once it's
/// actually about to be bought.
pub struct AcquisitionEngine {
    metrics_provider: Arc<dyn MetricsProvider>,
    exchange: Arc<dyn ExchangeClient>,
    ledger: Arc<dyn AcquisitionLedger>,
    criteria: AcquisitionCriteria,
    take_profit: ProfitTarget,
    stop_loss: StopLoss,
    /// Quote-currency amount to spend per position, e.g. 25.0 USDT.
    /// This is the single number that caps how much a single bad
    /// listing can cost - see the README for why this isn't optional.
    position_size: Decimal,
    safety_gate: Option<SafetyGate>,
}

impl AcquisitionEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        metrics_provider: Arc<dyn MetricsProvider>,
        exchange: Arc<dyn ExchangeClient>,
        ledger: Arc<dyn AcquisitionLedger>,
        criteria: AcquisitionCriteria,
        take_profit: ProfitTarget,
        stop_loss: StopLoss,
        position_size: Decimal,
        safety_gate: Option<SafetyGate>,
    ) -> Self {
        Self {
            metrics_provider,
            exchange,
            ledger,
            criteria,
            take_profit,
            stop_loss,
            position_size,
            safety_gate,
        }
    }

    /// Evaluates a freshly-detected listing and, if it qualifies, buys
    /// it. Returns `Ok(None)` for "we looked and passed" - not finding a
    /// reason to buy is the expected outcome for most listings, not a
    /// failure.
    pub async fn evaluate_and_buy(&self, listing: &Listing) -> Result<Option<Position>, PortError> {
        let Some(metrics) = self.metrics_provider.metrics(&listing.symbol).await? else {
            debug!(symbol = listing.symbol.as_str(), "no metrics yet, skipping");
            return Ok(None);
        };

        if !self.criteria.matches(&metrics) {
            debug!(
                symbol = listing.symbol.as_str(),
                volume = %metrics.volume_24h,
                "does not meet acquisition criteria, skipping"
            );
            return Ok(None);
        }

        if let Some(gate) = &self.safety_gate {
            let Some(report) = gate.checker.assess(&listing.symbol).await? else {
                debug!(symbol = listing.symbol.as_str(), "no safety assessment yet, skipping");
                return Ok(None);
            };

            if !gate.criteria.passes(&report) {
                info!(
                    symbol = listing.symbol.as_str(),
                    sell_tax_bps = report.sell_tax_bps,
                    ownership_renounced = report.ownership_renounced,
                    liquidity_locked = report.liquidity_locked,
                    is_mintable = report.is_mintable,
                    "failed safety check, skipping (likely honeypot/rug signal)"
                );
                return Ok(None);
            }
        }

        // Everything else passed - this is the point where two sources
        // reporting the same underlying token would otherwise cause a
        // double-buy. Reserve the canonical identity now, right before
        // committing capital, so the reservation window is as small as
        // possible.
        let canonical_id = CanonicalTokenId::from_listing(listing);
        if !self.ledger.try_reserve(canonical_id.as_str()).await? {
            debug!(
                symbol = listing.symbol.as_str(),
                canonical_id = %canonical_id,
                "already acquired via another source, skipping"
            );
            return Ok(None);
        }

        let price = match self.exchange.current_price(&listing.symbol).await {
            Ok(price) if price > Decimal::ZERO => price,
            Ok(_) => {
                debug!(symbol = listing.symbol.as_str(), "non-positive price quoted, skipping");
                self.release_reservation(&canonical_id).await;
                return Ok(None);
            }
            Err(e) => {
                self.release_reservation(&canonical_id).await;
                return Err(e);
            }
        };

        let quantity = self.position_size / price;

        let order = match Order::new(
            listing.venue.clone(),
            listing.symbol.clone(),
            OrderSide::Buy,
            quantity,
        ) {
            Ok(order) => order,
            Err(e) => {
                self.release_reservation(&canonical_id).await;
                return Err(e.into());
            }
        };

        let filled = match self.exchange.submit_order(order).await {
            Ok(filled) => filled,
            Err(e) => {
                // The reservation was for a buy that never actually
                // happened - release it so a later poll can retry this
                // token instead of it being permanently locked out by a
                // single transient failure.
                self.release_reservation(&canonical_id).await;
                return Err(e);
            }
        };

        info!(
            symbol = listing.symbol.as_str(),
            venue = %listing.venue,
            entry_price = %price,
            quantity = %filled.quantity,
            "autonomous buy executed"
        );

        let position = Position::new(
            listing.venue.clone(),
            listing.symbol.clone(),
            price,
            filled.quantity,
            self.take_profit,
            self.stop_loss,
        );

        Ok(Some(position))
    }

    async fn release_reservation(&self, canonical_id: &CanonicalTokenId) {
        if let Err(e) = self.ledger.release(canonical_id.as_str()).await {
            warn!(canonical_id = %canonical_id, error = %e, "failed to release ledger reservation after aborted buy");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ben_snipes_domain::{Chain, ListingMetrics, OrderStatus, SafetyReport, Symbol, Venue, VenueKind};
    use std::collections::HashSet;
    use time::OffsetDateTime;
    use tokio::sync::Mutex;

    struct StubMetricsProvider {
        report: Option<ListingMetrics>,
    }

    #[async_trait]
    impl MetricsProvider for StubMetricsProvider {
        async fn metrics(&self, _symbol: &Symbol) -> Result<Option<ListingMetrics>, PortError> {
            Ok(self.report)
        }
    }

    struct StubSafetyChecker {
        report: Option<SafetyReport>,
    }

    #[async_trait]
    impl TokenSafetyChecker for StubSafetyChecker {
        async fn assess(&self, _symbol: &Symbol) -> Result<Option<SafetyReport>, PortError> {
            Ok(self.report)
        }
    }

    struct StubExchange {
        price: Decimal,
        orders_submitted: Mutex<u32>,
        fail_submit: bool,
    }

    #[async_trait]
    impl ExchangeClient for StubExchange {
        fn venue_name(&self) -> &str {
            "stub"
        }

        async fn current_price(&self, _symbol: &Symbol) -> Result<Decimal, PortError> {
            Ok(self.price)
        }

        async fn submit_order(&self, mut order: Order) -> Result<Order, PortError> {
            if self.fail_submit {
                return Err(PortError::Rejected("stub configured to fail".to_string()));
            }
            *self.orders_submitted.lock().await += 1;
            order.status = OrderStatus::Filled;
            Ok(order)
        }
    }

    /// In-memory ledger for tests - same contract as the real
    /// file-backed one, just without touching disk.
    struct InMemoryLedger {
        reserved: Mutex<HashSet<String>>,
    }

    impl InMemoryLedger {
        fn empty() -> Self {
            Self {
                reserved: Mutex::new(HashSet::new()),
            }
        }
    }

    #[async_trait]
    impl AcquisitionLedger for InMemoryLedger {
        async fn try_reserve(&self, canonical_id: &str) -> Result<bool, PortError> {
            Ok(self.reserved.lock().await.insert(canonical_id.to_string()))
        }

        async fn release(&self, canonical_id: &str) -> Result<(), PortError> {
            self.reserved.lock().await.remove(canonical_id);
            Ok(())
        }
    }

    fn sample_listing() -> Listing {
        let venue = Venue::new(VenueKind::Dex, "raydium-test").expect("literal venue is valid");
        let chain = Chain::new("solana").expect("literal chain is valid");
        let symbol = Symbol::new("NEWCOIN").expect("literal symbol is valid");
        Listing::new(symbol, venue, chain, OffsetDateTime::UNIX_EPOCH)
    }

    fn passing_metrics() -> ListingMetrics {
        ListingMetrics {
            volume_24h: Decimal::from(200_000),
            market_cap: Decimal::from(500_000),
        }
    }

    fn build_engine(
        metrics: Option<ListingMetrics>,
        safety_gate: Option<SafetyGate>,
        exchange: Arc<StubExchange>,
        ledger: Arc<dyn AcquisitionLedger>,
    ) -> AcquisitionEngine {
        AcquisitionEngine::new(
            Arc::new(StubMetricsProvider { report: metrics }),
            exchange,
            ledger,
            AcquisitionCriteria::new(Decimal::from(50_000)).expect("literal criteria is valid"),
            ProfitTarget::from_percent(Decimal::TEN).expect("valid target"),
            StopLoss::from_percent(Decimal::from(5)).expect("valid stop-loss"),
            Decimal::from(25),
            safety_gate,
        )
    }

    #[tokio::test]
    async fn buys_when_no_safety_gate_configured() {
        let exchange = Arc::new(StubExchange {
            price: Decimal::ONE,
            orders_submitted: Mutex::new(0),
            fail_submit: false,
        });
        let engine = build_engine(Some(passing_metrics()), None, exchange.clone(), Arc::new(InMemoryLedger::empty()));

        let result = engine
            .evaluate_and_buy(&sample_listing())
            .await
            .expect("stub dependencies cannot fail");

        assert!(result.is_some());
        assert_eq!(*exchange.orders_submitted.lock().await, 1);
    }

    #[tokio::test]
    async fn skips_a_listing_that_fails_the_safety_gate() {
        let exchange = Arc::new(StubExchange {
            price: Decimal::ONE,
            orders_submitted: Mutex::new(0),
            fail_submit: false,
        });
        let dangerous_report = SafetyReport {
            sell_tax_bps: 9_000,
            ownership_renounced: false,
            liquidity_locked: false,
            is_mintable: true,
        };
        let gate = SafetyGate::new(
            Arc::new(StubSafetyChecker { report: Some(dangerous_report) }),
            SafetyCriteria::new(1_000),
        );
        let engine = build_engine(Some(passing_metrics()), Some(gate), exchange.clone(), Arc::new(InMemoryLedger::empty()));

        let result = engine
            .evaluate_and_buy(&sample_listing())
            .await
            .expect("stub dependencies cannot fail");

        assert!(result.is_none());
        assert_eq!(*exchange.orders_submitted.lock().await, 0);
    }

    #[tokio::test]
    async fn second_source_reporting_the_same_token_is_skipped_via_the_ledger() {
        let exchange = Arc::new(StubExchange {
            price: Decimal::ONE,
            orders_submitted: Mutex::new(0),
            fail_submit: false,
        });
        let ledger: Arc<dyn AcquisitionLedger> = Arc::new(InMemoryLedger::empty());

        let engine_a = build_engine(Some(passing_metrics()), None, exchange.clone(), ledger.clone());
        let engine_b = build_engine(Some(passing_metrics()), None, exchange.clone(), ledger.clone());

        // Two different "sources" (engines) reporting the exact same
        // canonical token (same chain + symbol) - only the first buy
        // should go through.
        let first = engine_a
            .evaluate_and_buy(&sample_listing())
            .await
            .expect("stub dependencies cannot fail");
        let second = engine_b
            .evaluate_and_buy(&sample_listing())
            .await
            .expect("stub dependencies cannot fail");

        assert!(first.is_some());
        assert!(second.is_none());
        assert_eq!(*exchange.orders_submitted.lock().await, 1);
    }

    #[tokio::test]
    async fn reservation_is_released_when_order_submission_fails() {
        let exchange = Arc::new(StubExchange {
            price: Decimal::ONE,
            orders_submitted: Mutex::new(0),
            fail_submit: true,
        });
        let ledger: Arc<dyn AcquisitionLedger> = Arc::new(InMemoryLedger::empty());
        let engine = build_engine(Some(passing_metrics()), None, exchange.clone(), ledger.clone());

        let result = engine.evaluate_and_buy(&sample_listing()).await;
        assert!(result.is_err());

        // The failed attempt should have released its reservation, so a
        // retry (a fresh engine, same ledger) can still claim this token.
        let canonical_id = CanonicalTokenId::from_listing(&sample_listing());
        let can_still_reserve = ledger
            .try_reserve(canonical_id.as_str())
            .await
            .expect("in-memory ledger cannot fail");
        assert!(can_still_reserve, "reservation should have been released after the failed submit");
    }
}

--- ./crates/application/Cargo.toml ---
[package]
name = "ben_snipes-application"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "Use-case orchestration: the services that coordinate domain rules and ports to actually do something, like detecting new listings or managing an open position."

[dependencies]
ben_snipes-domain = { workspace = true }
ben_snipes-ports = { workspace = true }
tracing = { workspace = true }
rust_decimal = { workspace = true }

[dev-dependencies]
async-trait = { workspace = true }
tokio = { workspace = true, features = ["sync"] }
time = { workspace = true }

--- ./crates/ports/src/exchange_client.rs ---
use crate::PortError;
use async_trait::async_trait;
use ben_snipes_domain::{Order, Symbol};
use rust_decimal::Decimal;

/// Trading operations against a single venue. A CEX adapter implements
/// this with signed REST calls; a DEX adapter implements it with signed
/// on-chain transactions (ideally routed through a private relay - see
/// the README for why that matters here). The application layer doesn't
/// need to know or care which.
#[async_trait]
pub trait ExchangeClient: Send + Sync {
    fn venue_name(&self) -> &str;

    /// Current price of `symbol` in the venue's quote asset.
    async fn current_price(&self, symbol: &Symbol) -> Result<Decimal, PortError>;

    /// Submit an order and return it with the venue's response applied
    /// (fill status, etc). Implementations are responsible for their own
    /// slippage/gas handling internally - the port only cares about intent
    /// in, result out.
    async fn submit_order(&self, order: Order) -> Result<Order, PortError>;
}

--- ./crates/ports/src/clock.rs ---
use time::OffsetDateTime;

/// Abstracts "what time is it" so application logic that stamps a
/// `Listing::first_seen` can be unit tested with a fixed clock instead of
/// depending on wall-clock time.
pub trait Clock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}

/// The real clock, used everywhere except tests.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

--- ./crates/ports/src/token_safety_checker.rs ---
use crate::PortError;
use async_trait::async_trait;
use ben_snipes_domain::{SafetyReport, Symbol};

/// Supplies the honeypot/rug safety signals `SafetyCriteria` checks
/// before a DEX buy. Separate from `MetricsProvider` because the two
/// concerns come from genuinely different data sources in a real
/// deployment - volume/market-cap usually comes from a price API,
/// contract safety usually comes from simulating a sell or from a
/// contract-analysis service (e.g. a token scanner) - and a venue could
/// reasonably have one without the other.
#[async_trait]
pub trait TokenSafetyChecker: Send + Sync {
    /// Returns `Ok(None)` if a safety assessment isn't available yet.
    /// `AcquisitionEngine` treats `None` the same as a failed check -
    /// not enough information to buy - never as permission to skip the
    /// gate.
    async fn assess(&self, symbol: &Symbol) -> Result<Option<SafetyReport>, PortError>;
}

--- ./crates/ports/src/lib.rs ---
//! `ben_snipes-ports` defines the boundary of the hexagon: traits that
//! describe what the application needs from the outside world (a
//! listings feed, a place to persist state, an exchange to trade on, a
//! clock) without saying anything about how those needs get met.
//!
//! Concrete implementations live in `ben_snipes-adapter-*` crates and get
//! wired in at the composition root (the `runner` binary). This is what
//! makes it possible to swap a mock CEX adapter for a real MEXC adapter
//! without touching a single line of application logic.

mod acquisition_ledger;
mod clock;
mod error;
mod exchange_client;
mod listing_source;
mod metrics_provider;
mod state_store;
mod token_safety_checker;

pub use acquisition_ledger::AcquisitionLedger;
pub use clock::Clock;
pub use error::PortError;
pub use exchange_client::ExchangeClient;
pub use listing_source::{ListingSnapshot, ListingSource};
pub use metrics_provider::MetricsProvider;
pub use state_store::{KnownListings, ListingStateStore};
pub use token_safety_checker::TokenSafetyChecker;

--- ./crates/ports/src/error.rs ---
use thiserror::Error;

/// Errors that cross the hexagon boundary: network failures, malformed
/// responses, disk errors. Kept separate from `ben_snipes_domain::DomainError`
/// on purpose, since "the exchange API timed out" and "you asked for a
/// negative take-profit" are different categories of problem and callers
/// often want to handle them differently (retry one, reject the other).
#[derive(Debug, Error)]
pub enum PortError {
    #[error("network request to {venue} failed: {source}")]
    Network {
        venue: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("failed to parse response from {venue}: {reason}")]
    MalformedResponse { venue: String, reason: String },

    #[error("state store I/O failed: {0}")]
    Storage(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("domain rule violated while translating adapter data: {0}")]
    Domain(#[from] ben_snipes_domain::DomainError),

    #[error("venue rejected the request: {0}")]
    Rejected(String),
}

--- ./crates/ports/src/state_store.rs ---
use crate::PortError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// What we remember about a single listing source between polls: the set
/// of dedupe keys (see `Listing::dedupe_key`) we've already seen, plus
/// whatever cursor that source gave us last time, if any.
///
/// This is what gets written to the statefile (or database, or key-value
/// store - whatever `ListingStateStore` adapter is wired in).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KnownListings {
    pub seen_keys: HashSet<String>,
    pub cursor: Option<String>,
    /// Whether we've ever recorded a baseline for this source. `false`
    /// means the very next full snapshot should be treated as "this is
    /// everything that already existed" rather than "this is all new" -
    /// without that distinction, the first poll of any full-snapshot
    /// source would flag its entire existing symbol universe as newly
    /// listed.
    #[serde(default)]
    pub bootstrapped: bool,
}

#[async_trait]
pub trait ListingStateStore: Send + Sync {
    /// Load what we last knew about a given source. Returns a default
    /// (empty) `KnownListings` if this source has never been polled
    /// before - that's not an error, it's the expected state on first run.
    async fn load(&self, source_id: &str) -> Result<KnownListings, PortError>;

    /// Persist the updated state for a source after a poll.
    async fn save(&self, source_id: &str, state: &KnownListings) -> Result<(), PortError>;
}

--- ./crates/ports/src/listing_source.rs ---
use crate::PortError;
use async_trait::async_trait;
use ben_snipes_domain::Listing;

/// What a listing source hands back on a single poll.
///
/// Some venues let you ask "give me everything new since cursor X" - a
/// paginated REST endpoint with a `since` param, or a WebSocket stream you
/// can resume from a sequence number. That's the cheap case: the venue
/// does the diffing for you, so we return `Incremental` and never need to
/// touch the full symbol list.
///
/// Other venues only expose "here is the full list of tradable symbols
/// right now" with no way to ask for just the delta. For those we return
/// `Full`, and the application layer (`NewListingDetector`) is responsible
/// for diffing it against what a `ListingStateStore` remembers from last
/// time.
///
/// Modelling both cases in one enum, rather than picking one strategy for
/// every adapter, is what lets a fast venue stay fast while a slow venue
/// still works correctly.
#[derive(Debug, Clone)]
pub enum ListingSnapshot {
    Full(Vec<Listing>),
    Incremental {
        new: Vec<Listing>,
        /// Opaque cursor to pass back on the next poll. Adapters define
        /// their own cursor format (a timestamp, a sequence number, a
        /// page token) - the application layer just stores and forwards
        /// it verbatim.
        cursor: Option<String>,
    },
}

#[async_trait]
pub trait ListingSource: Send + Sync {
    /// A short, unique name for this source, used as the key under which
    /// its state (known symbols / cursor) is persisted. E.g. "mexc" or
    /// "raydium".
    fn source_id(&self) -> &str;

    /// Poll for listings. `cursor` is whatever this source returned last
    /// time (`None` on the very first poll, or if this source doesn't do
    /// cursors at all). Implementations that don't support incremental
    /// fetching should just ignore the cursor and always return `Full`.
    async fn poll(&self, cursor: Option<&str>) -> Result<ListingSnapshot, PortError>;
}

--- ./crates/ports/src/metrics_provider.rs ---
use crate::PortError;
use async_trait::async_trait;
use ben_snipes_domain::{ListingMetrics, Symbol};

/// Supplies the volume/market-cap snapshot a listing needs before
/// `AcquisitionCriteria` can be applied to it. Kept separate from
/// `ExchangeClient` in the trait definition (even though in practice a
/// single adapter usually implements both, since exchanges bundle ticker
/// stats with price data) because not every venue that can price a
/// symbol can also tell you its market cap - a DEX adapter, for
/// instance, may need a different upstream (a token info API) for that.
#[async_trait]
pub trait MetricsProvider: Send + Sync {
    /// Returns `Ok(None)` if this provider has no metrics for the
    /// symbol yet (common right after a listing appears - volume/market
    /// cap data can lag the listing itself by a few seconds). Callers
    /// should treat `None` as "not enough information to buy", not as
    /// an error.
    async fn metrics(&self, symbol: &Symbol) -> Result<Option<ListingMetrics>, PortError>;
}

--- ./crates/ports/src/acquisition_ledger.rs ---
use crate::PortError;
use async_trait::async_trait;

/// The cross-source "have we already acted on this token" store.
///
/// This is what actually prevents a double-buy when more than one
/// `ListingSource` reports the same underlying token (via
/// `CanonicalTokenId`) - each source's own dedupe state in
/// `ListingStateStore` only knows "is this new to me", not "has anyone
/// already bought this". `AcquisitionEngine` calls `try_reserve`
/// immediately before submitting a buy order.
#[async_trait]
pub trait AcquisitionLedger: Send + Sync {
    /// Attempts to claim `canonical_id` for acquisition. Returns `true`
    /// if this call successfully claimed it - the caller now "owns" it
    /// and should proceed with the buy. Returns `false` if it was
    /// already claimed (by this call or an earlier one) - the caller
    /// must not buy.
    async fn try_reserve(&self, canonical_id: &str) -> Result<bool, PortError>;

    /// Releases a reservation. Used when a reserved buy never actually
    /// happened (e.g. the order submission failed after the reservation
    /// succeeded) - without this, a single transient failure would
    /// permanently block ever buying that token, since the reservation
    /// would still show as claimed forever.
    async fn release(&self, canonical_id: &str) -> Result<(), PortError>;
}

--- ./crates/ports/Cargo.toml ---
[package]
name = "ben_snipes-ports"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "Trait definitions (ports) that adapters implement and the application layer depends on. This is the seam of the hexagon: application code only ever talks to these traits, never to a concrete exchange or database."

[dependencies]
ben_snipes-domain = { workspace = true }
async-trait = { workspace = true }
thiserror = { workspace = true }
serde = { workspace = true }
rust_decimal = { workspace = true }
time = { workspace = true }

--- ./crates/domain/src/order.rs ---
use crate::{DomainError, Symbol, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderStatus {
    Pending,
    Filled,
    PartiallyFilled,
    Rejected,
    Cancelled,
}

/// A single buy or sell instruction. Adapters translate this into
/// whatever the venue actually needs (a signed REST payload for a CEX, a
/// signed transaction for a DEX) - the domain only cares about the intent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Order {
    pub venue: Venue,
    pub symbol: Symbol,
    pub side: OrderSide,
    pub quantity: Decimal,
    pub status: OrderStatus,
}

impl Order {
    pub fn new(
        venue: Venue,
        symbol: Symbol,
        side: OrderSide,
        quantity: Decimal,
    ) -> Result<Self, DomainError> {
        if quantity <= Decimal::ZERO {
            return Err(DomainError::InvalidQuantity(quantity.to_string()));
        }
        Ok(Self {
            venue,
            symbol,
            side,
            quantity,
            status: OrderStatus::Pending,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VenueKind;

    #[test]
    fn rejects_zero_or_negative_quantity() {
        let venue = Venue::new(VenueKind::Cex, "mexc").expect("literal name is valid");
        let symbol = Symbol::new("PEPEUSDT").expect("literal symbol is valid");
        let result = Order::new(venue, symbol, OrderSide::Buy, Decimal::ZERO);
        assert!(result.is_err());
    }
}

--- ./crates/domain/src/acquisition.rs ---
use crate::DomainError;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// The 24h volume and market cap of a symbol at the moment we're
/// deciding whether to buy it. This is deliberately a separate type from
/// `Listing` rather than fields bolted onto it - detecting that a symbol
/// exists is a different concern from assessing whether it's worth
/// buying, and a venue adapter can support one without the other.
///
/// `market_cap` is carried here for logging/sizing context even though
/// `AcquisitionCriteria` doesn't gate on it - see that type's docs for
/// why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListingMetrics {
    pub volume_24h: Decimal,
    pub market_cap: Decimal,
}

/// The filter that decides which newly-detected listings are worth
/// acquiring: active volume, full stop. A high market cap with real,
/// active trading volume is just as tradeable as a low one - what
/// actually matters for "can I get back out at +10%" is whether there's
/// a live market, not how big the token is. So this deliberately does
/// **not** gate on market cap at all; a listing qualifies purely on
/// whether its 24h volume clears the bar, whatever its market cap is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquisitionCriteria {
    min_volume_24h: Decimal,
}

impl AcquisitionCriteria {
    pub fn new(min_volume_24h: Decimal) -> Result<Self, DomainError> {
        if min_volume_24h < Decimal::ZERO {
            return Err(DomainError::InvalidMinVolume(min_volume_24h.to_string()));
        }
        Ok(Self { min_volume_24h })
    }

    pub fn matches(&self, metrics: &ListingMetrics) -> bool {
        metrics.volume_24h >= self.min_volume_24h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn criteria() -> AcquisitionCriteria {
        AcquisitionCriteria::new(Decimal::from(50_000)).expect("literal value here is valid")
    }

    #[test]
    fn rejects_negative_min_volume() {
        assert!(AcquisitionCriteria::new(Decimal::from(-1)).is_err());
    }

    #[test]
    fn accepts_zero_min_volume() {
        // Zero is a legitimate (if permissive) threshold - "any volume
        // at all counts" - so it should not be rejected the way a
        // negative value is.
        assert!(AcquisitionCriteria::new(Decimal::ZERO).is_ok());
    }

    #[test]
    fn matches_low_market_cap_listing_with_sufficient_volume() {
        let metrics = ListingMetrics {
            volume_24h: Decimal::from(200_000),
            market_cap: Decimal::from(500_000),
        };
        assert!(criteria().matches(&metrics));
    }

    #[test]
    fn matches_high_market_cap_listing_with_sufficient_volume() {
        // The whole point of this filter: a big market cap should not
        // disqualify a listing on its own, as long as volume is real.
        let metrics = ListingMetrics {
            volume_24h: Decimal::from(200_000),
            market_cap: Decimal::from(50_000_000),
        };
        assert!(criteria().matches(&metrics));
    }

    #[test]
    fn rejects_listing_with_insufficient_volume_regardless_of_market_cap() {
        let low_cap = ListingMetrics {
            volume_24h: Decimal::from(1_000),
            market_cap: Decimal::from(500_000),
        };
        let high_cap = ListingMetrics {
            volume_24h: Decimal::from(1_000),
            market_cap: Decimal::from(50_000_000),
        };
        assert!(!criteria().matches(&low_cap));
        assert!(!criteria().matches(&high_cap));
    }
}

--- ./crates/domain/src/venue.rs ---
use crate::DomainError;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Whether a venue is a centralised exchange (an API you authenticate
/// against) or a decentralised one (a chain you read/write on-chain state
/// against). Kept as a simple two-way split at the domain level; the
/// specifics of "which chain" or "which exchange" live in the venue name
/// and get resolved to a concrete adapter at the composition root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VenueKind {
    Cex,
    Dex,
}

impl fmt::Display for VenueKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VenueKind::Cex => write!(f, "cex"),
            VenueKind::Dex => write!(f, "dex"),
        }
    }
}

/// A trading venue, e.g. `Venue::new(VenueKind::Cex, "mexc")` or
/// `Venue::new(VenueKind::Dex, "raydium")`.
///
/// This is deliberately just a tag, not a live connection. The domain
/// layer doesn't know how to talk to MEXC or Raydium; it only needs to
/// know that a `Listing` came from one of them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Venue {
    kind: VenueKind,
    name: String,
}

impl Venue {
    pub fn new(kind: VenueKind, name: impl Into<String>) -> Result<Self, DomainError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(DomainError::EmptyVenueName);
        }
        Ok(Self {
            kind,
            name: name.to_lowercase(),
        })
    }

    pub fn kind(&self) -> VenueKind {
        self.kind
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for Venue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_name() {
        assert_eq!(
            Venue::new(VenueKind::Cex, "  "),
            Err(DomainError::EmptyVenueName)
        );
    }

    #[test]
    fn normalises_name_to_lowercase() {
        let venue = Venue::new(VenueKind::Dex, "Raydium").expect("valid name is fine here");
        assert_eq!(venue.name(), "raydium");
    }
}

--- ./crates/domain/src/safety.rs ---
use serde::{Deserialize, Serialize};

/// On-chain safety signals for a token, gathered before buying a DEX
/// listing. This is scoped to DEX-style acquisitions on purpose: a CEX
/// listing has already been through the exchange's own vetting (it
/// can't be an unsellable honeypot contract, because the exchange
/// controls the order book, not a smart contract the token author
/// wrote), so `AcquisitionEngine` only applies this gate when a
/// `SafetyGate` is actually configured for a venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyReport {
    /// Sell tax in basis points (100 = 1%). A high, unverifiable, or
    /// "can't even simulate a sell" tax is the single strongest honeypot
    /// signal - it's usually the actual mechanism a honeypot contract
    /// uses to trap buyers.
    pub sell_tax_bps: u32,
    /// Whether contract ownership has been renounced (no admin function
    /// left that could rug the token after purchase).
    pub ownership_renounced: bool,
    /// Whether the liquidity pool backing this token is time-locked
    /// (the classic "dev pulls liquidity" rug becomes much harder).
    pub liquidity_locked: bool,
    /// Whether the contract retains a mint function that could inflate
    /// supply, and therefore dump price, after purchase.
    pub is_mintable: bool,
}

/// The rule that decides whether a `SafetyReport` clears the bar to buy.
///
/// Deliberately conservative by default: a listing needs an acceptable
/// sell tax, must not be freely mintable, and must show at least one of
/// "ownership renounced" or "liquidity locked" - neither one alone is a
/// guarantee, but the complete absence of both is one of the most
/// reliable rug signals there is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyCriteria {
    max_sell_tax_bps: u32,
}

impl SafetyCriteria {
    pub fn new(max_sell_tax_bps: u32) -> Self {
        Self { max_sell_tax_bps }
    }

    pub fn passes(&self, report: &SafetyReport) -> bool {
        if report.sell_tax_bps > self.max_sell_tax_bps {
            return false;
        }
        if report.is_mintable {
            return false;
        }
        if !(report.ownership_renounced || report.liquidity_locked) {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn safe_report() -> SafetyReport {
        SafetyReport {
            sell_tax_bps: 200,
            ownership_renounced: true,
            liquidity_locked: true,
            is_mintable: false,
        }
    }

    #[test]
    fn accepts_a_clean_report() {
        let criteria = SafetyCriteria::new(1_000);
        assert!(criteria.passes(&safe_report()));
    }

    #[test]
    fn rejects_sell_tax_above_threshold() {
        let criteria = SafetyCriteria::new(500);
        let report = SafetyReport {
            sell_tax_bps: 900,
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn rejects_mintable_supply_regardless_of_other_signals() {
        let criteria = SafetyCriteria::new(1_000);
        let report = SafetyReport {
            is_mintable: true,
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn rejects_when_neither_renounced_nor_locked() {
        let criteria = SafetyCriteria::new(1_000);
        let report = SafetyReport {
            ownership_renounced: false,
            liquidity_locked: false,
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn accepts_when_only_liquidity_is_locked() {
        let criteria = SafetyCriteria::new(1_000);
        let report = SafetyReport {
            ownership_renounced: false,
            liquidity_locked: true,
            ..safe_report()
        };
        assert!(criteria.passes(&report));
    }
}

--- ./crates/domain/src/lib.rs ---
//! `ben_snipes-domain` holds the core business types for the bot: what a
//! listing is, what a venue is, what a position is, and the rules that
//! govern them (like "a take-profit percentage must be positive").
//!
//! Nothing in this crate talks to a network, a filesystem, or a clock.
//! That's on purpose - it's the "hexagon" in hexagonal architecture, and
//! keeping it pure means we can unit test all our business rules without
//! spinning up mock servers or touching disk.

mod acquisition;
mod canonical;
mod chain;
mod error;
mod listing;
mod order;
mod position;
mod safety;
mod venue;

pub use acquisition::{AcquisitionCriteria, ListingMetrics};
pub use canonical::CanonicalTokenId;
pub use chain::Chain;
pub use error::DomainError;
pub use listing::{Listing, Symbol};
pub use order::{Order, OrderSide, OrderStatus};
pub use position::{ExitReason, Position, ProfitTarget, StopLoss};
pub use safety::{SafetyCriteria, SafetyReport};
pub use venue::{Venue, VenueKind};

--- ./crates/domain/src/error.rs ---
use thiserror::Error;

/// Errors that come from violating a business rule, as opposed to errors
/// from I/O (those live in `ben_snipes-ports`, next to the traits that can
/// fail in I/O-flavoured ways).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DomainError {
    #[error("take-profit percentage must be positive, got {0}")]
    InvalidProfitTarget(String),

    #[error("stop-loss percentage must be positive, got {0}")]
    InvalidStopLoss(String),

    #[error("symbol cannot be empty")]
    EmptySymbol,

    #[error("venue name cannot be empty")]
    EmptyVenueName,

    #[error("chain identifier cannot be empty")]
    EmptyChain,

    #[error("order quantity must be positive, got {0}")]
    InvalidQuantity(String),

    #[error("min volume must not be negative, got {0}")]
    InvalidMinVolume(String),
}

--- ./crates/domain/src/position.rs ---
use crate::{DomainError, Symbol, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// A take-profit rule expressed as a percentage above entry price, e.g.
/// `ProfitTarget::from_percent(10)` for "sell at +10%".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfitTarget(Decimal);

impl ProfitTarget {
    pub fn from_percent(percent: Decimal) -> Result<Self, DomainError> {
        if percent <= Decimal::ZERO {
            return Err(DomainError::InvalidProfitTarget(percent.to_string()));
        }
        Ok(Self(percent))
    }

    pub fn percent(&self) -> Decimal {
        self.0
    }

    /// Given an entry price, what exit price hits this target.
    pub fn exit_price(&self, entry_price: Decimal) -> Decimal {
        entry_price + (entry_price * self.0 / Decimal::ONE_HUNDRED)
    }

    /// Whether the current price has reached this target relative to the
    /// given entry price.
    pub fn is_reached(&self, entry_price: Decimal, current_price: Decimal) -> bool {
        current_price >= self.exit_price(entry_price)
    }
}

/// A stop-loss rule expressed as a percentage below entry price, e.g.
/// `StopLoss::from_percent(5)` for "sell at -5%".
///
/// This exists because a take-profit target alone is a one-way bet: a
/// position that never reaches +10% just sits there forever, and a
/// single bad listing (a slow rug, a dead market with no buyers left)
/// can erase the gains from several good ones. Every position gets both
/// a ceiling and a floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopLoss(Decimal);

impl StopLoss {
    pub fn from_percent(percent: Decimal) -> Result<Self, DomainError> {
        if percent <= Decimal::ZERO {
            return Err(DomainError::InvalidStopLoss(percent.to_string()));
        }
        Ok(Self(percent))
    }

    pub fn percent(&self) -> Decimal {
        self.0
    }

    pub fn exit_price(&self, entry_price: Decimal) -> Decimal {
        entry_price - (entry_price * self.0 / Decimal::ONE_HUNDRED)
    }

    pub fn is_triggered(&self, entry_price: Decimal, current_price: Decimal) -> bool {
        current_price <= self.exit_price(entry_price)
    }
}

/// Why a position was (or should be) closed. Kept separate from a plain
/// `bool` so `PositionManager` can log and act on the actual reason
/// rather than just "something said sell" - a take-profit and a
/// stop-loss are very different outcomes worth telling apart in logs
/// and, eventually, in P&L reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    TakeProfit,
    StopLoss,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub venue: Venue,
    pub symbol: Symbol,
    pub entry_price: Decimal,
    pub quantity: Decimal,
    pub target: ProfitTarget,
    pub stop_loss: StopLoss,
}

impl Position {
    pub fn new(
        venue: Venue,
        symbol: Symbol,
        entry_price: Decimal,
        quantity: Decimal,
        target: ProfitTarget,
        stop_loss: StopLoss,
    ) -> Self {
        Self {
            venue,
            symbol,
            entry_price,
            quantity,
            target,
            stop_loss,
        }
    }

    /// Checks the take-profit first, then the stop-loss. If somehow both
    /// would trigger on the same price read (only possible with a
    /// pathological config where the stop-loss percent exceeds the
    /// take-profit percent), take-profit wins - exiting at a gain is
    /// never the wrong call.
    pub fn exit_reason(&self, current_price: Decimal) -> Option<ExitReason> {
        if self.target.is_reached(self.entry_price, current_price) {
            Some(ExitReason::TakeProfit)
        } else if self.stop_loss.is_triggered(self.entry_price, current_price) {
            Some(ExitReason::StopLoss)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_percent_target_computes_correct_exit_price() {
        let target =
            ProfitTarget::from_percent(Decimal::TEN).expect("ten percent is a valid target");
        let exit = target.exit_price(Decimal::ONE_HUNDRED);
        assert_eq!(exit, Decimal::from(110));
    }

    #[test]
    fn rejects_non_positive_target() {
        assert!(ProfitTarget::from_percent(Decimal::ZERO).is_err());
    }

    #[test]
    fn five_percent_stop_loss_computes_correct_exit_price() {
        let stop_loss =
            StopLoss::from_percent(Decimal::from(5)).expect("five percent is a valid stop-loss");
        let exit = stop_loss.exit_price(Decimal::ONE_HUNDRED);
        assert_eq!(exit, Decimal::from(95));
    }

    #[test]
    fn rejects_non_positive_stop_loss() {
        assert!(StopLoss::from_percent(Decimal::ZERO).is_err());
    }

    fn sample_position() -> Position {
        let venue = crate::Venue::new(crate::VenueKind::Cex, "mexc").expect("literal venue is valid");
        let symbol = crate::Symbol::new("PEPEUSDT").expect("literal symbol is valid");
        Position::new(
            venue,
            symbol,
            Decimal::ONE_HUNDRED,
            Decimal::TEN,
            ProfitTarget::from_percent(Decimal::TEN).expect("valid target"),
            StopLoss::from_percent(Decimal::from(5)).expect("valid stop-loss"),
        )
    }

    #[test]
    fn exit_reason_is_none_between_the_two_thresholds() {
        let position = sample_position();
        assert_eq!(position.exit_reason(Decimal::from(102)), None);
    }

    #[test]
    fn exit_reason_is_take_profit_at_or_above_target() {
        let position = sample_position();
        assert_eq!(position.exit_reason(Decimal::from(110)), Some(ExitReason::TakeProfit));
    }

    #[test]
    fn exit_reason_is_stop_loss_at_or_below_floor() {
        let position = sample_position();
        assert_eq!(position.exit_reason(Decimal::from(95)), Some(ExitReason::StopLoss));
    }
}

--- ./crates/domain/src/listing.rs ---
use crate::{Chain, DomainError, Venue};
use serde::{Deserialize, Serialize};
use std::fmt;
use time::OffsetDateTime;

/// A ticker, pair, or token identifier as the venue names it. We keep this
/// as an opaque string rather than parsing it into base/quote assets here,
/// because that parsing is venue-specific (a CEX gives you "BTCUSDT", a
/// Solana DEX gives you a base58 mint address) and belongs in the adapter
/// that produced it, not in the domain.
///
/// For DEX listings, adapters should set this to the token's actual
/// contract/mint address (lowercased), not a display ticker - the
/// address is what `CanonicalTokenId` keys on, and tickers can collide
/// or be spoofed in a way an address can't.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Symbol(String);

impl Symbol {
    pub fn new(raw: impl Into<String>) -> Result<Self, DomainError> {
        let raw = raw.into();
        if raw.trim().is_empty() {
            return Err(DomainError::EmptySymbol);
        }
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A symbol observed as tradable on a venue, at the time we first saw it.
/// This is the unit that flows out of listing detection and into the
/// "should we buy this" decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub symbol: Symbol,
    pub venue: Venue,
    pub chain: Chain,
    pub first_seen: OffsetDateTime,
}

impl Listing {
    pub fn new(symbol: Symbol, venue: Venue, chain: Chain, first_seen: OffsetDateTime) -> Self {
        Self {
            symbol,
            venue,
            chain,
            first_seen,
        }
    }

    /// A stable key for diffing snapshots against a single source's
    /// state store. Two listings with the same key are "the same
    /// listing" *as far as that one source is concerned* - this is
    /// intentionally per-venue, not per-chain, so it stays correct even
    /// before `CanonicalTokenId` gets involved. See
    /// `crate::CanonicalTokenId` for the cross-source identity used to
    /// avoid buying the same token twice via two different sources.
    pub fn dedupe_key(&self) -> String {
        format!("{}::{}", self.venue, self.symbol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VenueKind;

    #[test]
    fn rejects_empty_symbol() {
        assert_eq!(Symbol::new(""), Err(DomainError::EmptySymbol));
    }

    #[test]
    fn dedupe_key_combines_venue_and_symbol() {
        let venue = Venue::new(VenueKind::Dex, "pumpfun").expect("literal name is valid");
        let chain = Chain::new("solana").expect("literal chain is valid");
        let symbol = Symbol::new("someMintAddress111").expect("literal symbol is valid");
        let listing = Listing::new(symbol, venue, chain, OffsetDateTime::UNIX_EPOCH);
        assert_eq!(listing.dedupe_key(), "dex:pumpfun::someMintAddress111");
    }
}

--- ./crates/domain/src/chain.rs ---
use crate::DomainError;
use serde::{Deserialize, Serialize};
use std::fmt;

/// The blockchain a DEX-listed token actually lives on - "solana",
/// "ethereum", "base", etc. This is deliberately a separate concept
/// from `Venue`: `Venue` identifies *which source/DEX* reported a
/// listing (e.g. "pumpfun", "uniswap-v2-ethereum"), while `Chain`
/// identifies *where the token itself exists on-chain*.
///
/// That split is what makes cross-source deduplication possible: two
/// different sources watching the same chain (say, our own on-chain
/// watcher and a third-party indexer, both watching Ethereum) can
/// report the same token through two different `Venue`s, but they'll
/// always agree on `Chain` - so `CanonicalTokenId` keys on chain +
/// address, not on venue.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Chain(String);

impl Chain {
    pub fn new(raw: impl Into<String>) -> Result<Self, DomainError> {
        let raw = raw.into();
        if raw.trim().is_empty() {
            return Err(DomainError::EmptyChain);
        }
        Ok(Self(raw.to_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Chain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_chain() {
        assert_eq!(Chain::new("  "), Err(DomainError::EmptyChain));
    }

    #[test]
    fn normalises_to_lowercase() {
        let chain = Chain::new("Solana").expect("literal chain is valid");
        assert_eq!(chain.as_str(), "solana");
    }
}

--- ./crates/domain/src/canonical.rs ---
use crate::Listing;
use serde::{Deserialize, Serialize};
use std::fmt;

/// The identity a token has regardless of which source detected it or
/// which specific DEX its pool lives on: its chain, plus its lowercased
/// contract/mint address.
///
/// This is deliberately coarser than `Listing::dedupe_key` (which is
/// per-venue, so it can track "is this new to source X"). Two different
/// sources - our own on-chain watcher and a third-party indexer, say,
/// both watching Solana - can each produce a `Listing` with a different
/// `Venue` for the exact same token. Those two listings have different
/// `dedupe_key()`s (correctly - each source needs its own "have I seen
/// this" memory) but the *same* `CanonicalTokenId`, because they're
/// describing the same underlying asset.
///
/// `AcquisitionEngine` reserves a `CanonicalTokenId` in the
/// `AcquisitionLedger` immediately before buying, so whichever source
/// gets there first wins the reservation and every other source's report
/// of the same token becomes a no-op - this is the actual mechanism that
/// stops the bot from buying a token twice because two sources both
/// noticed it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CanonicalTokenId(String);

impl CanonicalTokenId {
    pub fn from_listing(listing: &Listing) -> Self {
        Self(format!(
            "{}:{}",
            listing.chain.as_str(),
            listing.symbol.as_str().to_lowercase()
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CanonicalTokenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Chain, Symbol, Venue, VenueKind};
    use time::OffsetDateTime;

    fn listing_with(venue_name: &str, symbol: &str) -> Listing {
        let venue = Venue::new(VenueKind::Dex, venue_name).expect("literal venue is valid");
        let chain = Chain::new("solana").expect("literal chain is valid");
        let symbol = Symbol::new(symbol).expect("literal symbol is valid");
        Listing::new(symbol, venue, chain, OffsetDateTime::UNIX_EPOCH)
    }

    #[test]
    fn two_different_venues_reporting_the_same_token_share_a_canonical_id() {
        let from_source_a = listing_with("pumpfun", "SomeMintAddress111");
        let from_source_b = listing_with("birdeye-poller", "somemintaddress111");

        assert_eq!(
            CanonicalTokenId::from_listing(&from_source_a),
            CanonicalTokenId::from_listing(&from_source_b)
        );
    }

    #[test]
    fn different_tokens_on_the_same_venue_have_different_canonical_ids() {
        let a = listing_with("pumpfun", "MintAddressA");
        let b = listing_with("pumpfun", "MintAddressB");
        assert_ne!(CanonicalTokenId::from_listing(&a), CanonicalTokenId::from_listing(&b));
    }
}

--- ./crates/domain/Cargo.toml ---
[package]
name = "ben_snipes-domain"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "Core business types and rules for ben_snipes. No I/O, no async runtime, no adapters - just the shapes and logic that define what a listing, a position, and a profit target are."

[dependencies]
serde = { workspace = true }
thiserror = { workspace = true }
time = { workspace = true }
rust_decimal = { workspace = true }

--- ./crates/adapters/statefile/src/lib.rs ---
//! A `ListingStateStore` backed by plain JSON files on disk. One file per
//! source (e.g. `state/mexc.json`, `state/raydium.json`), so unrelated
//! sources never contend for the same file.
//!
//! This is the simplest adapter that could possibly work, which makes it
//! a good default and a good reference for writing a fancier one later
//! (sqlite, redis, whatever scaling calls for). Swapping it out means
//! writing a new struct that implements `ListingStateStore` - nothing
//! upstream of the port needs to change.

use async_trait::async_trait;
use ben_snipes_ports::{KnownListings, ListingStateStore, PortError};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::debug;

mod ledger;
pub use ledger::FileAcquisitionLedger;

pub struct StatefileStore {
    directory: PathBuf,
}

impl StatefileStore {
    /// `directory` is created if it doesn't already exist the first time
    /// `save` is called - we don't touch the filesystem in the
    /// constructor, since construction should never fail on its own.
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    fn path_for(&self, source_id: &str) -> PathBuf {
        // Source IDs are adapter-controlled short names (like "mexc"), not
        // user input, but we still guard against anything that would
        // escape the state directory if that ever changes.
        let sanitised: String = source_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        self.directory.join(format!("{sanitised}.json"))
    }

    async fn ensure_directory_exists(&self) -> Result<(), PortError> {
        fs::create_dir_all(&self.directory)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))
    }
}

#[async_trait]
impl ListingStateStore for StatefileStore {
    async fn load(&self, source_id: &str) -> Result<KnownListings, PortError> {
        let path = self.path_for(source_id);

        if !path_exists(&path).await {
            debug!(source_id, "no existing state file, starting fresh");
            return Ok(KnownListings::default());
        }

        let raw = fs::read_to_string(&path)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;

        serde_json::from_str(&raw).map_err(|e| PortError::MalformedResponse {
            venue: source_id.to_string(),
            reason: e.to_string(),
        })
    }

    async fn save(&self, source_id: &str, state: &KnownListings) -> Result<(), PortError> {
        self.ensure_directory_exists().await?;

        let path = self.path_for(source_id);
        let tmp_path = path.with_extension("json.tmp");

        let serialised = serde_json::to_vec_pretty(state).map_err(|e| PortError::Storage(Box::new(e)))?;

        // Write to a temp file and rename over the real one. Rename is
        // atomic on the same filesystem, so a crash mid-write can never
        // leave us with a half-written, unparseable state file - worst
        // case we lose the update and fall back to what was there before.
        fs::write(&tmp_path, serialised)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;
        fs::rename(&tmp_path, &path)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;

        Ok(())
    }
}

async fn path_exists(path: &Path) -> bool {
    fs::metadata(path).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[tokio::test]
    async fn round_trips_state_through_disk() {
        let dir = std::env::temp_dir().join(format!("ben_snipes-test-{}", uuid_like()));
        let store = StatefileStore::new(&dir);

        let mut seen = HashSet::new();
        seen.insert("cex:mexc::AAAUSDT".to_string());
        let state = KnownListings {
            seen_keys: seen,
            cursor: Some("cursor-123".to_string()),
        };

        store.save("mexc", &state).await.expect("save to temp dir should not fail");
        let loaded = store.load("mexc").await.expect("load from temp dir should not fail");

        assert_eq!(loaded.cursor, Some("cursor-123".to_string()));
        assert!(loaded.seen_keys.contains("cex:mexc::AAAUSDT"));

        // Clean up after ourselves; not load-bearing for the test result.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_file_returns_default_state() {
        let dir = std::env::temp_dir().join(format!("ben_snipes-test-{}", uuid_like()));
        let store = StatefileStore::new(&dir);

        let loaded = store
            .load("never-seen-before")
            .await
            .expect("missing file is not an error, it's a fresh start");

        assert!(loaded.seen_keys.is_empty());
        assert!(loaded.cursor.is_none());
    }

    /// A tiny, dependency-free stand-in for a UUID so tests don't collide
    /// on temp directory names. Not for use outside tests.
    fn uuid_like() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should never be before the epoch in CI")
            .as_nanos()
    }
}

--- ./crates/adapters/statefile/src/ledger.rs ---
//! A file-backed `AcquisitionLedger`: a single JSON file holding the set
//! of canonical token IDs we've already acted on, guarded by an
//! in-process async mutex so `try_reserve` is atomic within this
//! process.
//!
//! That last qualifier matters: this only guarantees "no double-reserve
//! within one running instance" - it does **not** coordinate across
//! multiple bot processes sharing the same ledger file. Running more
//! than one instance of ben_snipes against the same state directory
//! needs a real concurrent store (e.g. a database with a unique
//! constraint) instead of this one. See the README.

use async_trait::async_trait;
use ben_snipes_ports::{AcquisitionLedger, PortError};
use std::collections::HashSet;
use std::path::PathBuf;
use tokio::fs;
use tokio::sync::Mutex;

pub struct FileAcquisitionLedger {
    path: PathBuf,
    reserved: Mutex<HashSet<String>>,
}

impl FileAcquisitionLedger {
    /// Loads the ledger from `path` if it exists, or starts empty if it
    /// doesn't (a fresh deployment has nothing reserved yet - that's the
    /// expected first-run state, not an error). Unlike `StatefileStore`,
    /// this does its I/O at construction time rather than lazily,
    /// because the ledger's whole contract depends on having the
    /// complete set loaded before the first `try_reserve` call.
    pub async fn load(path: impl Into<PathBuf>) -> Result<Self, PortError> {
        let path = path.into();

        let reserved = match fs::read_to_string(&path).await {
            Ok(raw) => serde_json::from_str(&raw).map_err(|e| PortError::MalformedResponse {
                venue: "acquisition-ledger".to_string(),
                reason: e.to_string(),
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
            Err(e) => return Err(PortError::Storage(Box::new(e))),
        };

        Ok(Self {
            path,
            reserved: Mutex::new(reserved),
        })
    }

    async fn persist(&self, snapshot: &HashSet<String>) -> Result<(), PortError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| PortError::Storage(Box::new(e)))?;
        }

        let tmp_path = self.path.with_extension("json.tmp");
        let serialised = serde_json::to_vec_pretty(snapshot).map_err(|e| PortError::Storage(Box::new(e)))?;

        // Same atomic temp-file-then-rename pattern as StatefileStore -
        // a crash mid-write can never corrupt the ledger, worst case we
        // lose the very last reservation and re-derive it on retry.
        fs::write(&tmp_path, serialised)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;
        fs::rename(&tmp_path, &self.path)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;

        Ok(())
    }
}

#[async_trait]
impl AcquisitionLedger for FileAcquisitionLedger {
    async fn try_reserve(&self, canonical_id: &str) -> Result<bool, PortError> {
        let mut guard = self.reserved.lock().await;
        if guard.contains(canonical_id) {
            return Ok(false);
        }
        guard.insert(canonical_id.to_string());
        self.persist(&guard).await?;
        Ok(true)
    }

    async fn release(&self, canonical_id: &str) -> Result<(), PortError> {
        let mut guard = self.reserved.lock().await;
        if guard.remove(canonical_id) {
            self.persist(&guard).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_ledger_path() -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should never be before the epoch in CI")
            .as_nanos();
        std::env::temp_dir().join(format!("ben_snipes-ledger-test-{nanos}.json"))
    }

    #[tokio::test]
    async fn first_reservation_succeeds_second_is_rejected() {
        let path = temp_ledger_path();
        let ledger = FileAcquisitionLedger::load(&path)
            .await
            .expect("fresh path should load as empty");

        let first = ledger.try_reserve("solana:abc").await.expect("reserve should not fail");
        let second = ledger.try_reserve("solana:abc").await.expect("reserve should not fail");

        assert!(first);
        assert!(!second);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn released_reservation_can_be_reclaimed() {
        let path = temp_ledger_path();
        let ledger = FileAcquisitionLedger::load(&path)
            .await
            .expect("fresh path should load as empty");

        assert!(ledger.try_reserve("solana:abc").await.expect("reserve should not fail"));
        ledger.release("solana:abc").await.expect("release should not fail");
        assert!(ledger.try_reserve("solana:abc").await.expect("reserve should not fail"));

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn reservations_persist_across_a_reload() {
        let path = temp_ledger_path();
        {
            let ledger = FileAcquisitionLedger::load(&path)
                .await
                .expect("fresh path should load as empty");
            ledger.try_reserve("solana:abc").await.expect("reserve should not fail");
        }

        let reloaded = FileAcquisitionLedger::load(&path)
            .await
            .expect("existing file should load");
        let can_reserve_again = reloaded
            .try_reserve("solana:abc")
            .await
            .expect("reserve should not fail");

        assert!(!can_reserve_again, "reservation from before the reload should still hold");

        let _ = std::fs::remove_file(&path);
    }
}

--- ./crates/adapters/statefile/Cargo.toml ---
[package]
name = "ben_snipes-adapter-statefile"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "ListingStateStore implementation backed by one JSON file per source on local disk. Simple, dependency-free persistence for sources that can't do incremental fetches and need a full-snapshot diff instead."

[dependencies]
ben_snipes-ports = { workspace = true }
async-trait = { workspace = true }
serde_json = { workspace = true }
tokio = { workspace = true, features = ["sync"] }
tracing = { workspace = true }

--- ./crates/adapters/dex-mock/src/lib.rs ---
//! A fake DEX. Real DEXes generally let you watch on-chain events (a
//! pool-created log, say) rather than only offering a "list everything"
//! endpoint, so `MockDexSource` simulates that: each simulated pool gets
//! a monotonically increasing block number, and `poll` only returns pools
//! created after the cursor it's given. That exercises the
//! `ListingSnapshot::Incremental` path in `NewListingDetector`, as
//! opposed to the `Full`-snapshot-diff path the CEX mock exercises.
//!
//! Replace this with a real adapter (Raydium, Uniswap, etc) by watching
//! the venue's actual pool-creation events/logs. Nothing outside this
//! crate needs to change.

use async_trait::async_trait;
use ben_snipes_domain::{
    Chain, Listing, ListingMetrics, Order, OrderStatus, SafetyReport, Symbol, Venue, VenueKind,
};
use ben_snipes_ports::{
    ExchangeClient, ListingSnapshot, ListingSource, MetricsProvider, PortError, TokenSafetyChecker,
};
use rust_decimal::Decimal;
use std::collections::HashMap;
use time::OffsetDateTime;
use tokio::sync::Mutex;

struct SimulatedPool {
    symbol: String,
    block: u64,
}

pub struct MockDexSource {
    venue_name: String,
    pools: Mutex<Vec<SimulatedPool>>,
    current_block: Mutex<u64>,
}

impl MockDexSource {
    pub fn new(venue_name: impl Into<String>) -> Self {
        Self {
            venue_name: venue_name.into(),
            pools: Mutex::new(Vec::new()),
            current_block: Mutex::new(0),
        }
    }

    /// Simulates a new pool being created on-chain at the next block.
    pub async fn simulate_new_pool(&self, symbol: impl Into<String>) {
        let mut block = self.current_block.lock().await;
        *block += 1;
        self.pools.lock().await.push(SimulatedPool {
            symbol: symbol.into(),
            block: *block,
        });
    }
}

#[async_trait]
impl ListingSource for MockDexSource {
    fn source_id(&self) -> &str {
        &self.venue_name
    }

    async fn poll(&self, cursor: Option<&str>) -> Result<ListingSnapshot, PortError> {
        let since_block: u64 = cursor
            .map(|c| {
                c.parse().map_err(|_| PortError::MalformedResponse {
                    venue: self.venue_name.clone(),
                    reason: format!("cursor '{c}' is not a valid block number"),
                })
            })
            .transpose()?
            .unwrap_or(0);

        let venue = Venue::new(VenueKind::Dex, self.venue_name.clone())?;
        let chain = Chain::new("solana")?;
        let pools = self.pools.lock().await;

        let mut new_listings = Vec::new();
        let mut latest_block = since_block;

        for pool in pools.iter().filter(|p| p.block > since_block) {
            let symbol = Symbol::new(pool.symbol.clone())?;
            new_listings.push(Listing::new(symbol, venue.clone(), chain.clone(), OffsetDateTime::now_utc()));
            latest_block = latest_block.max(pool.block);
        }

        Ok(ListingSnapshot::Incremental {
            new: new_listings,
            cursor: Some(latest_block.to_string()),
        })
    }
}

pub struct MockDexClient {
    venue_name: String,
    price: Mutex<Decimal>,
    metrics: Mutex<HashMap<String, ListingMetrics>>,
    safety_reports: Mutex<HashMap<String, SafetyReport>>,
}

impl MockDexClient {
    pub fn new(venue_name: impl Into<String>, starting_price: Decimal) -> Self {
        Self {
            venue_name: venue_name.into(),
            price: Mutex::new(starting_price),
            metrics: Mutex::new(HashMap::new()),
            safety_reports: Mutex::new(HashMap::new()),
        }
    }

    pub async fn set_price(&self, new_price: Decimal) {
        *self.price.lock().await = new_price;
    }

    /// Seeds volume/market-cap data for a symbol. A real DEX adapter
    /// would derive this from on-chain liquidity depth and trade volume
    /// rather than a hand-set map.
    pub async fn set_metrics(&self, symbol: impl Into<String>, metrics: ListingMetrics) {
        self.metrics.lock().await.insert(symbol.into(), metrics);
    }

    /// Seeds a honeypot/rug safety report for a symbol. A real adapter
    /// would get this by simulating a sell against the token contract
    /// and inspecting ownership/liquidity-lock state on-chain, rather
    /// than a hand-set map.
    pub async fn set_safety_report(&self, symbol: impl Into<String>, report: SafetyReport) {
        self.safety_reports.lock().await.insert(symbol.into(), report);
    }
}

#[async_trait]
impl ExchangeClient for MockDexClient {
    fn venue_name(&self) -> &str {
        &self.venue_name
    }

    async fn current_price(&self, _symbol: &Symbol) -> Result<Decimal, PortError> {
        Ok(*self.price.lock().await)
    }

    async fn submit_order(&self, mut order: Order) -> Result<Order, PortError> {
        // A real adapter would build, sign, and broadcast an on-chain
        // transaction here, ideally through a private relay to avoid
        // getting sandwiched. See the README for why that matters.
        order.status = OrderStatus::Filled;
        Ok(order)
    }
}

#[async_trait]
impl MetricsProvider for MockDexClient {
    async fn metrics(&self, symbol: &Symbol) -> Result<Option<ListingMetrics>, PortError> {
        Ok(self.metrics.lock().await.get(symbol.as_str()).copied())
    }
}

#[async_trait]
impl TokenSafetyChecker for MockDexClient {
    async fn assess(&self, symbol: &Symbol) -> Result<Option<SafetyReport>, PortError> {
        Ok(self.safety_reports.lock().await.get(symbol.as_str()).copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_poll_with_no_cursor_returns_all_pools_so_far() {
        let source = MockDexSource::new("raydium-test");
        source.simulate_new_pool("AAA").await;
        source.simulate_new_pool("BBB").await;

        let snapshot = source.poll(None).await.expect("mock source cannot fail");
        match snapshot {
            ListingSnapshot::Incremental { new, cursor } => {
                assert_eq!(new.len(), 2);
                assert_eq!(cursor, Some("2".to_string()));
            }
            ListingSnapshot::Full(_) => panic!("dex mock should always be incremental"),
        }
    }

    #[tokio::test]
    async fn poll_with_cursor_only_returns_pools_after_it() {
        let source = MockDexSource::new("raydium-test");
        source.simulate_new_pool("AAA").await;
        source.simulate_new_pool("BBB").await;

        let first = source.poll(None).await.expect("mock source cannot fail");
        let cursor = match first {
            ListingSnapshot::Incremental { cursor, .. } => cursor,
            _ => panic!("expected incremental"),
        };

        source.simulate_new_pool("CCC").await;

        let second = source
            .poll(cursor.as_deref())
            .await
            .expect("mock source cannot fail");
        match second {
            ListingSnapshot::Incremental { new, .. } => {
                assert_eq!(new.len(), 1);
                assert_eq!(new[0].symbol.as_str(), "CCC");
            }
            ListingSnapshot::Full(_) => panic!("dex mock should always be incremental"),
        }
    }
}

--- ./crates/adapters/dex-mock/Cargo.toml ---
[package]
name = "ben_snipes-adapter-dex-mock"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "A fake DEX adapter implementing ListingSource and ExchangeClient, simulating a venue that supports cursor-based incremental fetching (e.g. 'give me pools created after block N') to exercise that path of the diff strategy."

[dependencies]
ben_snipes-domain = { workspace = true }
ben_snipes-ports = { workspace = true }
async-trait = { workspace = true }
rust_decimal = { workspace = true }
time = { workspace = true }
tokio = { workspace = true, features = ["sync"] }

--- ./crates/adapters/cex-mock/src/lib.rs ---
//! A fake CEX. `MockCexSource` mimics an exchange that only exposes a
//! "get all tradable symbols" endpoint (no cursor support), so it always
//! returns `ListingSnapshot::Full` - exercising the diff-against-statefile
//! path in `NewListingDetector`. `MockCexClient` mimics order execution
//! with an in-memory price that never moves, so `PositionManager` logic
//! can be exercised without hitting a real market.
//!
//! Replace this with a real adapter (MEXC, Binance, etc) by implementing
//! the same two traits against that exchange's actual REST/WebSocket API.
//! Nothing outside this crate needs to change.

use async_trait::async_trait;
use ben_snipes_domain::{Listing, ListingMetrics, Order, OrderStatus, Symbol, Venue, VenueKind};
use ben_snipes_ports::{ExchangeClient, ListingSnapshot, ListingSource, MetricsProvider, PortError};
use rust_decimal::Decimal;
use std::collections::HashMap;
use time::OffsetDateTime;
use tokio::sync::Mutex;

pub struct MockCexSource {
    venue_name: String,
    symbols: Mutex<Vec<String>>,
}

impl MockCexSource {
    pub fn new(venue_name: impl Into<String>, initial_symbols: Vec<String>) -> Self {
        Self {
            venue_name: venue_name.into(),
            symbols: Mutex::new(initial_symbols),
        }
    }

    /// Simulates a brand-new listing appearing on the exchange. Useful in
    /// demos and integration tests to trigger the "new listing detected"
    /// path without waiting for a real exchange to list something.
    pub async fn simulate_new_listing(&self, symbol: impl Into<String>) {
        self.symbols.lock().await.push(symbol.into());
    }
}

#[async_trait]
impl ListingSource for MockCexSource {
    fn source_id(&self) -> &str {
        &self.venue_name
    }

    async fn poll(&self, _cursor: Option<&str>) -> Result<ListingSnapshot, PortError> {
        let venue = Venue::new(VenueKind::Cex, self.venue_name.clone())?;
        let symbols = self.symbols.lock().await;

        let listings = symbols
            .iter()
            .map(|raw| {
                let symbol = Symbol::new(raw.clone())?;
                Ok(Listing::new(symbol, venue.clone(), OffsetDateTime::now_utc()))
            })
            .collect::<Result<Vec<_>, ben_snipes_domain::DomainError>>()?;

        Ok(ListingSnapshot::Full(listings))
    }
}

pub struct MockCexClient {
    venue_name: String,
    price: Mutex<Decimal>,
    metrics: Mutex<HashMap<String, ListingMetrics>>,
}

impl MockCexClient {
    pub fn new(venue_name: impl Into<String>, starting_price: Decimal) -> Self {
        Self {
            venue_name: venue_name.into(),
            price: Mutex::new(starting_price),
            metrics: Mutex::new(HashMap::new()),
        }
    }

    /// Moves the simulated price, so tests/demos can trigger a
    /// take-profit exit deterministically.
    pub async fn set_price(&self, new_price: Decimal) {
        *self.price.lock().await = new_price;
    }

    /// Seeds volume/market-cap data for a symbol, so demos can control
    /// whether `AcquisitionCriteria` accepts or rejects it. A real
    /// adapter would pull this from the exchange's 24h ticker endpoint
    /// instead of a map you set by hand.
    pub async fn set_metrics(&self, symbol: impl Into<String>, metrics: ListingMetrics) {
        self.metrics.lock().await.insert(symbol.into(), metrics);
    }
}

#[async_trait]
impl ExchangeClient for MockCexClient {
    fn venue_name(&self) -> &str {
        &self.venue_name
    }

    async fn current_price(&self, _symbol: &Symbol) -> Result<Decimal, PortError> {
        Ok(*self.price.lock().await)
    }

    async fn submit_order(&self, mut order: Order) -> Result<Order, PortError> {
        // A real adapter would sign and send this to the exchange and
        // reflect back whatever fill status it actually gets. The mock
        // just says "filled" immediately, which is fine for exercising
        // the rest of the pipeline but obviously not for real trading.
        order.status = OrderStatus::Filled;
        Ok(order)
    }
}

#[async_trait]
impl MetricsProvider for MockCexClient {
    async fn metrics(&self, symbol: &Symbol) -> Result<Option<ListingMetrics>, PortError> {
        Ok(self.metrics.lock().await.get(symbol.as_str()).copied())
    }
}

--- ./crates/adapters/cex-mock/Cargo.toml ---
[package]
name = "ben_snipes-adapter-cex-mock"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "A fake centralised-exchange adapter that implements ListingSource and ExchangeClient with in-memory canned data. Exists so the rest of the system can be built, wired, and tested before a real exchange integration (auth, rate limits, order signing) is written."

[dependencies]
ben_snipes-domain = { workspace = true }
ben_snipes-ports = { workspace = true }
async-trait = { workspace = true }
rust_decimal = { workspace = true }
time = { workspace = true }
tokio = { workspace = true, features = ["sync"] }

--- ./crates/adapters/ws-support/src/lib.rs ---
//! A tiny shared helper: connect to a websocket URL, retrying with
//! exponential backoff on failure. Both `pumpfun` and `evm-onchain` are
//! long-running background listeners that need to survive a dropped
//! connection without the whole adapter (or the bot) going down, and
//! this is the one piece of that behaviour they'd otherwise each
//! duplicate.
//!
//! This crate deliberately does nothing else - no message parsing, no
//! protocol knowledge. That's each adapter's own concern.

use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::warn;

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Connects to `url`, retrying with exponential backoff (capped at 30s)
/// on failure. Never gives up - a background listener task is expected
/// to run for the lifetime of the process, so "stop retrying" isn't a
/// valid outcome here, only "keep trying, slower."
pub async fn connect_with_backoff(url: &str) -> WsStream {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        match connect_async(url).await {
            Ok((stream, _response)) => return stream,
            Err(e) => {
                warn!(url, error = %e, backoff_secs = backoff.as_secs(), "websocket connect failed, retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

--- ./crates/adapters/ws-support/Cargo.toml ---
[package]
name = "ben_snipes-adapter-ws-support"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "Shared reconnect-with-backoff helper for websocket-backed adapters (pumpfun, evm-onchain). Not a ListingSource itself - just the connection-management piece both real-time adapters would otherwise duplicate."

[dependencies]
tokio = { workspace = true, features = ["net"] }
tokio-tungstenite = { workspace = true }
tracing = { workspace = true }

--- ./crates/adapters/pumpfun/src/lib.rs ---
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
    Chain, DomainError, Listing, ListingMetrics, Order, SafetyReport, Symbol, Venue, VenueKind,
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

pub mod execution;
pub use execution::{execute_trade, load_wallet, TradeAction, TradeRequest};

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

/// Always-erroring `ExchangeClient` placeholder. Deliberately not a
/// "does nothing" stub - it's wired in purely to satisfy
/// `AcquisitionEngine`'s type requirements. It should be structurally
/// unreachable, because `NotYetImplementedMetrics` always returns
/// `None`, which makes `AcquisitionEngine` bail out before it ever
/// calls into an exchange client. If this ever actually gets invoked,
/// something upstream changed in a way that needs re-auditing - hence
/// the loud error rather than a silent no-op.
pub struct NotYetImplementedExchange;

#[async_trait]
impl ExchangeClient for NotYetImplementedExchange {
    fn venue_name(&self) -> &str {
        "pumpfun"
    }

    async fn current_price(&self, _symbol: &Symbol) -> Result<Decimal, PortError> {
        Err(PortError::Rejected(
            "real Solana trade execution is not yet implemented - see README".to_string(),
        ))
    }

    async fn submit_order(&self, _order: Order) -> Result<Order, PortError> {
        Err(PortError::Rejected(
            "real Solana trade execution is not yet implemented - see README".to_string(),
        ))
    }
}

--- ./crates/adapters/pumpfun/src/execution.rs ---
//! Signing and broadcast for PumpPortal's non-custodial Local
//! Transaction API (`/api/trade-local`): they build an unsigned
//! transaction, we sign it locally and broadcast it ourselves, so the
//! private key never leaves this process. Verified against PumpPortal's
//! published docs and multiple independent third-party examples at the
//! time of writing - request shape, and the fact the response is raw
//! transaction bytes rather than JSON, are both cross-confirmed.
//!
//! # The one section to re-verify before running with real funds
//!
//! `solana-sdk` went through a major breaking restructuring recently
//! (the Anza fork, v3 -> v4: `Keypair::from_bytes` was replaced by
//! `Keypair::try_from`, `Pubkey` became a type alias for a new
//! `Address` type, and the crate split into many granular sub-crates).
//! That means my working knowledge of this specific API has a real
//! chance of being stale in exactly the way that matters most here.
//!
//! Rather than reach for higher-level convenience constructors I
//! couldn't independently confirm still exist with the same shape, the
//! signing step below is built on the most fundamental, least-likely-
//! to-have-changed primitives: deserialize the raw bincode bytes into a
//! `VersionedTransaction`, sign the message bytes directly via the
//! `Signer` trait's `sign_message`, and place the resulting signature at
//! the matching index in `signatures`. Broadcast uses a raw JSON-RPC
//! `sendTransaction` call via `reqwest` rather than the `solana-client`
//! crate, specifically to avoid a second axis of API-version
//! uncertainty on top of the signing step - the JSON-RPC wire protocol
//! itself is far more stable than any one crate's Rust bindings to it.
//!
//! **Before running this against real funds:** open docs.rs for the
//! exact `solana-sdk` version pinned in this crate's `Cargo.toml` and
//! confirm `VersionedTransaction`, `VersionedMessage::static_account_keys`,
//! and `VersionedMessage::serialize` still have the shapes assumed
//! below, and that `bincode::deserialize`/`bincode::serialize` (this
//! crate pins `bincode = "1"`, the classic serde-based API) still
//! round-trip `VersionedTransaction` correctly for the current
//! solana-sdk version - if that assumption is wrong, `cargo build` will
//! fail with a clear trait-bound error rather than silently misbehave,
//! which is the safer of the two failure modes, but it does mean this
//! specific file is the most likely one to need a fix on first build.
//! This is the single highest-risk block of code in this project - it
//! moves money.

use rust_decimal::Decimal;
use solana_sdk::signer::keypair::Keypair;
use solana_sdk::signer::Signer;
use solana_sdk::transaction::VersionedTransaction;
use std::env;

const TRADE_LOCAL_URL: &str = "https://pumpportal.fun/api/trade-local";

/// Loads the wallet keypair from the `SOLANA_PRIVATE_KEY` environment
/// variable. Never reads from a file this codebase writes, never logs
/// the value (not even in error messages), and never falls back to a
/// default - there is no safe default for a private key. Expects the
/// base58-encoded 64-byte secret key format that `solana-keygen` and
/// most wallet exports use.
pub fn load_wallet() -> Result<Keypair, String> {
    let raw = env::var("SOLANA_PRIVATE_KEY")
        .map_err(|_| "SOLANA_PRIVATE_KEY environment variable is not set".to_string())?;

    let bytes = bs58::decode(raw.trim())
        .into_vec()
        .map_err(|e| format!("SOLANA_PRIVATE_KEY is not valid base58: {e}"))?;

    Keypair::try_from(bytes.as_slice())
        .map_err(|e| format!("SOLANA_PRIVATE_KEY did not decode to a valid keypair: {e}"))
}

/// A trade to submit through PumpPortal's Local Transaction API.
///
/// Note this is deliberately **not** shaped like `ExchangeClient::submit_order`
/// (which takes a token quantity) - see this crate's top-level docs for
/// why. PumpPortal's own interface is "spend this much SOL" for a buy,
/// or "sell this many tokens / this % of holdings" for a sell, and
/// forcing that into a pre-computed-quantity shape would mean either
/// fabricating a price (bonding-curve math not implemented here) or
/// silently mismatching what PumpPortal is actually asked to do.
pub struct TradeRequest {
    pub action: TradeAction,
    pub mint: String,
    /// For a buy: amount of SOL to spend, as a decimal string (e.g.
    /// "0.05"). For a sell: amount of tokens, or a percentage string
    /// like "100%" to sell the whole balance - PumpPortal accepts both
    /// shapes for `amount` on a sell.
    pub amount: String,
    pub slippage_percent: u32,
    pub priority_fee_sol: Decimal,
}

#[derive(Debug, Clone, Copy)]
pub enum TradeAction {
    Buy,
    Sell,
}

impl TradeAction {
    fn as_str(&self) -> &'static str {
        match self {
            TradeAction::Buy => "buy",
            TradeAction::Sell => "sell",
        }
    }

    /// PumpPortal's `denominatedInSol` flag: a buy's `amount` is a SOL
    /// figure, a sell's `amount` is a token figure (or percentage).
    fn denominated_in_sol(&self) -> &'static str {
        match self {
            TradeAction::Buy => "true",
            TradeAction::Sell => "false",
        }
    }
}

/// Requests, signs, and broadcasts one trade. Returns the transaction
/// signature (base58) on success.
pub async fn execute_trade(
    http: &reqwest::Client,
    wallet: &Keypair,
    rpc_url: &str,
    request: &TradeRequest,
) -> Result<String, String> {
    let body = serde_json::json!({
        "publicKey": wallet.pubkey().to_string(),
        "action": request.action.as_str(),
        "mint": request.mint,
        "denominatedInSol": request.action.denominated_in_sol(),
        // Sent as a JSON string unconditionally (covers both "0.05" and
        // "100%"). PumpPortal's own examples show amount as a bare
        // number in some places and a quoted string in others, which
        // reads as lenient/coercing parsing on their end rather than a
        // strict schema - if a trade gets rejected specifically citing
        // the amount field, that assumption is the first thing to check.
        "amount": request.amount,
        "slippage": request.slippage_percent,
        "priorityFee": request.priority_fee_sol.to_string(),
        "pool": "auto",
    });

    let response = http
        .post(TRADE_LOCAL_URL)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| format!("trade-local request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("trade-local returned {status}: {text}"));
    }

    let raw_tx_bytes = response
        .bytes()
        .await
        .map_err(|e| format!("failed to read trade-local response body: {e}"))?;

    let signed_bytes = sign_transaction(wallet, &raw_tx_bytes)?;
    broadcast(http, rpc_url, &signed_bytes).await
}

/// Deserializes PumpPortal's unsigned transaction bytes, signs the
/// message with `wallet`, and re-serializes. See this module's top
/// doc comment - this is the block to re-verify against the pinned
/// solana-sdk version's docs.rs page before trusting it with real funds.
fn sign_transaction(wallet: &Keypair, raw_tx_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut tx: VersionedTransaction = bincode::deserialize(raw_tx_bytes)
        .map_err(|e| format!("failed to deserialize transaction from trade-local: {e}"))?;

    let account_keys = tx.message.static_account_keys();
    let signer_index = account_keys
        .iter()
        .position(|key| *key == wallet.pubkey())
        .ok_or_else(|| "wallet public key not found among the transaction's required signers".to_string())?;

    let message_bytes = tx.message.serialize();
    let signature = wallet.sign_message(&message_bytes);
    tx.signatures[signer_index] = signature;

    bincode::serialize(&tx).map_err(|e| format!("failed to re-serialize signed transaction: {e}"))
}

/// Broadcasts already-signed transaction bytes via a raw JSON-RPC
/// `sendTransaction` call. Deliberately not using the `solana-client`
/// crate - see this module's top doc comment for why.
async fn broadcast(http: &reqwest::Client, rpc_url: &str, signed_bytes: &[u8]) -> Result<String, String> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(signed_bytes);

    let rpc_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendTransaction",
        "params": [encoded, { "encoding": "base64", "skipPreflight": false, "maxRetries": 3 }],
    });

    let response = http
        .post(rpc_url)
        .header("Content-Type", "application/json")
        .body(rpc_body.to_string())
        .send()
        .await
        .map_err(|e| format!("RPC sendTransaction request failed: {e}"))?;

    let response_json: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("failed to parse RPC response: {e}"))?;

    if let Some(error) = response_json.get("error") {
        return Err(format!("RPC rejected the transaction: {error}"));
    }

    response_json
        .get("result")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("RPC response had no result field: {response_json}"))
}

--- ./crates/adapters/pumpfun/Cargo.toml ---
[package]
name = "ben_snipes-adapter-pumpfun"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "Real Solana ListingSource backed by PumpPortal's free subscribeNewToken websocket feed, plus a real (but not yet ExchangeClient-wired) signing/broadcast path for their non-custodial Local Transaction API. See crate docs for the biggest caveat: solana-sdk recently went through a major breaking restructuring, so the signing code here is built on the lowest-level primitives verifiable at the time of writing, not on higher-level convenience APIs."

[dependencies]
ben_snipes-domain = { workspace = true }
ben_snipes-ports = { workspace = true }
ben_snipes-adapter-ws-support = { workspace = true }
async-trait = { workspace = true }
tokio = { workspace = true, features = ["sync", "rt"] }
tokio-tungstenite = { workspace = true }
futures-util = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
rust_decimal = { workspace = true }
time = { workspace = true }
tracing = { workspace = true }
solana-sdk = "4"
bincode = "1"
bs58 = "0.5"
base64 = "0.22"
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }

--- ./crates/adapters/evm-onchain/src/lib.rs ---
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
use ben_snipes_domain::{Chain, DomainError, Listing, ListingMetrics, Order, SafetyReport, Symbol, Venue, VenueKind};
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

--- ./crates/adapters/evm-onchain/Cargo.toml ---
[package]
name = "ben_snipes-adapter-evm-onchain"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "Real EVM ListingSource that subscribes directly to a DEX factory contract's pair/pool-creation logs over a websocket RPC (eth_subscribe), rather than polling an indexer API. Chain-, factory-, and event-agnostic - configured per chain, not hardcoded to one network."

[dependencies]
ben_snipes-domain = { workspace = true }
ben_snipes-ports = { workspace = true }
ben_snipes-adapter-ws-support = { workspace = true }
async-trait = { workspace = true }
tokio = { workspace = true, features = ["sync", "rt"] }
tokio-tungstenite = { workspace = true }
futures-util = { workspace = true }
serde_json = { workspace = true }
rust_decimal = { workspace = true }
time = { workspace = true }
tracing = { workspace = true }

--- ./bin/runner/src/main.rs ---
//! Composition root for ben_snipes. This is where concrete adapters get
//! wired into the traits `ben_snipes_application` depends on, and where
//! the actual scan -> filter -> buy -> hold -> exit loop runs.
//!
//! CEX support has been deliberately dropped from this binary - new CEX
//! listings are too rare and too slow relative to on-chain launches to
//! be worth the surface area. Detection now runs on two real sources:
//! PumpPortal (Solana, via `ben_snipes-adapter-pumpfun`) and, per
//! configured chain, a direct EVM factory-log subscription (via
//! `ben_snipes-adapter-evm-onchain`). Neither can execute real trades
//! yet - see each crate's docs and this file's `build_venues` for why
//! that's a safe default rather than an oversight. A `dex-mock` demo
//! venue is kept alongside them so `cargo run` still demonstrates the
//! full buy -> hold -> exit pipeline end to end with synthetic data.

use ben_snipes_adapter_dex_mock::{MockDexClient, MockDexSource};
use ben_snipes_adapter_evm_onchain::{
    EvmFactoryConfig, EvmFactoryLogSource, NotYetImplementedExchange as EvmNotYetImplementedExchange,
    NotYetImplementedMetrics as EvmNotYetImplementedMetrics,
};
use ben_snipes_adapter_pumpfun::{
    NotYetImplementedExchange as SolNotYetImplementedExchange,
    NotYetImplementedMetrics as SolNotYetImplementedMetrics, PumpPortalSource,
};
use ben_snipes_adapter_statefile::{FileAcquisitionLedger, StatefileStore};
use ben_snipes_application::{AcquisitionEngine, NewListingDetector, PositionManager, SafetyGate};
use ben_snipes_config::AppConfig;
use ben_snipes_domain::{
    AcquisitionCriteria, ListingMetrics, Position, ProfitTarget, SafetyCriteria, SafetyReport,
    StopLoss,
};
use ben_snipes_ports::{AcquisitionLedger, ListingSource};
use rust_decimal::Decimal;
use std::fmt::Display;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// Everything needed to watch one venue end-to-end: detect new listings
/// on it, decide whether to buy them, and later check any resulting
/// position for exit.
struct VenueHandle {
    source: Box<dyn ListingSource>,
    acquisition: AcquisitionEngine,
    position_manager: PositionManager,
}

/// Config values that violate a domain rule are a startup-time problem,
/// not a recoverable one. Prints a clear reason and exits, rather than
/// an `.expect()` that would just print a bare panic message.
fn expect_valid_config<T, E: Display>(result: Result<T, E>, what: &str) -> T {
    match result {
        Ok(value) => value,
        Err(e) => {
            eprintln!("invalid {what}: {e}");
            std::process::exit(1);
        }
    }
}

struct RiskParams {
    take_profit: ProfitTarget,
    stop_loss: StopLoss,
    criteria: AcquisitionCriteria,
    safety_criteria: SafetyCriteria,
}

async fn build_venues(
    config: &AppConfig,
    risk: &RiskParams,
    ledger: Arc<dyn AcquisitionLedger>,
) -> Vec<VenueHandle> {
    let mut venues = Vec::new();

    // --- Demo venue (synthetic data) -----------------------------------
    // Not a real detection source - kept so `cargo run` demonstrates the
    // full buy -> hold -> exit pipeline end to end without needing real
    // network access or a funded wallet. Everything below this comment
    // block is real.
    let dex_client = Arc::new(MockDexClient::new("raydium-demo", Decimal::ONE));
    let dex_source = MockDexSource::new("raydium-demo");

    dex_client
        .set_metrics(
            "NEWCOIN-SOL",
            ListingMetrics {
                volume_24h: Decimal::from(90_000),
                market_cap: Decimal::from(300_000),
            },
        )
        .await;
    dex_client
        .set_safety_report(
            "NEWCOIN-SOL",
            SafetyReport {
                sell_tax_bps: 150,
                ownership_renounced: true,
                liquidity_locked: true,
                is_mintable: false,
            },
        )
        .await;
    dex_source.simulate_new_pool("NEWCOIN-SOL").await;

    let demo_safety_gate = SafetyGate::new(dex_client.clone(), risk.safety_criteria);
    venues.push(VenueHandle {
        acquisition: AcquisitionEngine::new(
            dex_client.clone(),
            dex_client.clone(),
            ledger.clone(),
            risk.criteria,
            risk.take_profit,
            risk.stop_loss,
            config.risk.max_position_size,
            Some(demo_safety_gate),
        ),
        position_manager: PositionManager::new(dex_client.clone()),
        source: Box::new(dex_source),
    });

    // --- Solana: real detection via PumpPortal -------------------------
    let pumpfun_source = expect_valid_config(
        PumpPortalSource::spawn(config.solana.pumpportal_ws_url.clone()),
        "solana pumpportal source",
    );
    // No MetricsProvider or safety data exists for pump.fun tokens yet,
    // so this engine will never actually buy - see
    // ben_snipes-adapter-pumpfun's docs for why that's correct, not a
    // bug. It still runs the full detect -> ledger-dedup pipeline, so
    // wiring in real metrics/execution later is a drop-in swap.
    venues.push(VenueHandle {
        acquisition: AcquisitionEngine::new(
            Arc::new(SolNotYetImplementedMetrics),
            Arc::new(SolNotYetImplementedExchange),
            ledger.clone(),
            risk.criteria,
            risk.take_profit,
            risk.stop_loss,
            config.risk.max_position_size,
            None,
        ),
        position_manager: PositionManager::new(Arc::new(SolNotYetImplementedExchange)),
        source: Box::new(pumpfun_source),
    });

    // --- EVM: real detection per configured chain -----------------------
    for chain_config in &config.evm_chains {
        let factory_config = EvmFactoryConfig {
            chain_name: chain_config.chain_name.clone(),
            ws_rpc_url: chain_config.ws_rpc_url.clone(),
            factory_address: chain_config.factory_address.clone(),
            topic0: chain_config.topic0.clone(),
            base_assets: chain_config.base_assets.clone(),
        };
        let source = expect_valid_config(
            EvmFactoryLogSource::spawn(factory_config),
            &format!("evm_chains[{}] config", chain_config.chain_name),
        );

        venues.push(VenueHandle {
            acquisition: AcquisitionEngine::new(
                Arc::new(EvmNotYetImplementedMetrics),
                Arc::new(EvmNotYetImplementedExchange),
                ledger.clone(),
                risk.criteria,
                risk.take_profit,
                risk.stop_loss,
                config.risk.max_position_size,
                None,
            ),
            position_manager: PositionManager::new(Arc::new(EvmNotYetImplementedExchange)),
            source: Box::new(source),
        });
    }

    if config.evm_chains.is_empty() {
        info!("no evm_chains configured - EVM detection is inactive until config/default.toml lists at least one");
    }

    venues
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = match AppConfig::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("failed to load configuration: {e}");
            std::process::exit(1);
        }
    };

    let risk = RiskParams {
        take_profit: expect_valid_config(
            ProfitTarget::from_percent(config.risk.take_profit_percent),
            "risk.take_profit_percent",
        ),
        stop_loss: expect_valid_config(
            StopLoss::from_percent(config.risk.stop_loss_percent),
            "risk.stop_loss_percent",
        ),
        criteria: expect_valid_config(
            AcquisitionCriteria::new(config.risk.min_volume_24h),
            "risk.min_volume_24h",
        ),
        safety_criteria: SafetyCriteria::new(config.safety.max_sell_tax_bps),
    };

    info!(
        take_profit_percent = %config.risk.take_profit_percent,
        stop_loss_percent = %config.risk.stop_loss_percent,
        min_volume_24h = %config.risk.min_volume_24h,
        max_position_size = %config.risk.max_position_size,
        max_sell_tax_bps = config.safety.max_sell_tax_bps,
        evm_chains = config.evm_chains.len(),
        poll_interval_seconds = config.risk.poll_interval_seconds,
        "ben_snipes starting up"
    );

    let state_store = Arc::new(StatefileStore::new(&config.storage.state_dir));
    let detector = NewListingDetector::new(state_store);

    let ledger_path = format!("{}/acquisition-ledger.json", config.storage.state_dir);
    let ledger: Arc<dyn AcquisitionLedger> = Arc::new(expect_valid_config(
        FileAcquisitionLedger::load(&ledger_path).await,
        "acquisition ledger file",
    ));

    let venues = build_venues(&config, &risk, ledger).await;

    let mut open_positions: Vec<Position> = Vec::new();
    let mut interval = tokio::time::interval(Duration::from_secs(config.risk.poll_interval_seconds));
    let mut shutdown = std::pin::pin!(tokio::signal::ctrl_c());

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // 1. Scan every venue for newly-appeared listings and
                //    autonomously buy the ones that pass acquisition
                //    criteria, the safety gate (where configured), and
                //    the cross-source acquisition ledger.
                for venue in &venues {
                    let new_listings = match detector.poll(venue.source.as_ref()).await {
                        Ok(listings) => listings,
                        Err(e) => {
                            warn!(source = venue.source.source_id(), error = %e, "poll failed, will retry next tick");
                            continue;
                        }
                    };

                    for listing in new_listings {
                        info!(symbol = listing.symbol.as_str(), venue = %listing.venue, chain = %listing.chain, "new listing detected");

                        match venue.acquisition.evaluate_and_buy(&listing).await {
                            Ok(Some(position)) => {
                                info!(
                                    symbol = position.symbol.as_str(),
                                    entry_price = %position.entry_price,
                                    quantity = %position.quantity,
                                    "position opened, now watching for take-profit / stop-loss"
                                );
                                open_positions.push(position);
                            }
                            Ok(None) => {
                                info!(symbol = listing.symbol.as_str(), "did not qualify for acquisition, skipped");
                            }
                            Err(e) => {
                                warn!(symbol = listing.symbol.as_str(), error = %e, "acquisition attempt failed");
                            }
                        }
                    }
                }

                // 2. Check every open position against its venue's
                //    current price and exit on take-profit or stop-loss.
                let mut still_open = Vec::with_capacity(open_positions.len());
                for position in open_positions.drain(..) {
                    let venue = venues
                        .iter()
                        .find(|v| v.source.source_id() == position.venue.name());

                    let Some(venue) = venue else {
                        warn!(venue = %position.venue, "no handle found for this venue, dropping position from tracking");
                        continue;
                    };

                    match venue.position_manager.check_and_exit(&position).await {
                        Ok(Some((_filled_order, reason))) => {
                            info!(symbol = position.symbol.as_str(), reason = ?reason, "position closed");
                        }
                        Ok(None) => still_open.push(position),
                        Err(e) => {
                            warn!(symbol = position.symbol.as_str(), error = %e, "exit check failed, will retry next tick");
                            still_open.push(position);
                        }
                    }
                }
                open_positions = still_open;
            }
            _ = &mut shutdown => {
                info!(open_positions = open_positions.len(), "shutdown signal received, exiting cleanly");
                break;
            }
        }
    }
}

--- ./bin/runner/Cargo.toml ---
[package]
name = "ben_snipes-runner"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "The composition root: wires concrete adapters into the application layer's ports and runs the poll loop. This is the only crate in the workspace that's allowed to depend on every adapter at once."

[[bin]]
name = "ben_snipes"
path = "src/main.rs"

[dependencies]
ben_snipes-domain = { workspace = true }
ben_snipes-ports = { workspace = true }
ben_snipes-application = { workspace = true }
ben_snipes-config = { workspace = true }
ben_snipes-adapter-statefile = { workspace = true }
ben_snipes-adapter-dex-mock = { workspace = true }
ben_snipes-adapter-pumpfun = { workspace = true }
ben_snipes-adapter-evm-onchain = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
rust_decimal = { workspace = true }

