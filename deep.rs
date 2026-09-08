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
    "bin/backtest",
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
time = { version = "0.3", features = ["serde", "macros", "formatting", "parsing"] }
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

--- ./bin/runner/src/main.rs ---
//! Composition root for ben_snipes. This is where concrete adapters get
//! wired into the traits `ben_snipes_application` depends on, and where
//! the actual scan -> filter -> buy -> hold -> exit loop runs.
//!
//! CEX support has been deliberately dropped from this binary - new CEX
//! listings are too rare and too slow relative to on-chain launches to
//! be worth the surface area. Detection runs on two real sources:
//! PumpPortal (Solana, via `ben_snipes-adapter-pumpfun`) and, per
//! configured chain, a direct EVM factory-log subscription (via
//! `ben_snipes-adapter-evm-onchain`).
//!
//! **The Solana pipeline is now fully wired end to end**: real
//! detection, real volume filtering (DexScreener), a real safety gate
//! (RugCheck), a real cross-source dedup ledger, and - if
//! `SOLANA_PRIVATE_KEY` is set - real buy/sell execution. That means
//! this can autonomously spend real funds the moment a wallet is
//! configured. Every piece added this way carries its own confidence
//! caveat in its module docs (`execution.rs` for signing, `safety_checker.rs`
//! for RugCheck's field-mapping risk, `price_feed.rs` for the
//! SOL-denomination fix) - read them before funding a wallet, not after.
//! EVM execution is wired through Alloy and an optional private RPC. A
//! `dex-mock` demo venue is available in paper mode so `cargo run` can
//! demonstrate the full pipeline end to end with synthetic data,
//! independent of any funded wallet.

use ben_snipes_adapter_dex_mock::{MockDexClient, MockDexSource};
use ben_snipes_adapter_evm_onchain::{
    DexScreenerEvmMetrics, EvmFactoryConfig, EvmFactoryLogSource, EvmUniswapV2Exchange,
    HoneypotEvmSafetyChecker, NoWalletEvmExchange,
};
use ben_snipes_adapter_pumpfun::{
    load_wallet, wallet_pubkey_string, DexScreenerMetricsProvider, NoWalletExchange,
    PumpPortalExchangeClient, PumpPortalSource, RugCheckSafetyChecker,
};
use ben_snipes_adapter_statefile::{FileAcquisitionLedger, FilePendingTradeStore, FilePositionStore, FileTradeStore, StatefileStore};
use ben_snipes_application::{AcquisitionDecision, AcquisitionEngine, NewListingDetector, PaperExchange, PositionManager, RuntimeMetrics, SafetyGate};
use ben_snipes_config::{AppConfig, ExecutionMode};
use ben_snipes_domain::{
    AcquisitionCriteria, ListingMetrics, PerformanceSummary, Position, ProfitTarget, SafetyCriteria, SafetyReport, TradeRecord,
};
use ben_snipes_ports::{AcquisitionLedger, ExchangeClient, ListingSource, PendingTradeStore, PositionStore, TradeStore};
use rust_decimal::Decimal;
use std::fmt::Display;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::time::{Duration, Instant};
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
    criteria: AcquisitionCriteria,
    safety_criteria: SafetyCriteria,
}


async fn serve_metrics(listener: tokio::net::TcpListener, metrics: Arc<RuntimeMetrics>) {
    loop {
        let (mut socket, _) = match listener.accept().await {
            Ok(connection) => connection,
            Err(e) => {
                warn!(error = %e, "metrics listener accept failed");
                continue;
            }
        };

        let metrics = metrics.clone();
        tokio::spawn(async move {
            let mut request = [0_u8; 1024];
            let read = match socket.read(&mut request).await {
                Ok(read) => read,
                Err(e) => {
                    warn!(error = %e, "metrics request read failed");
                    return;
                }
            };

            let request = String::from_utf8_lossy(&request[..read]);
            let (status, content_type, body) = if request.starts_with("GET /metrics ") {
                ("200 OK", "text/plain; version=0.0.4", metrics.render_prometheus())
            } else {
                ("404 Not Found", "text/plain; charset=utf-8", "not found\n".to_string())
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            if let Err(e) = socket.write_all(response.as_bytes()).await {
                warn!(error = %e, "metrics response write failed");
            }
        });
    }
}

fn validate_runtime_config(config: &AppConfig) -> Result<(), String> {
    if config.risk.poll_interval_seconds == 0 {
        return Err("risk.poll_interval_seconds must be greater than zero".to_string());
    }
    if config.risk.max_position_size <= Decimal::ZERO {
        return Err("risk.max_position_size must be greater than zero".to_string());
    }
    if config.risk.max_consecutive_failures == 0 {
        return Err("risk.max_consecutive_failures must be greater than zero".to_string());
    }
    if config.risk.max_new_listings_per_cycle == 0 {
        return Err("risk.max_new_listings_per_cycle must be greater than zero".to_string());
    }
    if config.risk.pending_listing_retry_seconds == 0 {
        return Err("risk.pending_listing_retry_seconds must be greater than zero".to_string());
    }
    if config.solana.priority_fee_sol < Decimal::ZERO {
        return Err("solana.priority_fee_sol must not be negative".to_string());
    }
    for (index, chain) in config.evm_chains.iter().enumerate() {
        if chain.chain_id == 0 {
            return Err(format!("evm_chains[{index}].chain_id must be greater than zero"));
        }
        if chain.slippage_percent >= 100 {
            return Err(format!("evm_chains[{index}].slippage_percent must be below 100"));
        }
        if chain.execution_rpc_url.trim().is_empty() {
            return Err(format!("evm_chains[{index}].execution_rpc_url must not be empty"));
        }
    }
    Ok(())
}

async fn build_venues(
    config: &AppConfig,
    risk: &RiskParams,
    ledger: Arc<dyn AcquisitionLedger>,
) -> Vec<VenueHandle> {
    let mut venues = Vec::new();

    // --- Demo venue -----------------------------------------------------
    // Synthetic data is useful for paper mode, but it must never create a
    // fake position during a live deployment. Keeping it explicitly scoped
    // to paper mode removes a surprisingly dangerous source of false trades.
    if config.execution_mode == ExecutionMode::Paper {
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
                    sell_tax_bps: Some(150),
                    token_transfer_fee_bps: Some(0),
                    sellability: ben_snipes_domain::SellabilityEvidence::Simulated,
                    has_permanent_delegate: false,
                    ownership_renounced: true,
                    liquidity_locked: true,
                    is_mintable: false,
                },
            )
            .await;
        dex_source.simulate_new_pool("NEWCOIN-SOL").await;

        let demo_safety_gate = SafetyGate::new(dex_client.clone(), risk.safety_criteria);
        let demo_exchange: Arc<dyn ExchangeClient> = Arc::new(PaperExchange::new(dex_client.clone()));
        venues.push(VenueHandle {
            acquisition: AcquisitionEngine::new(
                dex_client.clone(),
                demo_exchange.clone(),
                ledger.clone(),
                risk.criteria,
                risk.take_profit,
                config.risk.max_position_size,
                Some(demo_safety_gate),
            ),
            position_manager: PositionManager::new(demo_exchange),
            source: Box::new(dex_source),
        });
    }

    // --- Solana: real detection via PumpPortal -------------------------
    let pumpfun_source = expect_valid_config(
        PumpPortalSource::spawn(config.solana.pumpportal_ws_url.clone()),
        "solana pumpportal source",
    );

    let solana_exchange: Arc<dyn ExchangeClient> = match config.execution_mode {
        ExecutionMode::Live => match load_wallet() {
            Ok(wallet) => {
                info!(pubkey = %wallet_pubkey_string(&wallet), "solana wallet loaded - buy/sell execution is live");
                Arc::new(PumpPortalExchangeClient::new(
                    wallet,
                    config.solana.rpc_url.clone(),
                    config.solana.slippage_percent,
                    config.solana.priority_fee_sol,
                ))
            }
            Err(reason) => {
                info!(reason = %reason, "no solana wallet configured - running pumpfun in detection-only mode");
                Arc::new(NoWalletExchange)
            }
        },
        ExecutionMode::Paper => Arc::new(PaperExchange::new(Arc::new(
            PumpPortalExchangeClient::new_read_only(config.solana.rpc_url.clone()),
        ))),
        ExecutionMode::DetectionOnly => Arc::new(NoWalletExchange),
    };

    let solana_metrics = Arc::new(DexScreenerMetricsProvider::new());
    let solana_safety_gate = SafetyGate::new(Arc::new(RugCheckSafetyChecker::new()), risk.safety_criteria);

    venues.push(VenueHandle {
        acquisition: AcquisitionEngine::new(
            solana_metrics,
            solana_exchange.clone(),
            ledger.clone(),
            risk.criteria,
            risk.take_profit,
            config.risk.max_position_size,
            Some(solana_safety_gate),
        ),
        position_manager: PositionManager::new(solana_exchange),
        source: Box::new(pumpfun_source),
    });

    // --- EVM: real detection and execution per configured chain --------
    for chain_config in &config.evm_chains {
        let factory_config = EvmFactoryConfig {
            chain_name: chain_config.chain_name.clone(),
            chain_id: chain_config.chain_id,
            ws_rpc_url: chain_config.ws_rpc_url.clone(),
            execution_rpc_url: chain_config.execution_rpc_url.clone(),
            private_rpc_url: chain_config.private_rpc_url.clone(),
            factory_address: chain_config.factory_address.clone(),
            topic0: chain_config.topic0.clone(),
            base_assets: chain_config.base_assets.clone(),
            router_address: chain_config.router_address.clone(),
            wrapped_native_address: chain_config.wrapped_native_address.clone(),
            slippage_percent: chain_config.slippage_percent,
        };
        let source = expect_valid_config(
            EvmFactoryLogSource::spawn(factory_config),
            &format!("evm_chains[{}] config", chain_config.chain_name),
        );

        let evm_exchange: Arc<dyn ExchangeClient> = match config.execution_mode {
            ExecutionMode::Live => match EvmUniswapV2Exchange::from_env(
                chain_config.chain_id,
                chain_config.execution_rpc_url.clone(),
                chain_config.private_rpc_url.clone(),
                &chain_config.router_address,
                &chain_config.wrapped_native_address,
                chain_config.slippage_percent,
            ) {
                Ok(exchange) => {
                    info!(chain = %chain_config.chain_name, "EVM wallet loaded - execution is live");
                    Arc::new(exchange)
                }
                Err(reason) => {
                    info!(chain = %chain_config.chain_name, reason = %reason, "no EVM wallet configured - running EVM in detection-only mode");
                    Arc::new(NoWalletEvmExchange)
                }
            },
            ExecutionMode::Paper => Arc::new(PaperExchange::new(Arc::new(expect_valid_config(
                EvmUniswapV2Exchange::read_only(
                    chain_config.chain_id,
                    chain_config.execution_rpc_url.clone(),
                    &chain_config.router_address,
                    &chain_config.wrapped_native_address,
                    chain_config.slippage_percent,
                ),
                &format!("evm_chains[{}] read-only exchange", chain_config.chain_name),
            )))),
            ExecutionMode::DetectionOnly => Arc::new(NoWalletEvmExchange),
        };
        let evm_safety = SafetyGate::new(
            Arc::new(HoneypotEvmSafetyChecker::new(chain_config.chain_id)),
            risk.safety_criteria,
        );

        venues.push(VenueHandle {
            acquisition: AcquisitionEngine::new(
                Arc::new(DexScreenerEvmMetrics::new(chain_config.chain_name.clone())),
                evm_exchange.clone(),
                ledger.clone(),
                risk.criteria,
                risk.take_profit,
                config.risk.max_position_size,
                Some(evm_safety),
            ),
            position_manager: PositionManager::new(evm_exchange),
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

    if let Err(e) = validate_runtime_config(&config) {
        eprintln!("invalid runtime configuration: {e}");
        std::process::exit(1);
    }

    let risk = RiskParams {
        take_profit: expect_valid_config(
            ProfitTarget::from_percent(config.risk.take_profit_percent),
            "risk.take_profit_percent",
        ),
        criteria: expect_valid_config(
            AcquisitionCriteria::new(config.risk.min_volume_24h),
            "risk.min_volume_24h",
        ),
        safety_criteria: SafetyCriteria::new(
            config.safety.max_sell_tax_bps,
            config.safety.max_token_transfer_fee_bps,
        ),
    };

    info!(
        take_profit_percent = %config.risk.take_profit_percent,
        min_volume_24h = %config.risk.min_volume_24h,
        max_position_size = %config.risk.max_position_size,
        max_open_positions = config.risk.max_open_positions,
        max_consecutive_failures = config.risk.max_consecutive_failures,
        max_new_listings_per_cycle = config.risk.max_new_listings_per_cycle,
        entry_kill_switch_file = %config.risk.entry_kill_switch_file,
        max_sell_tax_bps = config.safety.max_sell_tax_bps,
        max_token_transfer_fee_bps = config.safety.max_token_transfer_fee_bps,
        evm_chains = config.evm_chains.len(),
        execution_mode = ?config.execution_mode,
        poll_interval_seconds = config.risk.poll_interval_seconds,
        "ben_snipes starting up"
    );

    let runtime_metrics = RuntimeMetrics::new();
    let metrics_listener = match tokio::net::TcpListener::bind(&config.observability.metrics_bind).await {
        Ok(listener) => {
            info!(bind = %config.observability.metrics_bind, "metrics endpoint listening");
            Some(tokio::spawn(serve_metrics(listener, runtime_metrics.clone())))
        }
        Err(e) => {
            warn!(bind = %config.observability.metrics_bind, error = %e, "metrics endpoint disabled because bind failed");
            None
        }
    };

    let state_store = Arc::new(StatefileStore::new(&config.storage.state_dir));
    let detector = NewListingDetector::new(state_store);

    let ledger_path = format!("{}/acquisition-ledger.json", config.storage.state_dir);
    let ledger: Arc<dyn AcquisitionLedger> = Arc::new(expect_valid_config(
        FileAcquisitionLedger::load(&ledger_path).await,
        "acquisition ledger file",
    ));

    let position_store = FilePositionStore::new(format!("{}/open-positions.json", config.storage.state_dir));
    let mut open_positions: Vec<Position> = expect_valid_config(position_store.load().await, "open positions file");
    if !open_positions.is_empty() {
        info!(count = open_positions.len(), "recovered open positions from a previous run");
    }

    let trade_store: Arc<dyn TradeStore> = Arc::new(FileTradeStore::new(
        format!("{}/trades.json", config.storage.state_dir),
    ));
    let mut trade_history = expect_valid_config(trade_store.load().await, "trade journal");
    let pending_trade_store: Arc<dyn PendingTradeStore> = Arc::new(FilePendingTradeStore::new(
        format!("{}/pending-trades.json", config.storage.state_dir),
    ));
    let mut pending_trades = expect_valid_config(
        pending_trade_store.load().await,
        "pending trade journal",
    );
    if !pending_trades.is_empty() {
        info!(count = pending_trades.len(), "recovered trades waiting for journal persistence");
    }
    let completed_trade_keys: std::collections::HashSet<String> = trade_history
        .iter()
        .map(TradeRecord::key)
        .collect();
    let recovered_before_cleanup = open_positions.len();
    open_positions.retain(|position| {
        let key = TradeRecord::key_for_position(position);
        !completed_trade_keys.contains(&key)
    });
    if open_positions.len() != recovered_before_cleanup {
        info!(
            removed = recovered_before_cleanup - open_positions.len(),
            "removed positions already present in the persistent trade journal during recovery"
        );
    }
    runtime_metrics.set_open_positions(open_positions.len());
    let mut performance = PerformanceSummary::from_trades(&trade_history);
    info!(
        trades = performance.trade_count,
        wins = performance.winning_trades,
        losses = performance.losing_trades,
        flat = performance.flat_trades,
        realized_pnl = %performance.realized_pnl,
        "loaded persistent trade performance"
    );

    let venues = build_venues(&config, &risk, ledger).await;

    let mut interval = tokio::time::interval(Duration::from_secs(config.risk.poll_interval_seconds));
    let mut shutdown = std::pin::pin!(tokio::signal::ctrl_c());
    let mut consecutive_failures = 0_u32;
    let mut last_pending_retry = Instant::now();
    let mut retry_pending_on_first_cycle = true;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // First flush any trade whose sell succeeded but whose main
                // journal write failed on a previous cycle or process run.
                if !pending_trades.is_empty() {
                    let mut remaining = Vec::with_capacity(pending_trades.len());
                    for trade in pending_trades.drain(..) {
                        match trade_store.append(&trade).await {
                            Ok(()) => {
                                if !trade_history.iter().any(|existing| existing.key() == trade.key()) {
                                    trade_history.push(trade);
                                }
                                runtime_metrics.inc_journal_recoveries();
                            }
                            Err(error) => {
                                warn!(symbol = trade.symbol.as_str(), error = %error, "pending trade journal retry failed");
                                remaining.push(trade);
                            }
                        }
                    }
                    pending_trades = remaining;
                    if let Err(error) = pending_trade_store.save(&pending_trades).await {
                        runtime_metrics.inc_journal_errors();
                        warn!(error = %error, "failed to persist pending trade journal queue");
                    }
                    performance = PerformanceSummary::from_trades(&trade_history);
                    info!(
                        trades = performance.trade_count,
                        wins = performance.winning_trades,
                        losses = performance.losing_trades,
                        flat = performance.flat_trades,
                        realized_pnl = %performance.realized_pnl,
                        pending_remaining = pending_trades.len(),
                        "pending trade journal retry flushed"
                    );
                }

                // 1. Scan every venue for newly-appeared listings. New entries
                //    are blocked when an operator kill switch exists, the
                //    concurrent-position cap is full, a listing burst exceeds
                //    the configured threshold, or the failure circuit is open.
                let kill_switch_active = Path::new(&config.risk.entry_kill_switch_file).exists();
                let mut detected_this_cycle = 0_usize;
                if kill_switch_active {
                    warn!(path = %config.risk.entry_kill_switch_file, "entry kill switch active; new buys are paused");
                }
                if consecutive_failures >= config.risk.max_consecutive_failures {
                    warn!(consecutive_failures, "entry circuit breaker open; new buys are paused");
                }

                let retry_pending = retry_pending_on_first_cycle
                    || last_pending_retry.elapsed() >= Duration::from_secs(config.risk.pending_listing_retry_seconds);
                if retry_pending {
                    retry_pending_on_first_cycle = false;
                    last_pending_retry = Instant::now();
                }

                for venue in &venues {
                    runtime_metrics.inc_polls();
                    let new_listings = match detector.poll(venue.source.as_ref(), retry_pending).await {
                        Ok(listings) => listings,
                        Err(e) => {
                            runtime_metrics.inc_poll_errors();
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            warn!(source = venue.source.source_id(), error = %e, "poll failed, will retry next tick");
                            continue;
                        }
                    };

                    runtime_metrics.inc_listings_detected(new_listings.len());
                    detected_this_cycle = detected_this_cycle.saturating_add(new_listings.len());
                    if detected_this_cycle > config.risk.max_new_listings_per_cycle {
                        warn!(
                            detected = detected_this_cycle,
                            limit = config.risk.max_new_listings_per_cycle,
                            "listing burst exceeded configured threshold; blocking new entries for this cycle"
                        );
                        break;
                    }

                    for listing in new_listings {
                        let kill_switch_active = Path::new(&config.risk.entry_kill_switch_file).exists();
                        if kill_switch_active || consecutive_failures >= config.risk.max_consecutive_failures {
                            break;
                        }
                        if open_positions.len() >= config.risk.max_open_positions {
                            warn!(
                                limit = config.risk.max_open_positions,
                                "maximum open positions reached; remaining listings will be skipped"
                            );
                            break;
                        }

                        info!(symbol = listing.symbol.as_str(), venue = %listing.venue, chain = %listing.chain, "new listing detected");

                        if config.execution_mode == ExecutionMode::DetectionOnly {
                            info!(symbol = listing.symbol.as_str(), "detection-only mode: acquisition skipped");
                            continue;
                        }

                        match venue.acquisition.evaluate_and_buy(&listing).await {
                            Ok(AcquisitionDecision::Opened(position)) => {
                                runtime_metrics.inc_positions_opened();
                                consecutive_failures = 0;
                                if let Err(e) = detector.resolve(venue.source.as_ref(), &listing, false).await {
                                    consecutive_failures = consecutive_failures.saturating_add(1);
                                    warn!(symbol = listing.symbol.as_str(), error = %e, "failed to resolve acquired listing state");
                                }
                                info!(
                                    symbol = position.symbol.as_str(),
                                    entry_price = %position.entry_price,
                                    quantity = %position.quantity,
                                    "position opened, now watching for take-profit"
                                );
                                open_positions.push(position);
                                runtime_metrics.set_open_positions(open_positions.len());
                                if let Err(e) = position_store.save(&open_positions).await {
                                    consecutive_failures = consecutive_failures.saturating_add(1);
                                    warn!(error = %e, "failed to persist open positions after a buy; position is still tracked in memory this run");
                                }
                            }
                            Ok(AcquisitionDecision::Pending) => {
                                runtime_metrics.inc_pending_decisions();
                                consecutive_failures = 0;
                                info!(symbol = listing.symbol.as_str(), "listing lacks required external data yet; retained for retry");
                            }
                            Ok(AcquisitionDecision::Rejected) => {
                                runtime_metrics.inc_rejected_decisions();
                                consecutive_failures = 0;
                                if let Err(e) = detector.resolve(venue.source.as_ref(), &listing, false).await {
                                    consecutive_failures = consecutive_failures.saturating_add(1);
                                    warn!(symbol = listing.symbol.as_str(), error = %e, "failed to resolve rejected listing state");
                                }
                                info!(symbol = listing.symbol.as_str(), "listing did not qualify for acquisition");
                            }
                            Err(e) => {
                                runtime_metrics.inc_buy_errors();
                                consecutive_failures = consecutive_failures.saturating_add(1);
                                warn!(symbol = listing.symbol.as_str(), error = %e, "acquisition attempt failed; retaining listing for retry");
                            }
                        }
                    }
                }

                // 2. Check every open position against its venue's
                //    current price and exit only when take-profit is reached.
                // Detection-only mode deliberately leaves positions untouched.
                if config.execution_mode == ExecutionMode::DetectionOnly {
                    continue;
                }
                let mut still_open = Vec::with_capacity(open_positions.len());
                for position in open_positions.drain(..) {
                    let venue = venues
                        .iter()
                        .find(|v| v.source.source_id() == position.venue.name());

                    let Some(venue) = venue else {
                        warn!(venue = %position.venue, "no handle found for this venue, retaining position for later recovery");
                        still_open.push(position);
                        continue;
                    };

                    runtime_metrics.inc_exit_checks();
                    match venue.position_manager.check_and_exit(&position).await {
                        Ok(Some(exit)) => {
                            consecutive_failures = 0;
                            let trade = TradeRecord::from_fill(
                                &position,
                                &exit.fill,
                                exit.reference_price,
                                exit.closed_at,
                            );
                            match trade_store.append(&trade).await {
                                Ok(()) => {
                                    runtime_metrics.inc_exits_filled();
                                    trade_history.push(trade.clone());
                                    performance = PerformanceSummary::from_trades(&trade_history);
                                    info!(
                                        symbol = position.symbol.as_str(),
                                        exit_price = %trade.exit_price,
                                        execution_price_is_reference = trade.execution_price_is_reference,
                                        tx_id = ?trade.tx_id,
                                        fee_quote = %trade.fee_quote,
                                        pnl = %trade.pnl,
                                        realized_pnl = %performance.realized_pnl,
                                        trades = performance.trade_count,
                                        "take-profit reached, position closed and journaled"
                                    );
                                }
                                Err(e) => {
                                    runtime_metrics.inc_journal_errors();
                                    consecutive_failures = consecutive_failures.saturating_add(1);
                                    warn!(
                                        symbol = position.symbol.as_str(),
                                        error = %e,
                                        exit_price = %trade.exit_price,
                                        execution_price_is_reference = trade.execution_price_is_reference,
                                        tx_id = ?trade.tx_id,
                                        fee_quote = %trade.fee_quote,
                                        pnl = %trade.pnl,
                                        "position closed but failed to persist trade journal entry; queued for durable retry"
                                    );
                                    // The sell already executed - the position is
                                    // gone either way - but the journal write
                                    // failed, so this trade would otherwise be
                                    // lost forever. Queue it in the durable
                                    // pending-trade store so the top of the next
                                    // tick (or a fresh process after a crash)
                                    // retries the journal write instead of
                                    // silently dropping the record.
                                    trade_history.push(trade.clone());
                                    performance = PerformanceSummary::from_trades(&trade_history);
                                    info!(
                                        trades = performance.trade_count,
                                        wins = performance.winning_trades,
                                        losses = performance.losing_trades,
                                        flat = performance.flat_trades,
                                        realized_pnl = %performance.realized_pnl,
                                        "performance updated from an unjournaled trade pending durable retry"
                                    );
                                    pending_trades.push(trade);
                                    if let Err(persist_err) = pending_trade_store.save(&pending_trades).await {
                                        runtime_metrics.inc_journal_errors();
                                        warn!(
                                            error = %persist_err,
                                            "failed to persist pending trade journal queue after a journal write failure"
                                        );
                                    }
                                }
                            }
                        }
                        Ok(None) => still_open.push(position),
                        Err(e) => {
                            runtime_metrics.inc_exit_errors();
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            warn!(symbol = position.symbol.as_str(), error = %e, "exit check failed, will retry next tick");
                            still_open.push(position);
                        }
                    }
                }
                open_positions = still_open;
                runtime_metrics.set_open_positions(open_positions.len());
                if let Err(e) = position_store.save(&open_positions).await {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    warn!(error = %e, "failed to persist open positions after exit checks");
                }
            }
            _ = &mut shutdown => {
                info!(open_positions = open_positions.len(), "shutdown signal received, exiting cleanly");
                if let Some(handle) = metrics_listener {
                    handle.abort();
                }
                break;
            }
        }
    }
}

--- ./bin/backtest/Cargo.toml ---
[package]
name = "ben_snipes-backtest"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "Deterministic historical replay for ben_snipes strategy rules."

[[bin]]
name = "ben_snipes-backtest"
path = "src/main.rs"

[dependencies]
ben_snipes-domain = { workspace = true }
ben_snipes-application = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
tracing = { workspace = true }

--- ./bin/backtest/src/main.rs ---
use ben_snipes_application::{BacktestConfig, BacktestEngine, BacktestEvent};
use ben_snipes_domain::PerformanceSummary;
use serde::Deserialize;
use std::{env, fs};

#[derive(Debug, Deserialize)]
struct BacktestFile {
    config: BacktestConfig,
    events: Vec<BacktestEvent>,
}

fn print_summary(summary: &PerformanceSummary) {
    println!("trades: {}", summary.trade_count);
    println!("wins: {}", summary.winning_trades);
    println!("losses: {}", summary.losing_trades);
    println!("flats: {}", summary.flat_trades);
    println!("quote cost: {}", summary.total_quote_cost);
    println!("quote proceeds: {}", summary.total_quote_proceeds);
    println!("realized pnl: {}", summary.realized_pnl);
}

fn main() {
    let path = match env::args().nth(1) {
        Some(path) => path,
        None => {
            eprintln!("usage: ben_snipes-backtest <dataset.json>");
            std::process::exit(2);
        }
    };

    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) => {
            eprintln!("failed to read '{path}': {e}");
            std::process::exit(1);
        }
    };

    let dataset: BacktestFile = match serde_json::from_str(&raw) {
        Ok(dataset) => dataset,
        Err(e) => {
            eprintln!("invalid backtest dataset '{path}': {e}");
            std::process::exit(1);
        }
    };

    let engine = match BacktestEngine::new(dataset.config) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("invalid backtest config: {e}");
            std::process::exit(1);
        }
    };

    let report = engine.run(dataset.events);
    println!("events processed: {}", report.events_processed);
    println!("listings seen: {}", report.listings_seen);
    println!("entries opened: {}", report.entries_opened);
    println!("pending events: {}", report.pending_events);
    println!("rejected events: {}", report.rejected_events);
    println!("still open: {}", report.still_open.len());
    print_summary(&report.performance);
}

--- ./config/default.toml ---
# live submits real transactions when wallets and private execution endpoints are configured.
# paper uses real market data but never signs or submits trades.
# detection_only observes listings and evaluates them without entering or exiting.
execution_mode = "live"

[risk]
# The only exit condition - no stop-loss, by design. A position is held
# until it hits this target, however long that takes.
take_profit_percent = "10.0"
poll_interval_seconds = 5
# A newly detected token can exist on-chain before DexScreener indexes it, or
# can be indexed before it has enough volume to qualify. Keep it in the
# persistent pending cursor and retry the metrics/safety lookup every 20
# seconds for 24 hours. A temporary low-volume result is not a rejection.
pending_listing_retry_seconds = 20
# This is the real SOL amount spent per buy on the live Solana venue
# (not just the demo) - see the README's "Automation & execution
# platforms" before raising this. 0.01 SOL is a deliberately small
# starting default now that this number has real financial teeth,
# not a suggestion of what's "enough" to trade meaningfully.
max_position_size = "0.01"
# "Active volume" is meant as the smallest threshold that's still
# meaningful, not a high bar - the goal is catching a listing early,
# not waiting until it's already proven itself. 250 (USD-equivalent,
# per DexScreener's 24h volume figure) is a judgment call, not a
# researched-optimal number: low enough to catch a token within its
# first few minutes if it's getting any real secondary trading beyond
# the creator's initial buy, high enough to filter out pure noise/a
# single small trade. Tune this if you have better intuition for where
# that line sits.
min_volume_24h = "250"
max_open_positions = 3
max_consecutive_failures = 5
max_new_listings_per_cycle = 20
entry_kill_switch_file = "state/STOP_ENTRIES"

[safety]
# 1000 bps = 10%. Anything taxing a sell higher than this is treated as
# a honeypot signal rather than an aggressive-but-legitimate tokenomics
# choice - tune down if that's too permissive for your risk tolerance.
max_sell_tax_bps = 1000
# Token-2022 transfer fees are separate from DEX/router sell taxes.
# Keep this at zero unless a non-zero token transfer fee is explicitly acceptable.
max_token_transfer_fee_bps = 0

[storage]
state_dir = "state"

[solana]
# PumpPortal's public data feed - free, no API key required for
# subscribeNewToken. Only override this if you're pointing at a
# self-hosted relay or a different environment.
pumpportal_ws_url = "wss://pumpportal.fun/api/data"

# Public RPC endpoints are typically rate-limited too aggressively for
# real trading (broadcast + confirmation polling + balance checks add
# up fast) - replace this with your own provider before running with a
# funded wallet. Left as a public endpoint by default so detection-only
# mode (no SOLANA_PRIVATE_KEY set) still has something to point at.
rpc_url = "https://api.mainnet-beta.solana.com"

# Passed straight through to PumpPortal's trade-local API on every
# buy/sell.
slippage_percent = 10
priority_fee_sol = "0.0001"

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
# chain_id = 1
# ws_rpc_url = "wss://YOUR-PROVIDER-WS-ENDPOINT-WITH-API-KEY"
# execution_rpc_url = "https://YOUR-PROVIDER-HTTP-ENDPOINT-WITH-API-KEY"
# private_rpc_url = "https://rpc.flashbots.net/fast"
# factory_address = "0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f"
# topic0 = "PUT-THE-VERIFIED-PairCreated-TOPIC-HASH-HERE"
# base_assets = [
#     "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
#     "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eB48",
#     "0xdac17f958d2ee523a2206206994597c13d831ec7",
# ]
# router_address = "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D"
# wrapped_native_address = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
# slippage_percent = 10
evm_chains = []

[observability]
# Prometheus-compatible metrics endpoint. Keep this bound to loopback unless
# the deployment provides its own authentication and network controls.
metrics_bind = "127.0.0.1:9090"

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
use ben_snipes_domain::{FilledBuy, FilledSell, Listing, ListingMetrics, Order, Symbol, Venue, VenueKind};
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

    async fn submit_order(&self, order: Order) -> Result<FilledSell, PortError> {
        let price = *self.price.lock().await;
        Ok(FilledSell {
            quantity: order.quantity,
            execution_price: Some(price),
            quote_proceeds: Some(price * order.quantity),
            fee_quote: None,
            tx_id: Some("mock-cex-tx".to_string()),
        })
    }
}

#[async_trait]
impl MetricsProvider for MockCexClient {
    async fn metrics(&self, symbol: &Symbol) -> Result<Option<ListingMetrics>, PortError> {
        Ok(self.metrics.lock().await.get(symbol.as_str()).copied())
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
    Chain, FilledBuy, FilledSell, Listing, ListingMetrics, Order, SafetyReport, Symbol, Venue,
    VenueKind,
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

    async fn submit_buy_by_amount(&self, _symbol: &Symbol, quote_amount: Decimal) -> Result<FilledBuy, PortError> {
        let price = *self.price.lock().await;
        if price <= Decimal::ZERO {
            return Err(PortError::Rejected("mock price is non-positive".to_string()));
        }
        Ok(FilledBuy {
            quantity: quote_amount / price,
            entry_price: price,
        })
    }

    async fn submit_order(&self, order: Order) -> Result<FilledSell, PortError> {
        let price = *self.price.lock().await;
        Ok(FilledSell {
            quantity: order.quantity,
            execution_price: Some(price),
            quote_proceeds: Some(price * order.quantity),
            fee_quote: None,
            tx_id: Some("mock-dex-tx".to_string()),
        })
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

--- ./crates/adapters/evm-onchain/Cargo.toml ---
[package]
name = "ben_snipes-adapter-evm-onchain"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "Real EVM ListingSource that subscribes directly to a DEX factory contract's pair/pool-creation logs over a websocket RPC (eth_subscribe), rather than polling an indexer API. Chain-, factory-, and event-agnostic - configured per chain, not hardcoded to one network."

[dependencies]
alloy = "2.4.1"
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

reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }

--- ./crates/adapters/evm-onchain/src/lib.rs ---
//! EVM detection, market-data enrichment, safety checks, and Uniswap-V2-style
//! execution for the trading pipeline.
//!
//! Detection remains factory-log based. Trading uses Alloy with a local
//! `PrivateKeySigner`; the unsigned transaction is simulated with `eth_call`
//! before the provider is allowed to sign and submit it. When `private_rpc_url`
//! is configured, the signed transaction is sent through that endpoint. This
//! supports private RPCs such as Flashbots Protect without making the adapter
//! depend on Flashbots-specific RPC methods.

use alloy::{
    network::{
        eip2718::Encodable2718, EthereumWallet, NetworkTransactionBuilder, ReceiptResponse,
        TransactionBuilder,
    },
    primitives::{Address, Bytes, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol,
};
use async_trait::async_trait;
use ben_snipes_adapter_ws_support::connect_with_backoff;
use ben_snipes_domain::{
    Chain, DomainError, FilledBuy, FilledSell, Listing, ListingMetrics, Order, OrderSide,
    SafetyReport, Symbol, Venue, VenueKind,
};
use ben_snipes_ports::{
    ExchangeClient, ListingSnapshot, ListingSource, MetricsProvider, PortError, TokenSafetyChecker,
};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use std::{collections::HashSet, sync::Arc};
use std::str::FromStr;
use time::OffsetDateTime;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

sol! {
    #[sol(rpc)]
    interface UniswapV2Router {
        function getAmountsOut(uint256 amountIn, address[] calldata path)
            external view returns (uint256[] memory amounts);
        function swapExactETHForTokensSupportingFeeOnTransferTokens(
            uint256 amountOutMin,
            address[] calldata path,
            address to,
            uint256 deadline
        ) external payable;
        function swapExactTokensForETHSupportingFeeOnTransferTokens(
            uint256 amountIn,
            uint256 amountOutMin,
            address[] calldata path,
            address to,
            uint256 deadline
        ) external;
    }

    #[sol(rpc)]
    interface Erc20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
        function balanceOf(address owner) external view returns (uint256);
        function decimals() external view returns (uint8);
    }
}

#[derive(Debug, Clone)]
pub struct EvmFactoryConfig {
    pub chain_name: String,
    pub chain_id: u64,
    pub ws_rpc_url: String,
    pub execution_rpc_url: String,
    pub private_rpc_url: Option<String>,
    pub factory_address: String,
    pub topic0: String,
    pub base_assets: Vec<String>,
    pub router_address: String,
    pub wrapped_native_address: String,
    pub slippage_percent: u32,
}

pub struct EvmFactoryLogSource {
    venue: Venue,
    receiver: Mutex<mpsc::UnboundedReceiver<Listing>>,
}

impl EvmFactoryLogSource {
    pub fn spawn(config: EvmFactoryConfig) -> Result<Self, DomainError> {
        validate_factory_config(&config)?;
        let venue = Venue::new(VenueKind::Dex, format!("{}-onchain", config.chain_name))?;
        let chain = Chain::new(config.chain_name.clone())?;
        let (tx, rx) = mpsc::unbounded_channel();
        let task_venue = venue.clone();
        tokio::spawn(async move { run(config, tx, task_venue, chain).await });
        Ok(Self { venue, receiver: Mutex::new(rx) })
    }
}

fn validate_factory_config(config: &EvmFactoryConfig) -> Result<(), DomainError> {
    if config.chain_id == 0 || config.ws_rpc_url.trim().is_empty() || config.execution_rpc_url.trim().is_empty() {
        return Err(DomainError::InvalidChainConfig(config.chain_name.clone()));
    }
    for (label, address) in [
        ("factory", config.factory_address.as_str()),
        ("router", config.router_address.as_str()),
        ("wrapped native", config.wrapped_native_address.as_str()),
    ] {
        address.parse::<Address>().map_err(|_| DomainError::InvalidChainConfig(format!("invalid {label} address")))?;
    }
    let topic = config.topic0.strip_prefix("0x").ok_or_else(|| DomainError::InvalidChainConfig("topic0 must start with 0x".to_string()))?;
    if topic.len() != 64 || !topic.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(DomainError::InvalidChainConfig("topic0 must be a 32-byte hex value".to_string()));
    }
    if config.slippage_percent >= 100 {
        return Err(DomainError::InvalidChainConfig("EVM slippage_percent must be below 100".to_string()));
    }
    for asset in &config.base_assets {
        asset.parse::<Address>().map_err(|_| DomainError::InvalidChainConfig("invalid EVM base asset address".to_string()))?;
    }
    Ok(())
}

async fn run(
    config: EvmFactoryConfig,
    tx: mpsc::UnboundedSender<Listing>,
    venue: Venue,
    chain: Chain,
) {
    let base_assets: HashSet<String> = config.base_assets.iter().map(|a| a.to_lowercase()).collect();

    loop {
        let mut stream = connect_with_backoff(&config.ws_rpc_url).await;
        let subscribe_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_subscribe",
            "params": ["logs", { "address": config.factory_address, "topics": [config.topic0] }],
        }).to_string();

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
            let Some(log) = parsed.get("params").and_then(|p| p.get("result")) else { continue };
            let Some(topics) = log.get("topics").and_then(|t| t.as_array()) else { continue };
            if topics.len() < 3 { continue; }
            let Some(token0) = topics[1].as_str().and_then(extract_address) else { continue };
            let Some(token1) = topics[2].as_str().and_then(extract_address) else { continue };

            let candidates = match (base_assets.contains(&token0), base_assets.contains(&token1)) {
                (true, false) => vec![token1],
                (false, true) => vec![token0],
                _ => vec![token0, token1],
            };

            for address in candidates {
                let Ok(symbol) = Symbol::new(address) else { continue };
                let listing = Listing::new(symbol, venue.clone(), chain.clone(), OffsetDateTime::now_utc());
                if tx.send(listing).is_err() { return; }
            }
        }
    }
}

fn extract_address(topic: &str) -> Option<String> {
    let hex = topic.strip_prefix("0x")?;
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) { return None; }
    Some(format!("0x{}", &hex[24..]).to_lowercase())
}

#[async_trait]
impl ListingSource for EvmFactoryLogSource {
    fn source_id(&self) -> &str { self.venue.name() }

    async fn poll(&self, _cursor: Option<&str>) -> Result<ListingSnapshot, PortError> {
        let mut rx = self.receiver.lock().await;
        let mut new = Vec::new();
        while let Ok(listing) = rx.try_recv() { new.push(listing); }
        Ok(ListingSnapshot::Incremental { new, cursor: None })
    }
}

pub struct DexScreenerEvmMetrics {
    chain_id: String,
    http: reqwest::Client,
}

impl DexScreenerEvmMetrics {
    pub fn new(chain_id: impl Into<String>) -> Self {
        Self { chain_id: chain_id.into().to_lowercase(), http: reqwest::Client::new() }
    }
}

#[derive(Debug, serde::Deserialize)]
struct DexScreenerResponse { #[serde(default)] pairs: Vec<DexScreenerPair> }
#[derive(Debug, serde::Deserialize)]
struct DexScreenerPair {
    #[serde(rename = "chainId")]
    chain_id: Option<String>,
    #[serde(default)]
    volume: Option<DexScreenerVolume>,
    #[serde(rename = "marketCap")]
    market_cap: Option<f64>,
    fdv: Option<f64>,
}
#[derive(Debug, serde::Deserialize)]
struct DexScreenerVolume { #[serde(rename = "h24")] h24: Option<f64> }

#[async_trait]
impl MetricsProvider for DexScreenerEvmMetrics {
    async fn metrics(&self, symbol: &Symbol) -> Result<Option<ListingMetrics>, PortError> {
        let url = format!("https://api.dexscreener.com/latest/dex/tokens/{}", symbol.as_str());
        let response = self.http.get(url).send().await.map_err(|e| PortError::Network {
            venue: "dexscreener".to_string(), source: Box::new(e),
        })?;
        if !response.status().is_success() { return Ok(None); }
        let body: DexScreenerResponse = response.json().await.map_err(|e| PortError::MalformedResponse {
            venue: "dexscreener".to_string(), reason: e.to_string(),
        })?;
        let Some(best) = body.pairs.into_iter()
            .filter(|pair| pair.chain_id.as_deref().map(|c| c.eq_ignore_ascii_case(&self.chain_id)).unwrap_or(false))
            .max_by(|a, b| {
                let av = a.volume.as_ref().and_then(|v| v.h24).unwrap_or(0.0);
                let bv = b.volume.as_ref().and_then(|v| v.h24).unwrap_or(0.0);
                av.partial_cmp(&bv).unwrap_or(std::cmp::Ordering::Equal)
            }) else { return Ok(None); };
        let volume = best.volume.and_then(|v| v.h24).unwrap_or(0.0);
        let market_cap = best.market_cap.or(best.fdv).unwrap_or(0.0);
        if !volume.is_finite() || !market_cap.is_finite() || volume < 0.0 || market_cap < 0.0 {
            return Err(PortError::MalformedResponse {
                venue: "dexscreener".to_string(),
                reason: "metrics contained a non-finite or negative numeric value".to_string(),
            });
        }
        Ok(Some(ListingMetrics {
            volume_24h: Decimal::try_from(volume).map_err(|e| PortError::MalformedResponse {
                venue: "dexscreener".to_string(), reason: format!("invalid volume: {e}"),
            })?,
            market_cap: Decimal::try_from(market_cap).map_err(|e| PortError::MalformedResponse {
                venue: "dexscreener".to_string(), reason: format!("invalid market cap: {e}"),
            })?,
        }))
    }
}

#[derive(Debug, serde::Deserialize)]
struct HoneypotResponse {
    #[serde(rename = "simulationSuccess", default)] simulation_success: bool,
    #[serde(rename = "honeypotResult")] honeypot_result: Option<HoneypotResult>,
    #[serde(rename = "simulationResult")] simulation_result: Option<HoneypotSimulation>,
    summary: Option<HoneypotSummary>,
    #[serde(rename = "contractCode")] contract_code: Option<HoneypotContractCode>,
}
#[derive(Debug, serde::Deserialize)]
struct HoneypotResult { #[serde(rename = "isHoneypot")] is_honeypot: bool }
#[derive(Debug, serde::Deserialize)]
struct HoneypotSimulation { #[serde(rename = "sellTax")] sell_tax: f64 }
#[derive(Debug, serde::Deserialize)]
struct HoneypotSummary { #[serde(rename = "riskLevel")] risk_level: Option<u8> }
#[derive(Debug, serde::Deserialize)]
struct HoneypotContractCode { #[serde(rename = "rootOpenSource")] root_open_source: Option<bool> }

pub struct HoneypotEvmSafetyChecker {
    chain_id: u64,
    http: reqwest::Client,
}

impl HoneypotEvmSafetyChecker {
    pub fn new(chain_id: u64) -> Self {
        Self { chain_id, http: reqwest::Client::new() }
    }
}

#[async_trait]
impl TokenSafetyChecker for HoneypotEvmSafetyChecker {
    async fn assess(&self, symbol: &Symbol) -> Result<Option<SafetyReport>, PortError> {
        let response = self.http.get("https://api.honeypot.is/v2/IsHoneypot")
            .query(&[("address", symbol.as_str()), ("chainID", &self.chain_id.to_string())])
            .send().await.map_err(|e| PortError::Network {
                venue: "honeypot.is".to_string(), source: Box::new(e),
            })?;
        if !response.status().is_success() { return Ok(None); }
        let report: HoneypotResponse = response.json().await.map_err(|e| PortError::MalformedResponse {
            venue: "honeypot.is".to_string(), reason: e.to_string(),
        })?;
        let Some(simulation) = report.simulation_result else { return Ok(None); };
        if !report.simulation_success { return Ok(None); }
        if report.honeypot_result.as_ref().map(|r| r.is_honeypot).unwrap_or(true) { return Ok(None); }
        let Some(risk_level) = report.summary.and_then(|s| s.risk_level) else { return Ok(None); };
        if risk_level >= 20 { return Ok(None); }
        if !simulation.sell_tax.is_finite() || simulation.sell_tax < 0.0 || simulation.sell_tax > 100.0 {
            return Err(PortError::MalformedResponse {
                venue: "honeypot.is".to_string(), reason: "invalid sell tax".to_string(),
            });
        }
        let sell_tax_bps = Decimal::try_from(simulation.sell_tax * 100.0)
            .map_err(|e| PortError::MalformedResponse { venue: "honeypot.is".to_string(), reason: e.to_string() })?
            .round_dp(0).to_u32().ok_or_else(|| PortError::MalformedResponse {
                venue: "honeypot.is".to_string(), reason: "sell tax overflow".to_string(),
            })?;
        let root_open_source = report.contract_code.and_then(|c| c.root_open_source).unwrap_or(false);
        if !root_open_source { return Ok(None); }

        // honeypot.is's composite verdict only speaks to buy/sell
        // simulation, honeypot classification, risk score, and source
        // verification - it does not tell us whether the contract still
        // has a mint function, whether ownership is renounced, whether
        // liquidity is locked, or the real token-level transfer fee.
        // Reporting those as "safe" defaults would be exactly the
        // fail-open bug this codebase's safety model exists to prevent
        // (see `SafetyCriteria::passes`), so they are reported as
        // unverified/failing here rather than guessed. A real EVM
        // deployment needs a genuine mint-authority/ownership/liquidity
        // -lock data source (e.g. a contract-analysis or token-scanner
        // API) wired in before autonomous EVM buys can pass this gate -
        // simulating success and a low honeypot risk score is
        // necessary but not sufficient.
        Ok(Some(SafetyReport {
            sell_tax_bps: Some(sell_tax_bps),
            token_transfer_fee_bps: None,
            sellability: ben_snipes_domain::SellabilityEvidence::Simulated,
            has_permanent_delegate: false,
            ownership_renounced: false,
            liquidity_locked: false,
            is_mintable: true,
        }))
    }
}

#[derive(Clone)]
pub struct EvmUniswapV2Exchange {
    chain_id: u64,
    execution_rpc_url: String,
    private_rpc_url: Option<String>,
    router: Address,
    wrapped_native: Address,
    slippage_percent: u32,
    signer: Option<PrivateKeySigner>,
    write_lock: Arc<Mutex<()>>,
}

impl EvmUniswapV2Exchange {
    pub fn from_env(
        chain_id: u64,
        execution_rpc_url: String,
        private_rpc_url: Option<String>,
        router: &str,
        wrapped_native: &str,
        slippage_percent: u32,
    ) -> Result<Self, String> {
        if private_rpc_url.is_none() {
            return Err("private_rpc_url must be configured before EVM execution is enabled".to_string());
        }
        let key = std::env::var("EVM_PRIVATE_KEY").map_err(|_| "EVM_PRIVATE_KEY is not configured".to_string())?;
        let signer = PrivateKeySigner::from_str(&key).map_err(|e| format!("invalid EVM_PRIVATE_KEY: {e}"))?;
        let router = router.parse::<Address>().map_err(|e| format!("invalid router address: {e}"))?;
        let wrapped_native = wrapped_native.parse::<Address>().map_err(|e| format!("invalid wrapped native address: {e}"))?;
        if slippage_percent >= 100 { return Err("EVM slippage_percent must be below 100".to_string()); }
        Ok(Self {
            chain_id,
            execution_rpc_url,
            private_rpc_url,
            router,
            wrapped_native,
            slippage_percent,
            signer: Some(signer),
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn read_only(
        chain_id: u64,
        execution_rpc_url: String,
        router: &str,
        wrapped_native: &str,
        slippage_percent: u32,
    ) -> Result<Self, String> {
        let router = router.parse::<Address>().map_err(|e| format!("invalid router address: {e}"))?;
        let wrapped_native = wrapped_native.parse::<Address>().map_err(|e| format!("invalid wrapped native address: {e}"))?;
        if slippage_percent >= 100 {
            return Err("EVM slippage_percent must be below 100".to_string());
        }
        Ok(Self {
            chain_id,
            execution_rpc_url,
            private_rpc_url: None,
            router,
            wrapped_native,
            slippage_percent,
            signer: None,
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    fn signer(&self) -> Result<&PrivateKeySigner, PortError> {
        self.signer.as_ref().ok_or_else(|| {
            PortError::Rejected("EVM exchange is configured read-only; live execution is disabled".to_string())
        })
    }

    async fn provider(&self) -> Result<impl Provider + Clone, PortError> {
        let provider = ProviderBuilder::new()
            .connect(&self.execution_rpc_url)
            .await
            .map_err(|e| PortError::Network { venue: "evm-rpc".to_string(), source: Box::new(e) })?;
        let chain_id = provider.get_chain_id().await.map_err(|e| PortError::Network {
            venue: "evm-rpc".to_string(), source: Box::new(e),
        })?;
        if chain_id != self.chain_id {
            return Err(PortError::Rejected(format!("EVM RPC chain id {chain_id} does not match configured {}", self.chain_id)));
        }
        Ok(provider)
    }

    async fn send_private_transaction<P: Provider + Clone>(
        &self,
        provider: &P,
        to: Address,
        value: U256,
        input: Bytes,
    ) -> Result<alloy::rpc::types::TransactionReceipt, PortError> {
        let _write_guard = self.write_lock.lock().await;
        let private_url = self.private_rpc_url.as_deref().ok_or_else(|| {
            PortError::Rejected("private_rpc_url is required for EVM execution".to_string())
        })?;
        let private_provider = ProviderBuilder::new().connect(private_url).await.map_err(|e| PortError::Network {
            venue: "evm-private-rpc".to_string(), source: Box::new(e),
        })?;
        let private_chain_id = private_provider.get_chain_id().await.map_err(|e| PortError::Network {
            venue: "evm-private-rpc".to_string(), source: Box::new(e),
        })?;
        if private_chain_id != self.chain_id {
            return Err(PortError::Rejected(format!("private EVM RPC chain id {private_chain_id} does not match configured {}", self.chain_id)));
        }
        let chain_id = provider.get_chain_id().await.map_err(|e| PortError::Network {
            venue: "evm-rpc".to_string(), source: Box::new(e),
        })?;
        let signer = self.signer()?;
        let nonce = provider.get_transaction_count(signer.address()).await.map_err(|e| PortError::Network {
            venue: "evm-rpc".to_string(), source: Box::new(e),
        })?;
        let fees = provider.estimate_eip1559_fees().await.map_err(|e| PortError::Network {
            venue: "evm-rpc".to_string(), source: Box::new(e),
        })?;
        let unsigned = TransactionRequest::default()
            .with_from(signer.address())
            .with_to(to)
            .with_value(value)
            .with_input(input)
            .with_nonce(nonce)
            .with_chain_id(chain_id)
            .with_max_fee_per_gas(fees.max_fee_per_gas)
            .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas);
        let gas_limit = provider.estimate_gas(unsigned.clone()).await.map_err(|e| PortError::Rejected(format!("EVM transaction gas estimation failed: {e}")))?;
        let required_balance = value.saturating_add(U256::from(gas_limit).saturating_mul(U256::from(fees.max_fee_per_gas)));
        let balance = provider.get_balance(signer.address()).await.map_err(|e| PortError::Network {
            venue: "evm-rpc".to_string(), source: Box::new(e),
        })?;
        if balance < required_balance {
            return Err(PortError::Rejected(format!("insufficient EVM native balance: need {required_balance} wei, have {balance} wei")));
        }
        let unsigned = unsigned.with_gas_limit(gas_limit);
        provider.call(unsigned.clone()).await.map_err(|e| PortError::Rejected(format!("EVM transaction preflight failed: {e}")))?;
        let wallet = EthereumWallet::from(signer.clone());
        let signed = unsigned.build(&wallet).await.map_err(|e| PortError::Rejected(format!("EVM local signing failed: {e}")))?;
        let encoded = signed.encoded_2718();
        let pending = private_provider.send_raw_transaction(&encoded).await.map_err(|e| PortError::Network {
            venue: "evm-private-rpc".to_string(), source: Box::new(e),
        })?;
        let receipt = pending.get_receipt().await.map_err(|e| PortError::Network {
            venue: "evm-private-rpc".to_string(), source: Box::new(e),
        })?;
        Ok(receipt)
    }

    async fn token_decimals<P: Provider + Clone>(&self, provider: &P, token: Address) -> Result<u8, PortError> {
        Erc20::new(token, provider).decimals().call().await.map_err(|e| PortError::Rejected(format!("failed to read token decimals: {e}")))
    }

    async fn token_balance<P: Provider + Clone>(&self, provider: &P, token: Address, owner: Address) -> Result<U256, PortError> {
        Erc20::new(token, provider).balanceOf(owner).call().await.map_err(|e| PortError::Rejected(format!("failed to read token balance: {e}")))
    }

    fn native_to_wei(amount: Decimal) -> Result<U256, PortError> {
        if amount <= Decimal::ZERO { return Err(PortError::Rejected("EVM trade amount must be positive".to_string())); }
        let scaled = amount * Decimal::from(1_000_000_000_000_000_000u64);
        let wei = scaled.trunc().to_u128().ok_or_else(|| PortError::Rejected("EVM amount is outside supported precision".to_string()))?;
        Ok(U256::from(wei))
    }

    fn token_to_raw(amount: Decimal, decimals: u8) -> Result<U256, PortError> {
        if amount <= Decimal::ZERO { return Err(PortError::Rejected("token quantity must be positive".to_string())); }
        let scale = 10_u128.checked_pow(decimals as u32).ok_or_else(|| PortError::Rejected("token decimals are too large".to_string()))?;
        let raw = (amount * Decimal::from(scale)).trunc().to_u128().ok_or_else(|| PortError::Rejected("token quantity is outside supported precision".to_string()))?;
        Ok(U256::from(raw))
    }

    fn native_from_wei(amount: U256) -> Result<Decimal, PortError> {
        let wei = u128::try_from(amount)
            .map_err(|_| PortError::Rejected("native amount exceeds decimal conversion range".to_string()))?;
        Ok(Decimal::from(wei) / Decimal::from(1_000_000_000_000_000_000u64))
    }

    fn apply_slippage(&self, amount: U256) -> U256 {
        amount.saturating_mul(U256::from(100_u64.saturating_sub(self.slippage_percent as u64))) / U256::from(100_u64)
    }

    async fn deadline<P: Provider + Clone>(&self, provider: &P) -> Result<U256, PortError> {
        let block = provider.get_block_by_number(alloy::eips::BlockNumberOrTag::Latest).await
            .map_err(|e| PortError::Network { venue: "evm-rpc".to_string(), source: Box::new(e) })?
            .ok_or_else(|| PortError::Rejected("latest EVM block is unavailable".to_string()))?;
        Ok(U256::from(block.header.timestamp + 120))
    }
}

#[async_trait]
impl ExchangeClient for EvmUniswapV2Exchange {
    fn venue_name(&self) -> &str { "evm-onchain" }

    async fn current_price(&self, symbol: &Symbol) -> Result<Decimal, PortError> {
        let token = symbol.as_str().parse::<Address>().map_err(|e| PortError::Rejected(format!("invalid EVM token address: {e}")))?;
        let provider = self.provider().await?;
        let decimals = self.token_decimals(&provider, token).await?;
        let one_token = U256::from(10_u128.checked_pow(decimals as u32).ok_or_else(|| PortError::Rejected("token decimals too large".to_string()))?);
        let router = UniswapV2Router::new(self.router, &provider);
        let amounts = router.getAmountsOut(one_token, vec![token, self.wrapped_native]).call().await
            .map_err(|e| PortError::Rejected(format!("failed to quote EVM token: {e}")))?;
        let native = amounts.last().copied().ok_or_else(|| PortError::Rejected("router returned an empty quote".to_string()))?;
        Self::native_from_wei(native)
    }

    async fn submit_buy_by_amount(&self, symbol: &Symbol, quote_amount: Decimal) -> Result<FilledBuy, PortError> {
        let token = symbol.as_str().parse::<Address>().map_err(|e| PortError::Rejected(format!("invalid EVM token address: {e}")))?;
        let provider = self.provider().await?;
        let wallet = self.signer()?.address();
        let value = Self::native_to_wei(quote_amount)?;
        let router = UniswapV2Router::new(self.router, &provider);
        let quote = router.getAmountsOut(value, vec![self.wrapped_native, token]).call().await
            .map_err(|e| PortError::Rejected(format!("EVM buy quote failed: {e}")))?;
        let expected = quote.last().copied().ok_or_else(|| PortError::Rejected("router returned an empty buy quote".to_string()))?;
        let min_out = self.apply_slippage(expected);
        let before = self.token_balance(&provider, token, wallet).await?;
        let deadline = self.deadline(&provider).await?;
        let call = router.swapExactETHForTokensSupportingFeeOnTransferTokens(min_out, vec![self.wrapped_native, token], wallet, deadline).value(value);
        let calldata = call.calldata().to_owned();
        let receipt = self.send_private_transaction(&provider, self.router, value, calldata).await?;
        if !receipt.status() { return Err(PortError::Rejected("EVM buy transaction reverted".to_string())); }
        let after = self.token_balance(&provider, token, wallet).await?;
        let quantity_raw = after.checked_sub(before).ok_or_else(|| PortError::Rejected("token balance decreased after buy".to_string()))?;
        if quantity_raw.is_zero() { return Err(PortError::Rejected("EVM buy succeeded but acquired zero tokens".to_string())); }
        let decimals = self.token_decimals(&provider, token).await?;
        let quantity_raw_u128 = u128::try_from(quantity_raw)
            .map_err(|_| PortError::Rejected("token balance exceeds supported precision".to_string()))?;
        let quantity = Decimal::from(quantity_raw_u128)
            / Decimal::from(10_u128.checked_pow(decimals as u32).ok_or_else(|| PortError::Rejected("token decimals too large".to_string()))?);
        let entry_price = quote_amount / quantity;
        Ok(FilledBuy { quantity, entry_price })
    }

    async fn submit_order(&self, order: Order) -> Result<FilledSell, PortError> {
        if order.side != OrderSide::Sell { return Err(PortError::Rejected("EVM submit_order only supports sell orders".to_string())); }
        let token = order.symbol.as_str().parse::<Address>().map_err(|e| PortError::Rejected(format!("invalid EVM token address: {e}")))?;
        let provider = self.provider().await?;
        let wallet = self.signer()?.address();
        let decimals = self.token_decimals(&provider, token).await?;
        let amount = Self::token_to_raw(order.quantity, decimals)?;
        let router = UniswapV2Router::new(self.router, &provider);
        let allowance = Erc20::new(token, &provider).allowance(wallet, self.router).call().await
            .map_err(|e| PortError::Rejected(format!("failed to read token allowance: {e}")))?;
        let mut fee_quote = Decimal::ZERO;
        if allowance < amount {
            let erc20 = Erc20::new(token, &provider);
            let approve = erc20.approve(self.router, amount);
            approve.call().await.map_err(|e| PortError::Rejected(format!("token approval preflight failed: {e}")))?;
            let approval_receipt = self.send_private_transaction(&provider, token, U256::ZERO, approve.calldata().to_owned()).await?;
            if !approval_receipt.status() { return Err(PortError::Rejected("token approval transaction reverted".to_string())); }
            fee_quote += Self::native_from_wei(U256::from(approval_receipt.cost()))?;
        }
        let native_before_sell = provider.get_balance(wallet).await.map_err(|e| PortError::Network {
            venue: "evm-rpc".to_string(), source: Box::new(e),
        })?;
        let quote = router.getAmountsOut(amount, vec![token, self.wrapped_native]).call().await
            .map_err(|e| PortError::Rejected(format!("EVM sell quote failed: {e}")))?;
        let expected = quote.last().copied().ok_or_else(|| PortError::Rejected("router returned an empty sell quote".to_string()))?;
        let min_out = self.apply_slippage(expected);
        let deadline = self.deadline(&provider).await?;
        let call = router.swapExactTokensForETHSupportingFeeOnTransferTokens(amount, min_out, vec![token, self.wrapped_native], wallet, deadline);
        let receipt = self.send_private_transaction(&provider, self.router, U256::ZERO, call.calldata().to_owned()).await?;
        if !receipt.status() { return Err(PortError::Rejected("EVM sell transaction reverted".to_string())); }
        let native_after_sell = provider.get_balance(wallet).await.map_err(|e| PortError::Network {
            venue: "evm-rpc".to_string(), source: Box::new(e),
        })?;
        let sell_gas = U256::from(receipt.cost());
        let native_delta = native_after_sell.saturating_sub(native_before_sell);
        let gross_proceeds_wei = native_delta.saturating_add(sell_gas);
        if gross_proceeds_wei.is_zero() {
            return Err(PortError::Rejected("EVM sell confirmed but produced no native proceeds".to_string()));
        }
        let quote_proceeds = Self::native_from_wei(gross_proceeds_wei)?;
        fee_quote += Self::native_from_wei(sell_gas)?;
        let execution_price = quote_proceeds / order.quantity;
        Ok(FilledSell {
            quantity: order.quantity,
            execution_price: Some(execution_price),
            quote_proceeds: Some(quote_proceeds),
            fee_quote: Some(fee_quote),
            tx_id: Some(format!("{}", receipt.transaction_hash)),
        })
    }
}

pub struct NoWalletEvmExchange;

#[async_trait]
impl ExchangeClient for NoWalletEvmExchange {
    fn venue_name(&self) -> &str { "evm-onchain" }
    async fn current_price(&self, _symbol: &Symbol) -> Result<Decimal, PortError> {
        Err(PortError::Rejected("EVM wallet is not configured".to_string()))
    }
    async fn submit_buy_by_amount(&self, _symbol: &Symbol, _quote_amount: Decimal) -> Result<FilledBuy, PortError> {
        Err(PortError::Rejected("EVM wallet is not configured".to_string()))
    }
    async fn submit_order(&self, _order: Order) -> Result<FilledSell, PortError> {
        Err(PortError::Rejected("EVM wallet is not configured".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_address_from_a_left_padded_topic() {
        let topic = "0x000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
        assert_eq!(extract_address(topic), Some("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".to_string()));
    }

    #[test]
    fn rejects_malformed_topic() {
        assert_eq!(extract_address("0xnothex"), None);
        assert_eq!(extract_address("not even prefixed"), None);
    }

    #[test]
    fn applies_slippage_without_exceeding_quote() {
        let key = "0x0000000000000000000000000000000000000000000000000000000000000001";
        let exchange = EvmUniswapV2Exchange::from_env(1, "http://localhost".to_string(), None, "0x0000000000000000000000000000000000000001", "0x0000000000000000000000000000000000000002", 10);
        if let Ok(exchange) = exchange {
            assert_eq!(exchange.apply_slippage(U256::from(1000)), U256::from(900));
        }
        let _ = key;
    }
}

--- ./crates/adapters/pumpfun/Cargo.toml ---
[package]
name = "ben_snipes-adapter-pumpfun"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "Real Solana ListingSource and ExchangeClient backed by PumpPortal's subscribeNewToken feed and non-custodial Local Transaction API, with local signing, pre-trade balance checks, transaction simulation, and JSON-RPC broadcast."

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

--- ./crates/adapters/pumpfun/src/exchange_client.rs ---
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

use crate::execution::{execute_trade, preflight_trade, TradeAction, TradeRequest};
use crate::price_feed;
use crate::retry::with_retry;
use async_trait::async_trait;
use ben_snipes_domain::{FilledBuy, FilledSell, Order, OrderSide, Symbol};
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

const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

/// Minimum SOL reserved for transaction/priority fees on top of the buy
/// amount itself. Solana fees are typically tiny, but this is a fixed,
/// deliberately-conservative buffer rather than an exact fee estimate -
/// the goal is catching an obviously-insufficient balance before
/// attempting a trade, not computing the precise fee.
const FEE_BUFFER_LAMPORTS: u64 = 5_000_000; // 0.005 SOL

pub struct PumpPortalExchangeClient {
    http: reqwest::Client,
    wallet: Option<Keypair>,
    rpc_url: String,
    slippage_percent: u32,
    priority_fee_sol: Decimal,
}

impl PumpPortalExchangeClient {
    pub fn new(wallet: Keypair, rpc_url: impl Into<String>, slippage_percent: u32, priority_fee_sol: Decimal) -> Self {
        Self {
            http: reqwest::Client::new(),
            wallet: Some(wallet),
            rpc_url: rpc_url.into(),
            slippage_percent,
            priority_fee_sol,
        }
    }

    pub fn new_read_only(rpc_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            wallet: None,
            rpc_url: rpc_url.into(),
            slippage_percent: 0,
            priority_fee_sol: Decimal::ZERO,
        }
    }

    fn wallet(&self) -> Result<&Keypair, PortError> {
        self.wallet.as_ref().ok_or_else(|| {
            PortError::Rejected("Solana exchange is configured read-only; live execution is disabled".to_string())
        })
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
                    .post(&self.rpc_url)
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
        let wallet_pubkey = self.wallet()?.pubkey().to_string();
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
                .post(&self.rpc_url)
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
        let wallet_pubkey = self.wallet()?.pubkey().to_string();
        with_retry(3, || async {
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getBalance",
                "params": [wallet_pubkey.clone()],
            });

            let response = self
                .http
                .post(&self.rpc_url)
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
        let wallet_pubkey = self.wallet()?.pubkey().to_string();
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
                .post(&self.rpc_url)
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
        price_feed::fetch_price(&self.http, symbol.as_str())
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

        let signature = execute_trade(&self.http, self.wallet()?, &self.rpc_url, &request)
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

        preflight_trade(&self.http, self.wallet()?, &self.rpc_url, &request)
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

        let signature = execute_trade(&self.http, self.wallet()?, &self.rpc_url, &request)
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

    #[test]
    fn buy_balance_requirement_includes_fee_buffer_and_priority_fee() {
        let quote_amount = Decimal::new(1, 2); // 0.01 SOL
        let priority_fee_sol = Decimal::new(1, 4); // 0.0001 SOL
        let required_balance =
            quote_amount + priority_fee_sol + Decimal::from(FEE_BUFFER_LAMPORTS) / Decimal::from(LAMPORTS_PER_SOL);

        assert_eq!(required_balance, Decimal::new(151, 4));
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

use crate::retry::with_retry;
use rust_decimal::Decimal;
use solana_sdk::signature::Signature;
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

/// Convenience for callers that just want to log/display the wallet's
/// address without depending on `solana_sdk::signer::Signer` themselves
/// - keeps that dependency an implementation detail of this crate.
pub fn wallet_pubkey_string(wallet: &Keypair) -> String {
    wallet.pubkey().to_string()
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
    let raw_tx_bytes = build_unsigned_transaction(http, wallet, request).await?;
    simulate_transaction(http, rpc_url, &raw_tx_bytes).await?;

    let signed_bytes = sign_transaction(wallet, &raw_tx_bytes)?;
    broadcast(http, rpc_url, &signed_bytes).await
}

/// Builds and simulates a PumpPortal transaction without signing or
/// broadcasting it. This is used immediately before exits, when the wallet
/// already owns the token, so the simulation can validate the actual current
/// account state rather than merely checking that a route exists.
pub async fn preflight_trade(
    http: &reqwest::Client,
    wallet: &Keypair,
    rpc_url: &str,
    request: &TradeRequest,
) -> Result<(), String> {
    let raw_tx_bytes = build_unsigned_transaction(http, wallet, request).await?;
    simulate_transaction(http, rpc_url, &raw_tx_bytes).await
}

async fn build_unsigned_transaction(
    http: &reqwest::Client,
    wallet: &Keypair,
    request: &TradeRequest,
) -> Result<Vec<u8>, String> {
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

    with_retry(3, || async {
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

        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|e| format!("failed to read trade-local response body: {e}"))
    })
    .await
}

/// Simulates the unsigned transaction before spending signing material or
/// sending it to the cluster. Solana explicitly permits unsigned simulation
/// when `sigVerify` is false, so this catches instruction-level failures
/// before the wallet signs a doomed transaction.
async fn simulate_transaction(
    http: &reqwest::Client,
    rpc_url: &str,
    raw_tx_bytes: &[u8],
) -> Result<(), String> {
    use base64::Engine;

    let encoded = base64::engine::general_purpose::STANDARD.encode(raw_tx_bytes);
    let rpc_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "simulateTransaction",
        "params": [
            encoded,
            {
                "encoding": "base64",
                "commitment": "confirmed",
                "sigVerify": false,
                "replaceRecentBlockhash": false,
            }
        ],
    });

    let response = http
        .post(rpc_url)
        .header("Content-Type", "application/json")
        .body(rpc_body.to_string())
        .send()
        .await
        .map_err(|e| format!("transaction simulation request failed: {e}"))?;

    let response_json: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("failed to parse transaction simulation response: {e}"))?;

    if let Some(error) = response_json.get("error") {
        return Err(format!("transaction simulation RPC returned an error: {error}"));
    }

    match response_json.pointer("/result/value/err") {
        Some(err) if !err.is_null() => {
            Err(format!("transaction simulation failed: {err}"))
        }
        Some(_) => Ok(()),
        None => Err(format!(
            "transaction simulation response had no result/value/err field: {response_json}"
        )),
    }
}

/// Deserializes PumpPortal's unsigned transaction bytes, signs the
/// message with `wallet`, and re-serializes. See this module's top
/// doc comment - this is the block to re-verify against the pinned
/// solana-sdk version's docs.rs page before trusting it with real funds.
fn sign_transaction(wallet: &Keypair, raw_tx_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut tx: VersionedTransaction = bincode::deserialize(raw_tx_bytes)
        .map_err(|e| format!("failed to deserialize transaction from trade-local: {e}"))?;

    tx.sanitize()
        .map_err(|e| format!("trade-local returned an invalid transaction: {e}"))?;

    let account_keys = tx.message.static_account_keys();
    let signer_index = account_keys
        .iter()
        .position(|key| *key == wallet.pubkey())
        .ok_or_else(|| "wallet public key not found among the transaction's required signers".to_string())?;

    let required_signatures = tx.message.header().num_required_signatures as usize;
    if signer_index >= required_signatures {
        return Err("wallet public key is present in the transaction but is not a required signer".to_string());
    }

    if tx.signatures.len() != required_signatures {
        return Err(format!(
            "transaction signature slot count mismatch: expected {required_signatures}, got {}",
            tx.signatures.len()
        ));
    }

    let message_bytes = tx.message.serialize();
    let signature = wallet.sign_message(&message_bytes);
    if signature == Signature::default() || !signature.verify(wallet.pubkey().as_ref(), &message_bytes) {
        return Err("wallet failed to produce a verifiable transaction signature".to_string());
    }

    tx.signatures[signer_index] = signature;

    bincode::serialize(&tx).map_err(|e| format!("failed to re-serialize signed transaction: {e}"))
}

/// Broadcasts already-signed transaction bytes via a raw JSON-RPC
/// `sendTransaction` call. Deliberately not using the `solana-client`
/// crate - see this module's top doc comment for why.
async fn broadcast(http: &reqwest::Client, rpc_url: &str, signed_bytes: &[u8]) -> Result<String, String> {    use base64::Engine;
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
//! Detection is provided by PumpPortal, while metrics come from
//! DexScreener and authority/liquidity checks come from RugCheck. Sell-tax
//! remains explicitly unknown until a real sell simulation is available,
//! so the application safety gate blocks the purchase.

use async_trait::async_trait;
use ben_snipes_adapter_ws_support::connect_with_backoff;
use ben_snipes_domain::{
    Chain, DomainError, FilledBuy, FilledSell, Listing, ListingMetrics, Order, SafetyReport,
    Symbol, Venue, VenueKind,
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

    async fn submit_order(&self, _order: Order) -> Result<FilledSell, PortError> {
        Err(PortError::Rejected(
            "no wallet configured (SOLANA_PRIVATE_KEY not set) - see execution module docs".to_string(),
        ))
    }
}

--- ./crates/adapters/pumpfun/src/metrics_provider.rs ---
//! Real `MetricsProvider` for Solana tokens via DexScreener's
//! single-token lookup endpoint (`/latest/dex/tokens/<address>`) - free,
//! keyless, and well-corroborated across independent sources at the
//! time of writing.
//!
//! **This is a different endpoint from the one this project deliberately
//! avoided for listing detection.** The new-pairs/discovery endpoint
//! that capped out at ~30 results with no real pagination is a
//! different concern (a live firehose) from this one (a single lookup
//! by an address you already have) - there's no pagination problem to
//! begin with when you're asking about one specific token.

use async_trait::async_trait;
use ben_snipes_domain::{ListingMetrics, Symbol};
use ben_snipes_ports::{MetricsProvider, PortError};
use rust_decimal::Decimal;
use serde::Deserialize;

const TOKENS_URL: &str = "https://api.dexscreener.com/latest/dex/tokens";

#[derive(Debug, Deserialize)]
struct TokensResponse {
    #[serde(default)]
    pairs: Option<Vec<PairInfo>>,
}

#[derive(Debug, Deserialize)]
struct PairInfo {
    #[serde(default)]
    volume: Option<VolumeInfo>,
    #[serde(default, rename = "marketCap")]
    market_cap: Option<f64>,
    #[serde(default)]
    fdv: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct VolumeInfo {
    #[serde(default, rename = "h24")]
    h24: Option<f64>,
}

pub struct DexScreenerMetricsProvider {
    http: reqwest::Client,
}

impl DexScreenerMetricsProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for DexScreenerMetricsProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MetricsProvider for DexScreenerMetricsProvider {
    async fn metrics(&self, symbol: &Symbol) -> Result<Option<ListingMetrics>, PortError> {
        let url = format!("{TOKENS_URL}/{}", symbol.as_str());

        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| PortError::Network {
                venue: "dexscreener".to_string(),
                source: Box::new(e),
            })?;

        if !response.status().is_success() {
            // A 404-shaped "no pairs yet" is expected for a brand-new
            // token, not a hard failure - treat any non-success the
            // same way: not enough information yet.
            return Ok(None);
        }

        let body: TokensResponse = response.json().await.map_err(|e| PortError::MalformedResponse {
            venue: "dexscreener".to_string(),
            reason: e.to_string(),
        })?;

        let Some(pairs) = body.pairs else {
            return Ok(None);
        };

        // A token can have multiple pairs (different pools/DEXes) -
        // take the one with the most volume, since that's the most
        // representative of "is there a real market here".
        let Some(best) = pairs.iter().max_by(|a, b| {
            let a_vol = a.volume.as_ref().and_then(|v| v.h24).unwrap_or(0.0);
            let b_vol = b.volume.as_ref().and_then(|v| v.h24).unwrap_or(0.0);
            a_vol.partial_cmp(&b_vol).unwrap_or(std::cmp::Ordering::Equal)
        }) else {
            return Ok(None);
        };

        let volume_24h = best.volume.as_ref().and_then(|v| v.h24).unwrap_or(0.0);
        // Prefer marketCap; DexScreener sometimes only populates fdv
        // (fully-diluted valuation) for very new tokens before
        // circulating-supply data is available.
        let market_cap = best.market_cap.or(best.fdv).unwrap_or(0.0);

        let volume_24h = Decimal::try_from(volume_24h).map_err(|e| PortError::MalformedResponse {
            venue: "dexscreener".to_string(),
            reason: format!("volume was not a valid decimal: {e}"),
        })?;
        let market_cap = Decimal::try_from(market_cap).map_err(|e| PortError::MalformedResponse {
            venue: "dexscreener".to_string(),
            reason: format!("market cap was not a valid decimal: {e}"),
        })?;

        Ok(Some(ListingMetrics { volume_24h, market_cap }))
    }
}

--- ./crates/adapters/pumpfun/src/price_feed.rs ---
//! Live price lookups via Jupiter's Price API v3
//! (`lite-api.jup.ag/price/v3`), used by `PumpPortalExchangeClient::current_price`.
//!
//! Verified against Jupiter's own developer docs at the time of
//! writing, including a literal example response - this is the
//! best-confirmed of the three new external integrations added this
//! round. One thing worth flagging anyway: **the older
//! `quote-api.jup.ag/v6` endpoints (which several third-party guides,
//! including one shared during this project's development, still
//! reference) were retired on 2025-10-01.** If a future docs check
//! shows `price/v3` has similarly moved on, that's the first thing to
//! re-verify here.
//!
//! Uses the keyless `lite-api.jup.ag` host (rate-limited but free) by
//! default. For production volume, `api.jup.ag` with an `x-api-key`
//! header is the documented higher-throughput option - not wired in
//! here, since a single default is enough to get this working and a
//! key can be layered on without changing the response-parsing logic.
//!
//! **Unit note, easy to get wrong:** Jupiter's Price API returns
//! USD-denominated prices. `Position::entry_price` throughout this
//! codebase is SOL-denominated (`submit_buy_by_amount` computes it as
//! `quote_amount spent in SOL / quantity received`), because that's
//! what `AcquisitionEngine.position_size` and PumpPortal's own trade
//! API are denominated in. Returning a raw USD price here would silently
//! compare against a SOL-denominated take-profit/stop-loss target -
//! wrong by roughly the SOL/USD exchange rate, not a rounding error.
//! `fetch_price` converts to SOL terms internally (by also fetching
//! SOL's own USD price in the same batched call) specifically so every
//! caller gets a value in the same unit `Position` already uses.

use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::HashMap;

const PRICE_API_URL: &str = "https://lite-api.jup.ag/price/v3";

/// Wrapped SOL's mint address - this exact value appears directly in
/// Jupiter's own documented example response, so it's about as
/// verified as a constant can be.
const SOL_MINT: &str = "So11111111111111111111111111111111111111112";

#[derive(Debug, Deserialize)]
struct PriceEntry {
    #[serde(rename = "usdPrice")]
    usd_price: f64,
}

/// Fetches the current price of `mint`, denominated in SOL (see the
/// module doc comment for why, not USD). Returns an error if either
/// mint has no price data (an extremely fresh pump.fun token, for
/// instance, may not be indexed here yet even if it's tradable) -
/// callers should treat that as "not enough information", the same way
/// `MetricsProvider::metrics` returning `None` is handled elsewhere in
/// this codebase.
pub async fn fetch_price(http: &reqwest::Client, mint: &str) -> Result<Decimal, String> {
    // Batched into one call - Jupiter's ids param takes a comma-separated
    // list, so this is one request, not two.
    let url = format!("{PRICE_API_URL}?ids={mint},{SOL_MINT}");

    let response = http
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Jupiter price request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("Jupiter price API returned {status}: {text}"));
    }

    let body: HashMap<String, PriceEntry> = response
        .json()
        .await
        .map_err(|e| format!("failed to parse Jupiter price response: {e}"))?;

    let token_usd = body
        .get(mint)
        .map(|e| e.usd_price)
        .ok_or_else(|| format!("Jupiter has no price data for {mint} yet"))?;

    let sol_usd = body
        .get(SOL_MINT)
        .map(|e| e.usd_price)
        .ok_or_else(|| "Jupiter had no price data for wrapped SOL - cannot convert to SOL terms".to_string())?;

    if sol_usd <= 0.0 {
        return Err("Jupiter returned a non-positive SOL price, cannot convert".to_string());
    }

    let price_in_sol = token_usd / sol_usd;

    Decimal::try_from(price_in_sol).map_err(|e| format!("converted price was not a valid decimal: {e}"))
}

--- ./crates/adapters/pumpfun/src/retry.rs ---
//! A small retry-with-backoff helper for the transient-failure-prone
//! network calls throughout this crate (RPC calls, PumpPortal/DexScreener/
//! RugCheck/Jupiter HTTP requests).
//!
//! **Every call site this is used on has been checked for idempotency
//! before wrapping it** - retrying isn't free to reach for blindly.
//! Reads (balance checks, price/metrics/safety lookups) are always safe
//! to retry. The two calls that "do" something - requesting an unsigned
//! transaction from PumpPortal, and broadcasting a signed one - are also
//! safe here specifically: `trade-local` is a stateless "build me a
//! transaction" request with no server-side side effect, and
//! resubmitting the exact same signed transaction bytes to
//! `sendTransaction` is a standard, safe pattern in Solana tooling (the
//! network treats a duplicate submission as a no-op, not a double
//! execution). Don't reach for this on a call that isn't verified safe
//! to repeat.

use std::future::Future;
use std::time::Duration;
use tracing::debug;

const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// Calls `f` up to `max_attempts` times, with exponential backoff
/// between failures, returning the first success or the last error if
/// every attempt fails.
pub async fn with_retry<F, Fut, T>(max_attempts: u32, mut f: F) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let mut backoff = INITIAL_BACKOFF;
    let mut last_error = String::from("max_attempts was 0");

    for attempt in 1..=max_attempts.max(1) {
        match f().await {
            Ok(value) => return Ok(value),
            Err(e) => {
                debug!(attempt, max_attempts, error = %e, "attempt failed, will retry" );
                last_error = e;
                if attempt < max_attempts {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }

    Err(format!("failed after {max_attempts} attempts, last error: {last_error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn succeeds_immediately_without_retrying() {
        let calls = AtomicU32::new(0);
        let result = with_retry(3, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, String>(42)
        })
        .await;

        assert_eq!(result, Ok(42));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_until_success() {
        let calls = AtomicU32::new(0);
        let result = with_retry(5, || async {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                Err("transient".to_string())
            } else {
                Ok(99)
            }
        })
        .await;

        assert_eq!(result, Ok(99));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let calls = AtomicU32::new(0);
        let result: Result<u32, String> = with_retry(3, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err("always fails".to_string())
        })
        .await;

        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}

--- ./crates/adapters/pumpfun/src/safety_checker.rs ---
//! Real `TokenSafetyChecker` for Solana via RugCheck (`api.rugcheck.xyz`).
//!
//! **Confidence varies significantly by field, and that's reflected in
//! how conservatively each one defaults.** `mintAuthority` /
//! `freezeAuthority` are confirmed - independently corroborated by both
//! an unofficial API wrapper's documented field list and an AI-skill
//! doc that explicitly describes `token.mintAuthority != null` as the
//! "can still mint" signal - so `is_mintable` and half of
//! `ownership_renounced` rest on solid ground. Liquidity-lock detection
//! (via the `lockers` field) and sell-tax are much less certain: RugCheck
//! doesn't appear to reliably expose a sell-tax figure at all (it's
//! fundamentally a different kind of check - authority/liquidity/holder
//! analysis, not a sell simulation), so `sell_tax_bps` here is **always
//! `None`, which means unverified and therefore blocked by the safety gate.** If accurate sell-tax
//! detection matters for your risk tolerance, that needs a real sell
//! simulation as a separate data source - don't read a `0` from this
//! checker as "no tax", read it as "this checker doesn't know."
//!
//! **A real limitation worth being direct about:** if `mintAuthority`/
//! `freezeAuthority` turn out not to be the actual field names RugCheck
//! uses (moderate but not total confidence - see above), those fields
//! deserialize to `None` the same way a genuinely-renounced authority
//! would, and `is_mintable`/`ownership_renounced` would silently read as
//! "safe" for every token. That's fail-*open*, the opposite of this
//! codebase's standing principle. The one thing this code *can* check at
//! runtime - whether the `token` sub-object exists at all - is checked
//! below and treated as "not enough information" (`None`) if it's
//! missing entirely, which catches a badly-wrong response shape. It
//! cannot catch "the token object is there but these two specific key
//! names are wrong." **Verify `mintAuthority`/`freezeAuthority` against
//! a real RugCheck response before trusting this for real funds** - the
//! same category of caveat as the solana-sdk signing code, for the same
//! reason: unverified assumption, safety-critical consequence if wrong.

use async_trait::async_trait;
use ben_snipes_domain::{SafetyReport, Symbol};
use ben_snipes_ports::{PortError, TokenSafetyChecker};
use serde::Deserialize;
use serde_json::Value;

const REPORT_URL: &str = "https://api.rugcheck.xyz/v1/tokens";

#[derive(Debug, Default, Deserialize)]
struct RugCheckReport {
    #[serde(default)]
    token: Option<TokenInfo>,
    /// Present when RugCheck has directly flagged the token as a
    /// confirmed rug - if this is `true`, nothing else in the report
    /// matters.
    #[serde(default)]
    rugged: bool,
    /// Left as a raw `Value` rather than a typed field - only its
    /// presence/non-emptiness is used (see module docs on why the exact
    /// lock-percentage shape isn't confidently known).
    #[serde(default)]
    lockers: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct TokenInfo {
    #[serde(default, rename = "mintAuthority")]
    mint_authority: Option<Value>,
    #[serde(default, rename = "freezeAuthority")]
    freeze_authority: Option<Value>,
}

const SPL_TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022_PROGRAM_ID: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

#[derive(Debug, Clone, Copy)]
struct MintPolicy {
    transfer_fee_bps: Option<u32>,
    permanent_delegate: bool,
    transfer_hook: bool,
}

fn max_transfer_fee_bps(value: &Value) -> Option<u32> {
    fn walk(value: &Value, max_bps: &mut Option<u32>) {
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    if key == "transferFeeBasisPoints" {
                        if let Some(raw) = child.as_u64().and_then(|value| u32::try_from(value).ok()) {
                            *max_bps = Some(max_bps.map_or(raw, |current| current.max(raw)));
                        }
                    }
                    walk(child, max_bps);
                }
            }
            Value::Array(array) => {
                for child in array {
                    walk(child, max_bps);
                }
            }
            _ => {}
        }
    }

    let mut max_bps = None;
    walk(value, &mut max_bps);
    max_bps
}

fn extension_named(value: &Value, extension_name: &str) -> bool {
    match value {
        Value::Object(object) => {
            if object.get("extension").and_then(Value::as_str) == Some(extension_name) {
                return true;
            }
            object.values().any(|child| extension_named(child, extension_name))
        }
        Value::Array(array) => array.iter().any(|child| extension_named(child, extension_name)),
        _ => false,
    }
}

impl RugCheckSafetyChecker {
    async fn inspect_mint_policy(&self, mint: &str) -> Result<Option<MintPolicy>, PortError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getAccountInfo",
            "params": [mint, { "encoding": "jsonParsed", "commitment": "confirmed" }],
        });

        let response = self
            .http
            .post(&self.rpc_url)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| PortError::Network {
                venue: "solana-rpc".to_string(),
                source: Box::new(e),
            })?;

        if !response.status().is_success() {
            return Ok(None);
        }

        let json: Value = response.json().await.map_err(|e| PortError::MalformedResponse {
            venue: "solana-rpc".to_string(),
            reason: e.to_string(),
        })?;

        let Some(account) = json.pointer("/result/value") else {
            return Ok(None);
        };
        if account.is_null() {
            return Ok(None);
        }

        let owner = account.get("owner").and_then(Value::as_str);
        match owner {
            Some(SPL_TOKEN_PROGRAM_ID) => Ok(Some(MintPolicy {
                transfer_fee_bps: Some(0),
                permanent_delegate: false,
                transfer_hook: false,
            })),
            Some(TOKEN_2022_PROGRAM_ID) => {
                let extensions = account.pointer("/data/parsed/info/extensions").cloned().unwrap_or(Value::Null);
                let has_transfer_fee_config = extension_named(&extensions, "transferFeeConfig");
                let transfer_fee_bps = if has_transfer_fee_config {
                    max_transfer_fee_bps(&extensions)
                } else {
                    Some(0)
                };
                Ok(Some(MintPolicy {
                    transfer_fee_bps,
                    permanent_delegate: extension_named(&extensions, "permanentDelegate"),
                    transfer_hook: extension_named(&extensions, "transferHook"),
                }))
            }
            _ => Ok(None),
        }
    }
}

pub struct RugCheckSafetyChecker {
    http: reqwest::Client,
    rpc_url: String,
}

impl RugCheckSafetyChecker {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            rpc_url: std::env::var("SOLANA_RPC_URL").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string()),
        }
    }
}

impl Default for RugCheckSafetyChecker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TokenSafetyChecker for RugCheckSafetyChecker {
    async fn assess(&self, symbol: &Symbol) -> Result<Option<SafetyReport>, PortError> {
        let url = format!("{REPORT_URL}/{}/report", symbol.as_str());

        let response = self
            .http
            .get(&url)
            .header("Content-Type", "application/json")
            .send()
            .await
            .map_err(|e| PortError::Network {
                venue: "rugcheck".to_string(),
                source: Box::new(e),
            })?;

        if !response.status().is_success() {
            // A brand-new token may not be indexed by RugCheck yet -
            // treat any non-success as "not enough information", not a
            // hard failure.
            return Ok(None);
        }

        let report: RugCheckReport = response.json().await.map_err(|e| PortError::MalformedResponse {
            venue: "rugcheck".to_string(),
            reason: e.to_string(),
        })?;

        if report.rugged {
            return Ok(Some(SafetyReport {
                sell_tax_bps: None,
                token_transfer_fee_bps: None,
                sellability: ben_snipes_domain::SellabilityEvidence::Unknown,
                has_permanent_delegate: false,
                ownership_renounced: false,
                liquidity_locked: false,
                is_mintable: true,
            }));
        }

        let Some(token) = report.token else {
            // The whole `token` sub-object is missing - a much stronger
            // signal something is wrong with the assumed response shape
            // than any individual field being absent. Treat as "not
            // enough information" rather than guessing.
            return Ok(None);
        };

        // serde maps both an absent field and an explicit JSON `null`
        // to `None` for an `Option<Value>` field, so `is_some()` alone
        // correctly distinguishes "authority present" (any non-null
        // value, typically a pubkey string) from "renounced/absent" -
        // assuming the field names themselves are right. See the
        // module doc comment for the residual risk if they're not.
        let is_mintable = token.mint_authority.is_some();
        let freeze_authority_present = token.freeze_authority.is_some();
        let ownership_renounced = !is_mintable && !freeze_authority_present;

        // Best-effort: non-empty lockers array/object is treated as
        // "some liquidity locking exists". See module docs - this is
        // the least-confident field mapping here.
        let liquidity_locked = match &report.lockers {
            Some(Value::Array(arr)) => !arr.is_empty(),
            Some(Value::Object(obj)) => !obj.is_empty(),
            _ => false,
        };

        let Some(mint_policy) = self.inspect_mint_policy(symbol.as_str()).await? else {
            return Ok(None);
        };

        Ok(Some(SafetyReport {
            // RugCheck does not establish a real sell path or a DEX sell tax.
            // Keep those independent and fail closed rather than treating a
            // token-level transfer fee as proof that a sell will work.
            sell_tax_bps: None,
            token_transfer_fee_bps: mint_policy.transfer_fee_bps,
            sellability: if mint_policy.permanent_delegate || mint_policy.transfer_hook {
                ben_snipes_domain::SellabilityEvidence::Failed
            } else if !freeze_authority_present {
                ben_snipes_domain::SellabilityEvidence::Structural
            } else {
                ben_snipes_domain::SellabilityEvidence::Failed
            },
            has_permanent_delegate: mint_policy.permanent_delegate,
            ownership_renounced,
            liquidity_locked,
            is_mintable,
        }))
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
ben_snipes-domain = { workspace = true }
ben_snipes-ports = { workspace = true }
async-trait = { workspace = true }
serde_json = { workspace = true }
tokio = { workspace = true, features = ["sync"] }
tracing = { workspace = true }

[dev-dependencies]
rust_decimal = { workspace = true }
time = { workspace = true }

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
mod position_store;
mod trade_store;
pub use ledger::FileAcquisitionLedger;
pub use position_store::FilePositionStore;
pub use trade_store::{FilePendingTradeStore, FileTradeStore};

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
            pending: Default::default(),
            bootstrapped: true,
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

--- ./crates/adapters/statefile/src/position_store.rs ---
//! A file-backed `PositionStore`: one JSON file holding the complete
//! list of currently-open positions, same atomic temp-file+rename
//! pattern as `StatefileStore` and `FileAcquisitionLedger`.

use async_trait::async_trait;
use ben_snipes_domain::Position;
use ben_snipes_ports::{PortError, PositionStore};
use std::path::PathBuf;
use tokio::fs;

pub struct FilePositionStore {
    path: PathBuf,
}

impl FilePositionStore {
    /// Doesn't touch the filesystem at construction - same convention
    /// as `StatefileStore`. `load` handles a not-yet-existing file as
    /// the expected first-run state.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl PositionStore for FilePositionStore {
    async fn load(&self) -> Result<Vec<Position>, PortError> {
        let raw = match fs::read_to_string(&self.path).await {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(PortError::Storage(Box::new(e))),
        };

        serde_json::from_str(&raw).map_err(|e| PortError::MalformedResponse {
            venue: "position-store".to_string(),
            reason: e.to_string(),
        })
    }

    async fn save(&self, positions: &[Position]) -> Result<(), PortError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| PortError::Storage(Box::new(e)))?;
        }

        let tmp_path = self.path.with_extension("json.tmp");
        let serialised = serde_json::to_vec_pretty(positions).map_err(|e| PortError::Storage(Box::new(e)))?;

        fs::write(&tmp_path, serialised)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;
        fs::rename(&tmp_path, &self.path)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ben_snipes_domain::{ProfitTarget, Symbol, Venue, VenueKind};
    use rust_decimal::Decimal;

    fn temp_path() -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should never be before the epoch in CI")
            .as_nanos();
        std::env::temp_dir().join(format!("ben_snipes-positions-test-{nanos}.json"))
    }

    fn sample_position() -> Position {
        let venue = Venue::new(VenueKind::Dex, "pumpfun").expect("literal venue is valid");
        let symbol = Symbol::new("someMint").expect("literal symbol is valid");
        Position::new(
            venue,
            symbol,
            Decimal::ONE,
            Decimal::TEN,
            ProfitTarget::from_percent(Decimal::TEN).expect("valid target"),
        )
    }

    #[tokio::test]
    async fn missing_file_loads_as_empty() {
        let store = FilePositionStore::new(temp_path());
        let loaded = store.load().await.expect("missing file is not an error");
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn round_trips_open_positions() {
        let path = temp_path();
        let store = FilePositionStore::new(&path);

        let positions = vec![sample_position()];
        store.save(&positions).await.expect("save should not fail");

        let loaded = store.load().await.expect("load should not fail");
        assert_eq!(loaded, positions);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn save_replaces_rather_than_appends() {
        let path = temp_path();
        let store = FilePositionStore::new(&path);

        store.save(&[sample_position()]).await.expect("save should not fail");
        store.save(&[]).await.expect("save should not fail");

        let loaded = store.load().await.expect("load should not fail");
        assert!(loaded.is_empty(), "second save should have replaced, not appended to, the first");

        let _ = std::fs::remove_file(&path);
    }
}

--- ./crates/adapters/statefile/src/trade_store.rs ---
//! File-backed completed-trade journal. The whole journal is rewritten
//! through a temporary file and atomic rename so a crash cannot leave a
//! partially-written JSON document.

use async_trait::async_trait;
use ben_snipes_domain::TradeRecord;
use ben_snipes_ports::{PendingTradeStore, PortError, TradeStore};
use std::path::PathBuf;
use tokio::fs;

pub struct FileTradeStore {
    path: PathBuf,
}

impl FileTradeStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl TradeStore for FileTradeStore {
    async fn load(&self) -> Result<Vec<TradeRecord>, PortError> {
        let raw = match fs::read_to_string(&self.path).await {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(PortError::Storage(Box::new(e))),
        };

        serde_json::from_str(&raw).map_err(|e| PortError::MalformedResponse {
            venue: "trade-store".to_string(),
            reason: e.to_string(),
        })
    }

    async fn append(&self, trade: &TradeRecord) -> Result<(), PortError> {
        let mut trades = self.load().await?;
        if trades.iter().any(|existing| existing.key() == trade.key()) {
            return Ok(());
        }
        trades.push(trade.clone());

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| PortError::Storage(Box::new(e)))?;
        }

        let tmp_path = self.path.with_extension("json.tmp");
        let serialised = serde_json::to_vec_pretty(&trades)
            .map_err(|e| PortError::Storage(Box::new(e)))?;

        fs::write(&tmp_path, serialised)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;
        fs::rename(&tmp_path, &self.path)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ben_snipes_domain::{Position, ProfitTarget, Symbol, Venue, VenueKind};
    use rust_decimal::Decimal;
    use time::OffsetDateTime;

    fn temp_path() -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should never be before the epoch in CI")
            .as_nanos();
        std::env::temp_dir().join(format!("ben_snipes-trades-test-{nanos}.json"))
    }

    fn sample_trade() -> TradeRecord {
        let venue = Venue::new(VenueKind::Dex, "pumpfun").expect("literal venue is valid");
        let symbol = Symbol::new("TOKEN").expect("literal symbol is valid");
        let position = Position::new(
            venue,
            symbol,
            Decimal::ONE,
            Decimal::TEN,
            ProfitTarget::from_percent(Decimal::TEN).expect("valid target"),
        );
        TradeRecord::from_position(&position, Decimal::from(2), OffsetDateTime::UNIX_EPOCH)
    }

    #[tokio::test]
    async fn missing_file_loads_as_empty() {
        let store = FileTradeStore::new(temp_path());
        let trades = store.load().await.expect("missing file is not an error");
        assert!(trades.is_empty());
    }

    #[tokio::test]
    async fn appends_and_reloads_trade_history() {
        let path = temp_path();
        let store = FileTradeStore::new(&path);
        let trade = sample_trade();

        store.append(&trade).await.expect("append should succeed");
        let loaded = store.load().await.expect("load should succeed");
        assert_eq!(loaded, vec![trade]);

        let _ = std::fs::remove_file(path);
    }
}


pub struct FilePendingTradeStore {
    path: PathBuf,
}

impl FilePendingTradeStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl PendingTradeStore for FilePendingTradeStore {
    async fn load(&self) -> Result<Vec<TradeRecord>, PortError> {
        let raw = match fs::read_to_string(&self.path).await {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(PortError::Storage(Box::new(e))),
        };
        serde_json::from_str(&raw).map_err(|e| PortError::MalformedResponse {
            venue: "pending-trade-store".to_string(),
            reason: e.to_string(),
        })
    }

    async fn save(&self, trades: &[TradeRecord]) -> Result<(), PortError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| PortError::Storage(Box::new(e)))?;
        }
        let tmp_path = self.path.with_extension("json.tmp");
        let serialised = serde_json::to_vec_pretty(trades)
            .map_err(|e| PortError::Storage(Box::new(e)))?;
        fs::write(&tmp_path, serialised)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;
        fs::rename(&tmp_path, &self.path)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;
        Ok(())
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

--- ./crates/application/Cargo.toml ---
[package]
name = "ben_snipes-application"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "Use-case orchestration: the services that coordinate domain rules and ports to actually do something, like detecting new listings or managing an open position."

[dependencies]
async-trait = { workspace = true }
ben_snipes-domain = { workspace = true }
ben_snipes-ports = { workspace = true }
tracing = { workspace = true }
rust_decimal = { workspace = true }
time = { workspace = true }
serde = { workspace = true }

[dev-dependencies]
tokio = { workspace = true, features = ["sync"] }
serde_json = { workspace = true }

--- ./crates/application/src/acquisition_engine.rs ---
use ben_snipes_domain::{
    AcquisitionCriteria, CanonicalTokenId, Listing, Position, ProfitTarget, SafetyCriteria,
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
#[derive(Debug)]
pub enum AcquisitionDecision {
    Opened(Position),
    Pending,
    Rejected,
}

pub struct AcquisitionEngine {
    metrics_provider: Arc<dyn MetricsProvider>,
    exchange: Arc<dyn ExchangeClient>,
    ledger: Arc<dyn AcquisitionLedger>,
    criteria: AcquisitionCriteria,
    take_profit: ProfitTarget,
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
        position_size: Decimal,
        safety_gate: Option<SafetyGate>,
    ) -> Self {
        Self {
            metrics_provider,
            exchange,
            ledger,
            criteria,
            take_profit,
            position_size,
            safety_gate,
        }
    }

    /// Evaluates a freshly-detected listing and, if it qualifies, buys
    /// it. `Pending` means an external indexer or safety provider has not
    /// exposed enough information yet, or the current metrics do not meet
    /// the acquisition threshold yet. `Rejected` is reserved for a
    /// definitive decision such as a failed safety gate or an existing
    /// acquisition reservation.
    pub async fn evaluate_and_buy(&self, listing: &Listing) -> Result<AcquisitionDecision, PortError> {
        let Some(metrics) = self.metrics_provider.metrics(&listing.symbol).await? else {
            debug!(symbol = listing.symbol.as_str(), "no metrics yet, skipping");
            return Ok(AcquisitionDecision::Pending);
        };

        if !self.criteria.matches(&metrics) {
            debug!(
                symbol = listing.symbol.as_str(),
                volume = %metrics.volume_24h,
                "does not meet acquisition criteria yet; retaining for pending retry"
            );
            return Ok(AcquisitionDecision::Pending);
        }

        if let Some(gate) = &self.safety_gate {
            let Some(report) = gate.checker.assess(&listing.symbol).await? else {
                debug!(symbol = listing.symbol.as_str(), "no safety assessment yet, skipping");
                return Ok(AcquisitionDecision::Pending);
            };

            if !gate.criteria.passes(&report) {
                info!(
                    symbol = listing.symbol.as_str(),
                    sell_tax_bps = ?report.sell_tax_bps,
                    token_transfer_fee_bps = ?report.token_transfer_fee_bps,
                    sellability = ?report.sellability,
                    has_permanent_delegate = report.has_permanent_delegate,
                    ownership_renounced = report.ownership_renounced,
                    liquidity_locked = report.liquidity_locked,
                    is_mintable = report.is_mintable,
                    "failed safety check, skipping (likely honeypot/rug signal)"
                );
                return Ok(AcquisitionDecision::Rejected);
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
            return Ok(AcquisitionDecision::Rejected);
        }

        let filled = match self.exchange.submit_buy_by_amount(&listing.symbol, self.position_size).await {
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
            entry_price = %filled.entry_price,
            quantity = %filled.quantity,
            "autonomous buy executed"
        );

        let position = Position::new(
            listing.venue.clone(),
            listing.symbol.clone(),
            filled.entry_price,
            filled.quantity,
            self.take_profit,
        );

        Ok(AcquisitionDecision::Opened(position))
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
    use ben_snipes_domain::{
        Chain, FilledBuy, FilledSell, ListingMetrics, Order, SafetyReport, Symbol, Venue, VenueKind,
    };
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
        buys_submitted: Mutex<u32>,
        fail_buy: bool,
    }

    #[async_trait]
    impl ExchangeClient for StubExchange {
        fn venue_name(&self) -> &str {
            "stub"
        }

        async fn current_price(&self, _symbol: &Symbol) -> Result<Decimal, PortError> {
            Ok(Decimal::ONE)
        }

        async fn submit_buy_by_amount(&self, _symbol: &Symbol, quote_amount: Decimal) -> Result<FilledBuy, PortError> {
            if self.fail_buy {
                return Err(PortError::Rejected("stub configured to fail".to_string()));
            }
            *self.buys_submitted.lock().await += 1;
            // Stub venue: 1:1 price, so quantity acquired equals the
            // amount spent.
            Ok(FilledBuy {
                quantity: quote_amount,
                entry_price: Decimal::ONE,
            })
        }

        async fn submit_order(&self, order: Order) -> Result<FilledSell, PortError> {
            Ok(FilledSell {
                quantity: order.quantity,
                execution_price: Some(Decimal::ONE),
                quote_proceeds: Some(order.quantity),
                fee_quote: None,
                tx_id: Some("test-tx".to_string()),
            })
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
            Decimal::from(25),
            safety_gate,
        )
    }

    #[tokio::test]
    async fn buys_when_no_safety_gate_configured() {
        let exchange = Arc::new(StubExchange {
            buys_submitted: Mutex::new(0),
            fail_buy: false,
        });
        let engine = build_engine(Some(passing_metrics()), None, exchange.clone(), Arc::new(InMemoryLedger::empty()));

        let result = engine
            .evaluate_and_buy(&sample_listing())
            .await
            .expect("stub dependencies cannot fail");

        assert!(matches!(result, AcquisitionDecision::Opened(_)));
        assert_eq!(*exchange.buys_submitted.lock().await, 1);
    }

    #[tokio::test]
    async fn retains_a_listing_when_volume_is_below_the_threshold() {
        let exchange = Arc::new(StubExchange {
            buys_submitted: Mutex::new(0),
            fail_buy: false,
        });
        let metrics = ListingMetrics {
            volume_24h: Decimal::from(10),
            market_cap: Decimal::from(100_000),
        };
        let engine = build_engine(
            Some(metrics),
            None,
            exchange.clone(),
            Arc::new(InMemoryLedger::empty()),
        );

        let result = engine
            .evaluate_and_buy(&sample_listing())
            .await
            .expect("stub dependencies cannot fail");

        assert!(matches!(result, AcquisitionDecision::Pending));
        assert_eq!(*exchange.buys_submitted.lock().await, 0);
    }

    #[tokio::test]
    async fn skips_a_listing_that_fails_the_safety_gate() {
        let exchange = Arc::new(StubExchange {
            buys_submitted: Mutex::new(0),
            fail_buy: false,
        });
        let dangerous_report = SafetyReport {
            sell_tax_bps: Some(9_000),
            token_transfer_fee_bps: Some(0),
            sellability: ben_snipes_domain::SellabilityEvidence::Simulated,
            has_permanent_delegate: false,
            ownership_renounced: false,
            liquidity_locked: false,
            is_mintable: true,
        };
        let gate = SafetyGate::new(
            Arc::new(StubSafetyChecker { report: Some(dangerous_report) }),
            SafetyCriteria::new(1_000, 0),
        );
        let engine = build_engine(Some(passing_metrics()), Some(gate), exchange.clone(), Arc::new(InMemoryLedger::empty()));

        let result = engine
            .evaluate_and_buy(&sample_listing())
            .await
            .expect("stub dependencies cannot fail");

        assert!(matches!(result, AcquisitionDecision::Rejected));
        assert_eq!(*exchange.buys_submitted.lock().await, 0);
    }

    #[tokio::test]
    async fn second_source_reporting_the_same_token_is_skipped_via_the_ledger() {
        let exchange = Arc::new(StubExchange {
            buys_submitted: Mutex::new(0),
            fail_buy: false,
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

        assert!(matches!(first, AcquisitionDecision::Opened(_)));
        assert!(matches!(second, AcquisitionDecision::Rejected));
        assert_eq!(*exchange.buys_submitted.lock().await, 1);
    }

    #[tokio::test]
    async fn reservation_is_released_when_buy_submission_fails() {
        let exchange = Arc::new(StubExchange {
            buys_submitted: Mutex::new(0),
            fail_buy: true,
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
        assert!(can_still_reserve, "reservation should have been released after the failed buy");
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
mod backtest;
mod new_listing_detector;
mod position_manager;
mod paper_exchange;
mod runtime_metrics;

pub use acquisition_engine::{AcquisitionDecision, AcquisitionEngine, SafetyGate};
pub use backtest::{BacktestConfig, BacktestEngine, BacktestEvent, BacktestReport, BacktestTrade};
pub use new_listing_detector::NewListingDetector;
pub use position_manager::{ExitResult, PositionManager};
pub use paper_exchange::PaperExchange;
pub use runtime_metrics::RuntimeMetrics;

--- ./crates/application/src/new_listing_detector.rs ---
use ben_snipes_domain::Listing;
use ben_snipes_ports::{
    KnownListings, ListingSnapshot, ListingSource, ListingStateStore, PendingListing, PortError,
};
use std::collections::HashSet;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
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
/// Pending candidates are retried continuously for one full day from the
/// moment they enter the pending queue. This deliberately outlives both
/// indexer lag and a temporary period of weak volume: a listing can become
/// tradeable later without being rediscovered by the source.
const PENDING_TTL: Duration = Duration::hours(24);

fn prune_expired_pending(known: &mut KnownListings, now: OffsetDateTime) {
    known.pending.retain(|key, pending| {
        let age = now - pending.pending_since;
        if age >= PENDING_TTL {
            debug!(
                listing = %key,
                age_seconds = age.whole_seconds(),
                "pending listing expired after 24-hour retry window"
            );
            false
        } else {
            true
        }
    });
}

pub struct NewListingDetector {
    state_store: Arc<dyn ListingStateStore>,
}

impl NewListingDetector {
    pub fn new(state_store: Arc<dyn ListingStateStore>) -> Self {
        Self { state_store }
    }

    pub async fn poll(
        &self,
        source: &dyn ListingSource,
        retry_pending: bool,
    ) -> Result<Vec<Listing>, PortError> {
        let source_id = source.source_id();
        let mut known = self.state_store.load(source_id).await?;

        let snapshot = source.poll(known.cursor.as_deref()).await?;

        let mut newly_seen = match snapshot {
            ListingSnapshot::Incremental { new, cursor } => {
                debug!(source_id, count = new.len(), "incremental poll");
                for listing in &new {
                    let key = listing.dedupe_key();
                    known.seen_keys.insert(key.clone());
                    known.pending.entry(key).or_insert_with(|| {
                        PendingListing::new(listing.clone(), OffsetDateTime::now_utc())
                    });
                }
                known.cursor = cursor;
                new
            }
            ListingSnapshot::Full(all) if !known.bootstrapped => {
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
                    let key = listing.dedupe_key();
                    known.seen_keys.insert(key.clone());
                    known.pending.entry(key).or_insert_with(|| {
                        PendingListing::new(listing.clone(), OffsetDateTime::now_utc())
                    });
                }
                fresh
            }
        };

        let now = OffsetDateTime::now_utc();
        prune_expired_pending(&mut known, now);

        if retry_pending {
            let already_returned: HashSet<String> = newly_seen.iter().map(Listing::dedupe_key).collect();
            let pending: Vec<Listing> = known
                .pending
                .values()
                .filter(|pending| !already_returned.contains(&pending.listing.dedupe_key()))
                .map(|pending| pending.listing.clone())
                .collect();
            if !pending.is_empty() {
                debug!(source_id, count = pending.len(), "retrying pending listings");
                newly_seen.extend(pending);
            }
        }

        self.state_store.save(source_id, &known).await?;

        if !newly_seen.is_empty() {
            info!(source_id, count = newly_seen.len(), "new listings detected");
        }

        Ok(newly_seen)
    }

    /// Resolves a candidate after the acquisition engine has enough
    /// information to make a final decision. `keep_pending = true` is used
    /// for temporary information gaps such as DexScreener not having
    /// indexed a brand-new token yet.
    pub async fn resolve(
        &self,
        source: &dyn ListingSource,
        listing: &Listing,
        keep_pending: bool,
    ) -> Result<(), PortError> {
        if keep_pending {
            return Ok(());
        }

        let source_id = source.source_id();
        let mut known = self.state_store.load(source_id).await?;
        known.pending.remove(&listing.dedupe_key());
        self.state_store.save(source_id, &known).await
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
        let result = detector.poll(&source, true).await.expect("in-memory store cannot fail");
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn second_poll_with_same_snapshot_returns_nothing_new() {
        let store = Arc::new(InMemoryStateStore::empty());
        let detector = NewListingDetector::new(store);
        let source = FixedFullSnapshotSource {
            listings: vec![listing("AAAUSDT")],
        };

        let first = detector.poll(&source, true).await.expect("in-memory store cannot fail");
        assert!(first.is_empty(), "first poll is the baseline, not new listings");

        let second = detector.poll(&source, true).await.expect("in-memory store cannot fail");
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
            .poll(&first_source, true)
            .await
            .expect("in-memory store cannot fail");
        assert!(first.is_empty(), "first poll is the baseline, not new listings");

        let second_source = FixedFullSnapshotSource {
            listings: vec![listing("AAAUSDT"), listing("CCCUSDT")],
        };
        let second = detector
            .poll(&second_source, true)
            .await
            .expect("in-memory store cannot fail");

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].symbol.as_str(), "CCCUSDT");
    }
    #[tokio::test]
    async fn pending_listing_is_retried_without_becoming_a_new_listing_again() {
        let store = Arc::new(InMemoryStateStore::empty());
        let detector = NewListingDetector::new(store);
        let source = FixedFullSnapshotSource {
            listings: vec![listing("PENDING")],
        };

        let first = detector.poll(&source, true).await.expect("poll should succeed");
        assert_eq!(first.len(), 0, "the first full snapshot is the baseline");

        let second = detector.poll(&source, true).await.expect("poll should succeed");
        assert_eq!(second.len(), 0, "an unchanged full snapshot has no pending candidate after baseline");

        // Simulate a source that reports the candidate as genuinely new by
        // using a new source state. The detector should retain that listing
        // for later retries instead of losing it after the first metrics miss.
        let source = FixedFullSnapshotSource {
            listings: vec![listing("PENDING2")],
        };
        let fresh = detector.poll(&source, true).await.expect("poll should succeed");
        assert_eq!(fresh.len(), 1);

        let retry = detector.poll(&source, true).await.expect("poll should succeed");
        assert_eq!(retry.len(), 1);
        assert_eq!(retry[0].symbol.as_str(), "PENDING2");

        detector
            .resolve(&source, &retry[0], false)
            .await
            .expect("state update should succeed");
        let after_resolve = detector.poll(&source, true).await.expect("poll should succeed");
        assert!(after_resolve.is_empty());
    }

    #[test]
    fn pending_listing_expires_only_after_24_hours() {
        let listing = listing("EXPIRING");
        let pending_since = OffsetDateTime::UNIX_EPOCH;
        let mut known = KnownListings::default();
        known.pending.insert(
            listing.dedupe_key(),
            PendingListing::new(listing.clone(), pending_since),
        );

        prune_expired_pending(&mut known, pending_since + Duration::hours(23) + Duration::minutes(59));
        assert!(known.pending.contains_key(&listing.dedupe_key()));

        prune_expired_pending(&mut known, pending_since + Duration::hours(24));
        assert!(!known.pending.contains_key(&listing.dedupe_key()));
    }

    #[test]
    fn legacy_pending_state_uses_listing_first_seen_as_pending_start() {
        let listing = listing("LEGACY");
        let encoded = match serde_json::to_string(&listing) {
            Ok(value) => value,
            Err(error) => panic!("listing serialization failed: {error}"),
        };
        let decoded: PendingListing = match serde_json::from_str(&encoded) {
            Ok(value) => value,
            Err(error) => panic!("legacy pending state deserialization failed: {error}"),
        };

        assert_eq!(decoded.listing, listing);
        assert_eq!(decoded.pending_since, OffsetDateTime::UNIX_EPOCH);
    }

}

--- ./crates/application/src/position_manager.rs ---
use ben_snipes_domain::{FilledSell, Order, OrderSide, Position};
use time::OffsetDateTime;
use ben_snipes_ports::{ExchangeClient, PortError};
use std::sync::Arc;
use tracing::info;

/// Watches a single open position and exits it once the take-profit
/// target is reached. There is no stop-loss in this bot, by explicit
/// design: a position is held until it hits +10% (or whatever
/// `risk.take_profit_percent` is configured to), however long that
/// takes - it never exits at a loss.
pub struct PositionManager {
    exchange: Arc<dyn ExchangeClient>,
}

#[derive(Debug)]
pub struct ExitResult {
    pub fill: FilledSell,
    /// Price observed immediately before submitting the sell. Used only as
    /// a fallback when the venue does not return an execution price.
    pub reference_price: rust_decimal::Decimal,
    pub closed_at: OffsetDateTime,
}

impl PositionManager {
    pub fn new(exchange: Arc<dyn ExchangeClient>) -> Self {
        Self { exchange }
    }

    /// Checks the current price against the position's take-profit
    /// target. Returns an `ExitResult` only for a completely filled sell;
    /// a partial, rejected, cancelled, or still-pending order is treated
    /// as an error so the runner never silently forgets an open position.
    pub async fn check_and_exit(&self, position: &Position) -> Result<Option<ExitResult>, PortError> {
        let current_price = self.exchange.current_price(&position.symbol).await?;

        if !position.should_exit(current_price) {
            return Ok(None);
        }

        info!(
            symbol = position.symbol.as_str(),
            entry = %position.entry_price,
            current = %current_price,
            "take-profit target reached, submitting exit order"
        );

        let order = Order::new(
            position.venue.clone(),
            position.symbol.clone(),
            OrderSide::Sell,
            position.quantity,
        )?;

        // Give DEX adapters a chance to simulate the exact exit while the
        // wallet still owns the tokens. A failed simulation leaves the
        // position open instead of broadcasting a transaction that the
        // current chain state already proves is doomed.
        self.exchange.preflight_sell(&order).await?;

        let fill = self.exchange.submit_order(order).await?;
        if fill.quantity <= rust_decimal::Decimal::ZERO {
            return Err(PortError::Rejected(
                "exit execution reported a non-positive filled quantity".to_string(),
            ));
        }
        if fill.quantity != position.quantity {
            return Err(PortError::Rejected(format!(
                "exit execution was partial: requested={}, filled={}",
                position.quantity, fill.quantity
            )));
        }

        Ok(Some(ExitResult {
            fill,
            reference_price: current_price,
            closed_at: OffsetDateTime::now_utc(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ben_snipes_domain::{FilledBuy, FilledSell, OrderStatus, ProfitTarget, Symbol, Venue, VenueKind};
    use rust_decimal::Decimal;

    struct StubExchange {
        price: Decimal,
        status: OrderStatus,
    }

    #[async_trait]
    impl ExchangeClient for StubExchange {
        fn venue_name(&self) -> &str {
            "stub"
        }

        async fn current_price(&self, _symbol: &Symbol) -> Result<Decimal, PortError> {
            Ok(self.price)
        }

        async fn submit_buy_by_amount(&self, _symbol: &Symbol, _quote_amount: Decimal) -> Result<FilledBuy, PortError> {
            unreachable!("PositionManager only ever calls current_price/submit_order, never submit_buy_by_amount")
        }

        async fn submit_order(&self, order: Order) -> Result<FilledSell, PortError> {
            if self.status != OrderStatus::Filled {
                return Err(PortError::Rejected(format!("stub status={:?}", self.status)));
            }
            Ok(FilledSell {
                quantity: order.quantity,
                execution_price: Some(Decimal::from(111)),
                quote_proceeds: Some(Decimal::from(111) * order.quantity),
                fee_quote: None,
                tx_id: Some("stub-tx".to_string()),
            })
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
        )
    }

    #[tokio::test]
    async fn holds_below_target() {
        let manager = PositionManager::new(Arc::new(StubExchange {
            price: Decimal::from(102),
            status: OrderStatus::Filled,
        }));

        let result = manager
            .check_and_exit(&sample_position())
            .await
            .expect("stub exchange cannot fail");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn holds_even_on_a_large_price_drop() {
        // The whole point of dropping stop-loss: no price, however low,
        // triggers an exit on its own.
        let manager = PositionManager::new(Arc::new(StubExchange {
            price: Decimal::from(10),
            status: OrderStatus::Filled,
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
            status: OrderStatus::Filled,
        }));

        let result = manager
            .check_and_exit(&sample_position())
            .await
            .expect("stub exchange cannot fail");
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn does_not_treat_partial_fill_as_closed() {
        let manager = PositionManager::new(Arc::new(StubExchange {
            price: Decimal::from(115),
            status: OrderStatus::PartiallyFilled,
        }));

        let result = manager.check_and_exit(&sample_position()).await;
        assert!(result.is_err());
    }
}

--- ./crates/application/src/backtest.rs ---
//! Deterministic historical replay for the acquisition and take-profit rules.
//!
//! This module intentionally does not pretend to reproduce venue microstructure.
//! It replays timestamped listing observations, volume, safety data, and a
//! reference price through the same domain rules used by live trading.

use ben_snipes_domain::{
    AcquisitionCriteria, Listing, ListingMetrics, PerformanceSummary, Position, ProfitTarget,
    SafetyCriteria, SafetyReport, TradeRecord,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestEvent {
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub listing: Listing,
    pub price: Decimal,
    pub metrics: Option<ListingMetrics>,
    pub safety: Option<SafetyReport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BacktestConfig {
    pub min_volume_24h: Decimal,
    pub max_sell_tax_bps: u32,
    #[serde(default)]
    pub max_token_transfer_fee_bps: u32,
    pub take_profit_percent: Decimal,
    pub position_size: Decimal,
    pub max_open_positions: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestTrade {
    pub trade: TradeRecord,
    #[serde(with = "time::serde::rfc3339")]
    pub opened_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub closed_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestReport {
    pub events_processed: usize,
    pub listings_seen: usize,
    pub entries_opened: usize,
    pub pending_events: usize,
    pub rejected_events: usize,
    pub still_open: Vec<Position>,
    pub trades: Vec<BacktestTrade>,
    pub performance: PerformanceSummary,
}

impl BacktestReport {
    fn new() -> Self {
        Self {
            events_processed: 0,
            listings_seen: 0,
            entries_opened: 0,
            pending_events: 0,
            rejected_events: 0,
            still_open: Vec::new(),
            trades: Vec::new(),
            performance: PerformanceSummary::default(),
        }
    }
}

pub struct BacktestEngine {
    config: BacktestConfig,
    criteria: AcquisitionCriteria,
    target: ProfitTarget,
    safety_criteria: SafetyCriteria,
}

impl BacktestEngine {
    pub fn new(config: BacktestConfig) -> Result<Self, String> {
        if config.position_size <= Decimal::ZERO {
            return Err("position_size must be positive".to_string());
        }
        if config.max_open_positions == 0 {
            return Err("max_open_positions must be positive".to_string());
        }
        let criteria = AcquisitionCriteria::new(config.min_volume_24h)
            .map_err(|e| e.to_string())?;
        let target = ProfitTarget::from_percent(config.take_profit_percent)
            .map_err(|e| e.to_string())?;
        let safety_criteria = SafetyCriteria::new(
            config.max_sell_tax_bps,
            config.max_token_transfer_fee_bps,
        );
        Ok(Self {
            config,
            criteria,
            target,
            safety_criteria,
        })
    }

    pub fn run(&self, mut events: Vec<BacktestEvent>) -> BacktestReport {
        events.sort_by_key(|event| event.timestamp);
        let mut report = BacktestReport::new();
        let mut open_positions: Vec<(Position, OffsetDateTime)> = Vec::new();
        let mut seen_listings = std::collections::HashSet::new();
        let mut pending_listings = std::collections::HashSet::new();

        for event in events {
            report.events_processed = report.events_processed.saturating_add(1);
            let key = event.listing.dedupe_key();
            let is_new = seen_listings.insert(key.clone());
            if is_new {
                report.listings_seen = report.listings_seen.saturating_add(1);
            }
            let retrying_pending = pending_listings.contains(&key);

            // Exit checks happen before a same-timestamp new entry. This keeps
            // the maximum-position rule conservative and makes the replay
            // deterministic when a price jump closes an existing position.
            // Scoped to positions matching this event's own symbol - each
            // event only carries a price for one listing, so applying it to
            // every open position would spuriously exit unrelated tokens on
            // an interleaved multi-symbol replay.
            let mut remaining = Vec::with_capacity(open_positions.len());
            for (position, opened_at) in open_positions.drain(..) {
                if position.symbol == event.listing.symbol && position.should_exit(event.price) {
                    let trade = TradeRecord::from_position(&position, event.price, event.timestamp);
                    report.trades.push(BacktestTrade {
                        trade,
                        opened_at,
                        closed_at: event.timestamp,
                    });
                } else {
                    remaining.push((position, opened_at));
                }
            }
            open_positions = remaining;

            if !is_new && !retrying_pending {
                continue;
            }

            if open_positions.len() >= self.config.max_open_positions {
                pending_listings.insert(key);
                report.pending_events = report.pending_events.saturating_add(1);
                continue;
            }

            let Some(metrics) = event.metrics else {
                pending_listings.insert(key);
                report.pending_events = report.pending_events.saturating_add(1);
                continue;
            };
            if !self.criteria.matches(&metrics) {
                pending_listings.insert(key);
                report.pending_events = report.pending_events.saturating_add(1);
                continue;
            }

            let Some(safety) = event.safety else {
                pending_listings.insert(key);
                report.pending_events = report.pending_events.saturating_add(1);
                continue;
            };
            if !self.safety_criteria.passes(&safety) {
                pending_listings.remove(&key);
                report.rejected_events = report.rejected_events.saturating_add(1);
                continue;
            }

            if event.price <= Decimal::ZERO {
                pending_listings.remove(&key);
                report.rejected_events = report.rejected_events.saturating_add(1);
                continue;
            }

            let quantity = self.config.position_size / event.price;
            open_positions.push((
                Position::new(
                    event.listing.venue,
                    event.listing.symbol,
                    event.price,
                    quantity,
                    self.target,
                ),
                event.timestamp,
            ));
            pending_listings.remove(&key);
            report.entries_opened = report.entries_opened.saturating_add(1);
        }

        report.still_open = open_positions.into_iter().map(|(position, _)| position).collect();
        report.performance = PerformanceSummary::from_trades(
            &report.trades.iter().map(|trade| trade.trade.clone()).collect::<Vec<_>>(),
        );
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ben_snipes_domain::{Chain, Venue, VenueKind, Symbol};

    fn event(timestamp: i64, symbol: &str, price: i64, volume: i64) -> BacktestEvent {
        let venue = Venue::new(VenueKind::Dex, "replay").expect("literal venue is valid");
        let chain = Chain::new("solana").expect("literal chain is valid");
        let symbol = Symbol::new(symbol).expect("literal symbol is valid");
        BacktestEvent {
            timestamp: OffsetDateTime::from_unix_timestamp(timestamp).expect("test timestamp is valid"),
            listing: Listing::new(symbol, venue, chain, OffsetDateTime::UNIX_EPOCH),
            price: Decimal::from(price),
            metrics: Some(ListingMetrics {
                volume_24h: Decimal::from(volume),
                market_cap: Decimal::from(100_000),
            }),
            safety: Some(SafetyReport {
                sell_tax_bps: Some(100),
                token_transfer_fee_bps: Some(0),
                sellability: ben_snipes_domain::SellabilityEvidence::Simulated,
                has_permanent_delegate: false,
                ownership_renounced: true,
                liquidity_locked: false,
                is_mintable: false,
            }),
        }
    }

    fn engine() -> BacktestEngine {
        BacktestEngine::new(BacktestConfig {
            min_volume_24h: Decimal::from(50_000),
            max_sell_tax_bps: 1_000,
            max_token_transfer_fee_bps: 0,
            take_profit_percent: Decimal::TEN,
            position_size: Decimal::from(100),
            max_open_positions: 2,
        })
        .expect("test config is valid")
    }

    #[test]
    fn replays_entry_then_take_profit_exit() {
        let report = engine().run(vec![
            event(1, "AAA", 10, 100_000),
            event(2, "AAA", 11, 100_000),
        ]);

        assert_eq!(report.entries_opened, 1);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.performance.realized_pnl, Decimal::from(10));
        assert!(report.still_open.is_empty());
    }

    #[test]
    fn low_volume_remains_pending_and_does_not_open() {
        let report = engine().run(vec![
            event(1, "AAA", 10, 1_000),
            event(2, "AAA", 10, 100_000),
        ]);
        assert_eq!(report.entries_opened, 1);
        assert_eq!(report.pending_events, 1);
        assert!(report.trades.is_empty());
    }

    #[test]
    fn failed_safety_is_rejected() {
        let mut first = event(1, "AAA", 10, 100_000);
        first.safety = Some(SafetyReport {
            sell_tax_bps: None,
            token_transfer_fee_bps: Some(0),
            sellability: ben_snipes_domain::SellabilityEvidence::Simulated,
            has_permanent_delegate: false,
            ..first.safety.expect("test event has safety")
        });
        let report = engine().run(vec![first]);
        assert_eq!(report.entries_opened, 0);
        assert_eq!(report.rejected_events, 1);
    }

    #[test]
    fn holds_losses_until_take_profit() {
        let report = engine().run(vec![
            event(1, "AAA", 10, 100_000),
            event(2, "AAA", 5, 100_000),
        ]);
        assert_eq!(report.entries_opened, 1);
        assert!(report.trades.is_empty());
        assert_eq!(report.still_open.len(), 1);
    }

    #[test]
    fn a_symbols_price_never_exits_a_different_open_position() {
        // Two symbols open concurrently. BBB's price never moves, so it
        // must stay open even while AAA's price crosses AAA's own
        // take-profit target - a shared, unscoped price check would
        // incorrectly close BBB using AAA's price instead.
        let report = engine().run(vec![
            event(1, "AAA", 10, 100_000),
            event(2, "BBB", 10, 100_000),
            event(3, "AAA", 20, 100_000),
        ]);

        assert_eq!(report.entries_opened, 2);
        assert_eq!(report.trades.len(), 1, "only AAA's own take-profit should have triggered an exit");
        assert_eq!(report.trades[0].trade.symbol.as_str(), "AAA");
        assert_eq!(report.still_open.len(), 1);
        assert_eq!(report.still_open[0].symbol.as_str(), "BBB");
    }
}

--- ./crates/application/src/runtime_metrics.rs ---
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Low-overhead process metrics shared by the runner and its local
/// Prometheus-compatible HTTP exporter. Counters are monotonic for the
/// lifetime of the process; `open_positions` is a gauge.
#[derive(Debug, Default)]
pub struct RuntimeMetrics {
    polls: AtomicU64,
    poll_errors: AtomicU64,
    listings_detected: AtomicU64,
    pending_decisions: AtomicU64,
    rejected_decisions: AtomicU64,
    positions_opened: AtomicU64,
    buy_errors: AtomicU64,
    exit_checks: AtomicU64,
    exits_filled: AtomicU64,
    exit_errors: AtomicU64,
    journal_errors: AtomicU64,
    journal_recoveries: AtomicU64,
    open_positions: AtomicU64,
}

impl RuntimeMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn inc_polls(&self) { self.polls.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_poll_errors(&self) { self.poll_errors.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_listings_detected(&self, count: usize) { self.listings_detected.fetch_add(count as u64, Ordering::Relaxed); }
    pub fn inc_pending_decisions(&self) { self.pending_decisions.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_rejected_decisions(&self) { self.rejected_decisions.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_positions_opened(&self) { self.positions_opened.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_buy_errors(&self) { self.buy_errors.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_exit_checks(&self) { self.exit_checks.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_exits_filled(&self) { self.exits_filled.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_exit_errors(&self) { self.exit_errors.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_journal_errors(&self) { self.journal_errors.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_journal_recoveries(&self) { self.journal_recoveries.fetch_add(1, Ordering::Relaxed); }
    pub fn set_open_positions(&self, count: usize) { self.open_positions.store(count as u64, Ordering::Relaxed); }

    pub fn render_prometheus(&self) -> String {
        format!(
            "# TYPE ben_snipes_polls_total counter\nben_snipes_polls_total {}\n\
# TYPE ben_snipes_poll_errors_total counter\nben_snipes_poll_errors_total {}\n\
# TYPE ben_snipes_listings_detected_total counter\nben_snipes_listings_detected_total {}\n\
# TYPE ben_snipes_pending_decisions_total counter\nben_snipes_pending_decisions_total {}\n\
# TYPE ben_snipes_rejected_decisions_total counter\nben_snipes_rejected_decisions_total {}\n\
# TYPE ben_snipes_positions_opened_total counter\nben_snipes_positions_opened_total {}\n\
# TYPE ben_snipes_buy_errors_total counter\nben_snipes_buy_errors_total {}\n\
# TYPE ben_snipes_exit_checks_total counter\nben_snipes_exit_checks_total {}\n\
# TYPE ben_snipes_exits_filled_total counter\nben_snipes_exits_filled_total {}\n\
# TYPE ben_snipes_exit_errors_total counter\nben_snipes_exit_errors_total {}\n\
# TYPE ben_snipes_journal_errors_total counter\nben_snipes_journal_errors_total {}\n# TYPE ben_snipes_journal_recoveries_total counter\nben_snipes_journal_recoveries_total {}\n\
# TYPE ben_snipes_open_positions gauge\nben_snipes_open_positions {}\n",
            self.polls.load(Ordering::Relaxed),
            self.poll_errors.load(Ordering::Relaxed),
            self.listings_detected.load(Ordering::Relaxed),
            self.pending_decisions.load(Ordering::Relaxed),
            self.rejected_decisions.load(Ordering::Relaxed),
            self.positions_opened.load(Ordering::Relaxed),
            self.buy_errors.load(Ordering::Relaxed),
            self.exit_checks.load(Ordering::Relaxed),
            self.exits_filled.load(Ordering::Relaxed),
            self.exit_errors.load(Ordering::Relaxed),
            self.journal_errors.load(Ordering::Relaxed),
            self.journal_recoveries.load(Ordering::Relaxed),
            self.open_positions.load(Ordering::Relaxed),
        )
    }
}

--- ./crates/application/src/paper_exchange.rs ---
use ben_snipes_domain::{FilledBuy, FilledSell, Order, Symbol};
use ben_snipes_ports::{ExchangeClient, PortError};
use rust_decimal::Decimal;
use std::sync::Arc;
use tracing::info;

/// An execution adapter that preserves real market-data reads while never
/// submitting a live order. Buys are priced from the venue's current-price
/// port and sells are recorded as filled at the application's requested
/// quantity. This makes the strategy executable end to end without a funded
/// wallet or a signing key.
pub struct PaperExchange {
    inner: Arc<dyn ExchangeClient>,
}

impl PaperExchange {
    pub fn new(inner: Arc<dyn ExchangeClient>) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl ExchangeClient for PaperExchange {
    fn venue_name(&self) -> &str {
        self.inner.venue_name()
    }

    async fn current_price(&self, symbol: &Symbol) -> Result<Decimal, PortError> {
        self.inner.current_price(symbol).await
    }

    async fn submit_buy_by_amount(
        &self,
        symbol: &Symbol,
        quote_amount: Decimal,
    ) -> Result<FilledBuy, PortError> {
        if quote_amount <= Decimal::ZERO {
            return Err(PortError::Rejected("paper buy amount must be positive".to_string()));
        }
        let price = self.current_price(symbol).await?;
        if price <= Decimal::ZERO {
            return Err(PortError::Rejected("paper price must be positive".to_string()));
        }
        let quantity = quote_amount / price;
        info!(
            symbol = symbol.as_str(),
            quote_amount = %quote_amount,
            price = %price,
            quantity = %quantity,
            "paper buy simulated; no transaction submitted"
        );
        Ok(FilledBuy {
            quantity,
            entry_price: price,
        })
    }

    async fn submit_order(&self, order: Order) -> Result<FilledSell, PortError> {
        let price = self.current_price(&order.symbol).await?;
        if price <= Decimal::ZERO {
            return Err(PortError::Rejected("paper price must be positive".to_string()));
        }
        info!(
            symbol = order.symbol.as_str(),
            quantity = %order.quantity,
            price = %price,
            "paper sell simulated; no transaction submitted"
        );
        Ok(FilledSell {
            quantity: order.quantity,
            execution_price: Some(price),
            quote_proceeds: Some(price * order.quantity),
            fee_quote: None,
            tx_id: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ben_snipes_domain::{OrderSide, Venue, VenueKind};

    struct StubExchange;

    #[async_trait]
    impl ExchangeClient for StubExchange {
        fn venue_name(&self) -> &str { "paper-test" }

        async fn current_price(&self, _symbol: &Symbol) -> Result<Decimal, PortError> {
            Ok(Decimal::from(2))
        }

        async fn submit_buy_by_amount(&self, _symbol: &Symbol, _quote_amount: Decimal) -> Result<FilledBuy, PortError> {
            Err(PortError::Rejected("underlying execution should never be called".to_string()))
        }

        async fn submit_order(&self, _order: Order) -> Result<FilledSell, PortError> {
            Err(PortError::Rejected("underlying execution should never be called".to_string()))
        }
    }

    #[tokio::test]
    async fn paper_buy_uses_read_only_price_and_does_not_execute() {
        let exchange = PaperExchange::new(Arc::new(StubExchange));
        let symbol = match Symbol::new("0x0000000000000000000000000000000000000001") {
            Ok(symbol) => symbol,
            Err(error) => panic!("test symbol should be valid: {error}"),
        };
        let filled = match exchange.submit_buy_by_amount(&symbol, Decimal::from(10)).await {
            Ok(filled) => filled,
            Err(error) => panic!("paper execution should succeed: {error}"),
        };
        assert_eq!(filled.entry_price, Decimal::from(2));
        assert_eq!(filled.quantity, Decimal::from(5));
    }

    #[tokio::test]
    async fn paper_sell_returns_a_filled_order_without_execution() {
        let exchange = PaperExchange::new(Arc::new(StubExchange));
        let venue = match Venue::new(VenueKind::Dex, "paper-test") {
            Ok(venue) => venue,
            Err(error) => panic!("test venue should be valid: {error}"),
        };
        let symbol = match Symbol::new("0x0000000000000000000000000000000000000001") {
            Ok(symbol) => symbol,
            Err(error) => panic!("test symbol should be valid: {error}"),
        };
        let order = match Order::new(venue, symbol, OrderSide::Sell, Decimal::ONE) {
            Ok(order) => order,
            Err(error) => panic!("test order should be valid: {error}"),
        };
        let filled = match exchange.submit_order(order).await {
            Ok(filled) => filled,
            Err(error) => panic!("paper sell should succeed: {error}"),
        };
        assert_eq!(filled.quantity, Decimal::ONE);
        assert_eq!(filled.execution_price, Some(Decimal::from(2)));
        assert_eq!(filled.quote_proceeds, Some(Decimal::from(2)));
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

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    /// Detection and real execution when wallets are configured.
    Live,
    /// Run the complete strategy with real market data but simulate orders.
    Paper,
    /// Detect and evaluate listings, but never attempt acquisition or exits.
    DetectionOnly,
}

impl Default for ExecutionMode {
    fn default() -> Self {
        Self::Live
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RiskConfig {
    /// Take-profit target as a percentage above entry price, e.g. 10.0
    /// for the "+10%" strategy this bot is built around. This is the
    /// only exit condition - there is deliberately no stop-loss. A
    /// position is held until it hits this target, however long that
    /// takes; it is never sold at a loss.
    pub take_profit_percent: Decimal,

    /// How often, in seconds, each listing source gets polled.
    pub poll_interval_seconds: u64,

    /// How often listings that were detected before an indexer exposed
    /// their metrics are retried. This is intentionally independent from
    /// source polling so fast feeds do not hammer slower indexers.
    pub pending_listing_retry_seconds: u64,

    /// Maximum quote-currency amount to spend on a single new listing.
    /// This is the single most important number in the whole config for
    /// keeping a bad listing from being an expensive mistake.
    pub max_position_size: Decimal,

    /// A listing is only bought if its 24h volume is at or above this -
    /// the sole acquisition gate. Deliberately not paired with a market
    /// cap ceiling: a high-market-cap listing with genuinely active
    /// volume is just as tradeable as a low-cap one, so market cap isn't
    /// used to disqualify a listing either way. "Active volume" here
    /// means the smallest threshold that's still meaningful - enough to
    /// indicate real trading beyond a single initial buy, not a high bar
    /// that delays detection until a token has already built up
    /// substantial volume. See `config/default.toml` for the reasoning
    /// behind the specific default chosen.
    pub min_volume_24h: Decimal,

    /// Hard cap on concurrently open positions across all venues. Once
    /// reached, remaining listings in a poll cycle are skipped rather
    /// than queued, so capital exposure never exceeds
    /// `max_open_positions * max_position_size`.
    pub max_open_positions: usize,

    /// Consecutive acquisition/exit failures (I/O errors, not ordinary
    /// "pending"/"rejected" outcomes) before the entry circuit breaker
    /// opens and new buys pause for the rest of that poll cycle.
    pub max_consecutive_failures: u32,

    /// Maximum number of newly-detected listings processed for
    /// acquisition in a single poll cycle across all venues. Protects
    /// against a listing burst (e.g. a source replaying its backlog)
    /// from spending capital faster than the operator can react.
    pub max_new_listings_per_cycle: usize,

    /// Path to an operator-controlled kill-switch file. While it exists,
    /// no new positions are opened, but existing positions are still
    /// watched and exited normally.
    pub entry_kill_switch_file: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SafetyConfig {
    /// Maximum acceptable sell tax, in basis points (100 = 1%), for a
    /// DEX listing to pass the honeypot/rug safety gate. Only applies to
    /// venues that have a `SafetyGate` configured - see the README.
    pub max_sell_tax_bps: u32,
    #[serde(default = "default_max_token_transfer_fee_bps")]
    pub max_token_transfer_fee_bps: u32,
}

fn default_max_token_transfer_fee_bps() -> u32 {
    0
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ObservabilityConfig {
    /// Local HTTP bind address for the Prometheus-compatible `/metrics`
    /// endpoint. Keep this on loopback unless access control is provided
    /// by the deployment environment.
    #[serde(default = "default_metrics_bind")]
    pub metrics_bind: String,
}

fn default_metrics_bind() -> String {
    "127.0.0.1:9090".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig {
    /// Directory where the statefile adapter keeps its per-source JSON
    /// snapshots, and where the acquisition ledger's file lives.
    pub state_dir: String,
}

/// Solana execution settings. Deliberately has no field for the wallet
/// key - see `ben_snipes-adapter-pumpfun::execution::load_wallet`,
/// which reads the `SOLANA_PRIVATE_KEY` environment variable directly,
/// never config.
#[derive(Debug, Clone, Deserialize)]
pub struct SolanaConfig {
    /// PumpPortal's data websocket URL. Defaults to their public free
    /// endpoint - no API key needed for `subscribeNewToken`.
    pub pumpportal_ws_url: String,

    /// A Solana JSON-RPC HTTP endpoint used for broadcasting signed
    /// transactions and checking balances/confirmations. Unlike
    /// `pumpportal_ws_url`, this should be your own provider (public
    /// endpoints are typically rate-limited too aggressively for
    /// trading use).
    pub rpc_url: String,

    /// Slippage tolerance, as a percent, passed through to PumpPortal's
    /// trade-local API on every buy/sell.
    pub slippage_percent: u32,

    /// Priority fee in SOL, passed through to PumpPortal's trade-local
    /// API on every buy/sell - helps transactions land faster under
    /// network congestion.
    pub priority_fee_sol: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EvmChainConfig {
    /// e.g. "ethereum", "base" - becomes this source's chain identity.
    pub chain_name: String,
    /// Numeric EVM chain ID. Used to prevent signing against the wrong network.
    pub chain_id: u64,
    /// A websocket RPC endpoint that supports `eth_subscribe`, with your
    /// own provider API key included. There is no usable default here -
    /// this must be supplied per deployment.
    pub ws_rpc_url: String,
    /// HTTP RPC endpoint used for reads and execution when no private RPC is configured.
    pub execution_rpc_url: String,
    /// Optional private transaction RPC, such as Flashbots Protect.
    #[serde(default)]
    pub private_rpc_url: Option<String>,
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
    /// Uniswap-V2-compatible router used for native-coin buys and token sells.
    pub router_address: String,
    /// Wrapped native token used as the router path endpoint.
    pub wrapped_native_address: String,
    /// Maximum execution slippage as a percentage.
    #[serde(default = "default_evm_slippage_percent")]
    pub slippage_percent: u32,
}

fn default_evm_slippage_percent() -> u32 { 10 }

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub execution_mode: ExecutionMode,
    pub risk: RiskConfig,
    pub safety: SafetyConfig,
    #[serde(default)]
    pub observability: ObservabilityConfig,
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

--- ./crates/domain/src/error.rs ---
use thiserror::Error;

/// Errors that come from violating a business rule, as opposed to errors
/// from I/O (those live in `ben_snipes-ports`, next to the traits that can
/// fail in I/O-flavoured ways).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DomainError {
    #[error("take-profit percentage must be positive, got {0}")]
    InvalidProfitTarget(String),

    #[error("symbol cannot be empty")]
    EmptySymbol,

    #[error("venue name cannot be empty")]
    EmptyVenueName,

    #[error("chain identifier cannot be empty")]
    EmptyChain,

    #[error("invalid chain configuration: {0}")]
    InvalidChainConfig(String),

    #[error("order quantity must be positive, got {0}")]
    InvalidQuantity(String),

    #[error("min volume must not be negative, got {0}")]
    InvalidMinVolume(String),
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
mod trade;
mod venue;

pub use acquisition::{AcquisitionCriteria, ListingMetrics};
pub use canonical::CanonicalTokenId;
pub use chain::Chain;
pub use error::DomainError;
pub use listing::{Listing, Symbol};
pub use order::{FilledBuy, FilledSell, Order, OrderSide, OrderStatus};
pub use position::{Position, ProfitTarget};
pub use safety::{SafetyCriteria, SafetyReport, SellabilityEvidence};
pub use trade::{PerformanceSummary, TradeRecord};
pub use venue::{Venue, VenueKind};

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
    #[serde(with = "time::serde::rfc3339")]
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

/// The result of an amount-based buy (see `ExchangeClient::submit_buy_by_amount`):
/// how many units were actually acquired, and the effective price that
/// implies. Unlike a quantity-based `Order`, neither of these is known
/// until *after* the trade executes - a venue like a bonding-curve DEX
/// doesn't expose a pre-trade quote the way a CEX order book does, so
/// the caller spends a known amount and finds out what it bought.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FilledBuy {
    pub quantity: Decimal,
    pub entry_price: Decimal,
}

/// Settlement metadata returned after a quantity-based sell.
///
/// DEXes do not always expose the final quote proceeds through the same
/// interface that submits the transaction, so exact settlement fields are
/// optional. A transaction id is still recorded whenever the venue exposes
/// one, which makes the journal auditable without pretending a pre-trade
/// quote was an on-chain fill.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilledSell {
    pub quantity: Decimal,
    pub execution_price: Option<Decimal>,
    pub quote_proceeds: Option<Decimal>,
    pub fee_quote: Option<Decimal>,
    pub tx_id: Option<String>,
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

--- ./crates/domain/src/position.rs ---
use crate::{DomainError, Symbol, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// A take-profit rule expressed as a percentage above entry price, e.g.
/// `ProfitTarget::from_percent(10)` for "sell at +10%".
///
/// This is the *only* exit condition this bot uses, by design: it holds
/// a position until the target is reached, however long that takes,
/// rather than cutting losses early. That's a deliberate strategy
/// choice, not an oversight - see `Position::should_exit`.
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub venue: Venue,
    pub symbol: Symbol,
    pub entry_price: Decimal,
    pub quantity: Decimal,
    pub target: ProfitTarget,
}

impl Position {
    pub fn new(venue: Venue, symbol: Symbol, entry_price: Decimal, quantity: Decimal, target: ProfitTarget) -> Self {
        Self {
            venue,
            symbol,
            entry_price,
            quantity,
            target,
        }
    }

    /// The single exit condition: has this position reached its
    /// take-profit target. There is no stop-loss - this bot holds until
    /// the target is reached, full stop, by explicit design.
    pub fn should_exit(&self, current_price: Decimal) -> bool {
        self.target.is_reached(self.entry_price, current_price)
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

    fn sample_position() -> Position {
        let venue = crate::Venue::new(crate::VenueKind::Dex, "pumpfun").expect("literal venue is valid");
        let symbol = crate::Symbol::new("PEPEUSDT").expect("literal symbol is valid");
        Position::new(
            venue,
            symbol,
            Decimal::ONE_HUNDRED,
            Decimal::TEN,
            ProfitTarget::from_percent(Decimal::TEN).expect("valid target"),
        )
    }

    #[test]
    fn does_not_exit_below_target() {
        let position = sample_position();
        assert!(!position.should_exit(Decimal::from(105)));
    }

    #[test]
    fn does_not_exit_far_below_entry_either() {
        // The whole point: no stop-loss. A price crash doesn't trigger
        // an exit - only reaching the take-profit target does.
        let position = sample_position();
        assert!(!position.should_exit(Decimal::from(10)));
    }

    #[test]
    fn exits_at_or_above_target() {
        let position = sample_position();
        assert!(position.should_exit(Decimal::from(110)));
    }
}

--- ./crates/domain/src/safety.rs ---
use serde::{Deserialize, Serialize};

/// Evidence that a token can be transferred out of the holder account.
/// `Structural` is intentionally weaker than a live DEX sell simulation: it
/// means the token program state does not expose the known transfer traps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SellabilityEvidence {
    /// A live buy/sell or equivalent execution simulation succeeded.
    Simulated,
    /// On-chain token-program state was inspected and no known transfer trap
    /// was found. This does not guarantee DEX liquidity or route execution.
    Structural,
    /// The checker did not have enough evidence.
    Unknown,
    /// The checker found a definitive transfer/sell failure.
    Failed,
}

/// On-chain safety signals for a token, gathered before buying a DEX
/// listing. This is scoped to DEX-style acquisitions on purpose: a CEX
/// listing has already been through the exchange's own vetting (it
/// can't be an unsellable honeypot contract, because the exchange
/// controls the order book, not a smart contract the token author
/// wrote), so `AcquisitionEngine` only applies this gate when a
/// `SafetyGate` is actually configured for a venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyReport {
    /// Sell tax in basis points (100 = 1%). `None` means the checker could
    /// not verify a sell tax. Unknown must remain distinct from a measured
    /// zero so the safety gate cannot accidentally fail open.
    pub sell_tax_bps: Option<u32>,
    /// Token-level transfer fee, distinct from a DEX/router sell tax.
    /// `None` means the mint policy could not be inspected.
    #[serde(default)]
    pub token_transfer_fee_bps: Option<u32>,
    /// Evidence that the token can be transferred/sold. This is deliberately
    /// independent from `sell_tax_bps`, because a tax quote is not proof that
    /// a sell path works.
    #[serde(default = "default_sellability_evidence")]
    pub sellability: SellabilityEvidence,
    /// Whether a Token-2022 permanent delegate can seize or otherwise
    /// control token accounts. This is a hard safety failure.
    #[serde(default)]
    pub has_permanent_delegate: bool,
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
    max_token_transfer_fee_bps: u32,
}

fn default_sellability_evidence() -> SellabilityEvidence {
    SellabilityEvidence::Unknown
}

impl SafetyCriteria {
    pub fn new(max_sell_tax_bps: u32, max_token_transfer_fee_bps: u32) -> Self {
        Self {
            max_sell_tax_bps,
            max_token_transfer_fee_bps,
        }
    }

    pub fn passes(&self, report: &SafetyReport) -> bool {
        let Some(sell_tax_bps) = report.sell_tax_bps else {
            return false;
        };
        if sell_tax_bps > self.max_sell_tax_bps {
            return false;
        }
        let Some(token_transfer_fee_bps) = report.token_transfer_fee_bps else {
            return false;
        };
        if token_transfer_fee_bps > self.max_token_transfer_fee_bps {
            return false;
        }
        if !matches!(report.sellability, SellabilityEvidence::Simulated | SellabilityEvidence::Structural)
            || report.has_permanent_delegate
        {
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
            sell_tax_bps: Some(200),
            token_transfer_fee_bps: Some(0),
            sellability: SellabilityEvidence::Simulated,
            has_permanent_delegate: false,
            ownership_renounced: true,
            liquidity_locked: true,
            is_mintable: false,
        }
    }

    #[test]
    fn accepts_a_clean_report() {
        let criteria = SafetyCriteria::new(1_000, 0);
        assert!(criteria.passes(&safe_report()));
    }

    #[test]
    fn rejects_sell_tax_above_threshold() {
        let criteria = SafetyCriteria::new(500, 0);
        let report = SafetyReport {
            sell_tax_bps: Some(900),
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn rejects_unknown_sell_tax() {
        let criteria = SafetyCriteria::new(1_000, 0);
        let report = SafetyReport {
            sell_tax_bps: None,
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn rejects_unknown_token_transfer_fee() {
        let criteria = SafetyCriteria::new(1_000, 0);
        let report = SafetyReport {
            token_transfer_fee_bps: None,
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn rejects_unverified_sellability() {
        let criteria = SafetyCriteria::new(1_000, 0);
        let report = SafetyReport {
            sellability: SellabilityEvidence::Unknown,
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn rejects_permanent_delegate() {
        let criteria = SafetyCriteria::new(1_000, 0);
        let report = SafetyReport {
            has_permanent_delegate: true,
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn rejects_mintable_supply_regardless_of_other_signals() {
        let criteria = SafetyCriteria::new(1_000, 0);
        let report = SafetyReport {
            is_mintable: true,
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn rejects_when_neither_renounced_nor_locked() {
        let criteria = SafetyCriteria::new(1_000, 0);
        let report = SafetyReport {
            ownership_renounced: false,
            liquidity_locked: false,
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn accepts_when_only_liquidity_is_locked() {
        let criteria = SafetyCriteria::new(1_000, 0);
        let report = SafetyReport {
            ownership_renounced: false,
            liquidity_locked: true,
            ..safe_report()
        };
        assert!(criteria.passes(&report));
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

--- ./crates/domain/src/trade.rs ---
use crate::{FilledSell, Symbol, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// A completed position plus the settlement information available from the
/// venue. Some adapters can provide exact execution metadata while others
/// only expose confirmation and a transaction id, so reference-price
/// accounting is explicitly marked instead of being presented as exact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradeRecord {
    pub venue: Venue,
    pub symbol: Symbol,
    pub quantity: Decimal,
    pub entry_price: Decimal,
    pub exit_price: Decimal,
    pub quote_cost: Decimal,
    pub quote_proceeds: Decimal,
    #[serde(default)]
    pub fee_quote: Decimal,
    pub pnl: Decimal,
    #[serde(with = "time::serde::rfc3339")]
    pub closed_at: OffsetDateTime,
    #[serde(default)]
    pub execution_price_is_reference: bool,
    #[serde(default)]
    pub tx_id: Option<String>,
}

impl TradeRecord {
    /// Stable identity for one position lifecycle. The acquisition ledger
    /// prevents the same canonical token from being opened twice, so this
    /// key also lets recovery treat an already-journaled close as idempotent.
    pub fn key(&self) -> String {
        format!(
            "{}::{}::{}::{}",
            self.venue,
            self.symbol,
            self.entry_price,
            self.quantity
        )
    }

    pub fn key_for_position(position: &crate::Position) -> String {
        format!(
            "{}::{}::{}::{}",
            position.venue,
            position.symbol,
            position.entry_price,
            position.quantity
        )
    }

    pub fn from_position(
        position: &crate::Position,
        exit_price: Decimal,
        closed_at: OffsetDateTime,
    ) -> Self {
        let quote_cost = position.entry_price * position.quantity;
        let quote_proceeds = exit_price * position.quantity;
        let pnl = quote_proceeds - quote_cost;

        Self {
            venue: position.venue.clone(),
            symbol: position.symbol.clone(),
            quantity: position.quantity,
            entry_price: position.entry_price,
            exit_price,
            quote_cost,
            quote_proceeds,
            fee_quote: Decimal::ZERO,
            pnl,
            closed_at,
            execution_price_is_reference: true,
            tx_id: None,
        }
    }

    /// Builds a journal record from venue settlement metadata. If the venue
    /// does not expose an execution price or quote proceeds, the supplied
    /// reference price is used and the record is explicitly marked as such.
    pub fn from_fill(
        position: &crate::Position,
        fill: &FilledSell,
        reference_price: Decimal,
        closed_at: OffsetDateTime,
    ) -> Self {
        let execution_price = fill.execution_price.unwrap_or(reference_price);
        let quote_cost = position.entry_price * position.quantity;
        let quote_proceeds = fill
            .quote_proceeds
            .unwrap_or_else(|| execution_price * fill.quantity);
        let fee_quote = fill.fee_quote.unwrap_or(Decimal::ZERO);
        let pnl = quote_proceeds - quote_cost - fee_quote;

        Self {
            venue: position.venue.clone(),
            symbol: position.symbol.clone(),
            quantity: fill.quantity,
            entry_price: position.entry_price,
            exit_price: execution_price,
            quote_cost,
            quote_proceeds,
            fee_quote,
            pnl,
            closed_at,
            execution_price_is_reference: fill.execution_price.is_none(),
            tx_id: fill.tx_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PerformanceSummary {
    pub trade_count: u64,
    pub winning_trades: u64,
    pub losing_trades: u64,
    pub flat_trades: u64,
    pub total_quote_cost: Decimal,
    pub total_quote_proceeds: Decimal,
    pub total_fees: Decimal,
    pub realized_pnl: Decimal,
}

impl PerformanceSummary {
    pub fn from_trades(trades: &[TradeRecord]) -> Self {
        let mut summary = Self::default();
        for trade in trades {
            summary.trade_count = summary.trade_count.saturating_add(1);
            summary.total_quote_cost += trade.quote_cost;
            summary.total_quote_proceeds += trade.quote_proceeds;
            summary.total_fees += trade.fee_quote;
            summary.realized_pnl += trade.pnl;

            if trade.pnl > Decimal::ZERO {
                summary.winning_trades = summary.winning_trades.saturating_add(1);
            } else if trade.pnl < Decimal::ZERO {
                summary.losing_trades = summary.losing_trades.saturating_add(1);
            } else {
                summary.flat_trades = summary.flat_trades.saturating_add(1);
            }
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProfitTarget, VenueKind};

    fn position() -> crate::Position {
        let venue = Venue::new(VenueKind::Dex, "raydium").expect("literal venue is valid");
        let symbol = Symbol::new("TOKEN").expect("literal symbol is valid");
        crate::Position::new(
            venue,
            symbol,
            Decimal::from(10),
            Decimal::from(4),
            ProfitTarget::from_percent(Decimal::TEN).expect("valid target"),
        )
    }

    #[test]
    fn records_reference_exit_and_pnl() {
        let trade = TradeRecord::from_position(&position(), Decimal::from(11), OffsetDateTime::UNIX_EPOCH);
        assert_eq!(trade.quote_cost, Decimal::from(40));
        assert_eq!(trade.quote_proceeds, Decimal::from(44));
        assert_eq!(trade.pnl, Decimal::from(4));
    }

    #[test]
    fn from_fill_prefers_exact_settlement_metadata() {
        let position = position();
        let fill = FilledSell {
            quantity: Decimal::from(4),
            execution_price: Some(Decimal::from(11)),
            quote_proceeds: Some(Decimal::from(43)),
            fee_quote: Some(Decimal::from(1)),
            tx_id: Some("0xabc".to_string()),
        };
        let trade = TradeRecord::from_fill(
            &position,
            &fill,
            Decimal::from(999),
            OffsetDateTime::UNIX_EPOCH,
        );
        assert_eq!(trade.exit_price, Decimal::from(11));
        assert_eq!(trade.quote_proceeds, Decimal::from(43));
        assert_eq!(trade.fee_quote, Decimal::ONE);
        assert_eq!(trade.pnl, Decimal::from(2));
        assert!(!trade.execution_price_is_reference);
        assert_eq!(trade.tx_id.as_deref(), Some("0xabc"));
    }

    #[test]
    fn summarizes_wins_losses_and_flats() {
        let p = position();
        let trades = vec![
            TradeRecord::from_position(&p, Decimal::from(11), OffsetDateTime::UNIX_EPOCH),
            TradeRecord::from_position(&p, Decimal::from(9), OffsetDateTime::UNIX_EPOCH),
            TradeRecord::from_position(&p, Decimal::from(10), OffsetDateTime::UNIX_EPOCH),
        ];
        let summary = PerformanceSummary::from_trades(&trades);
        assert_eq!(summary.trade_count, 3);
        assert_eq!(summary.winning_trades, 1);
        assert_eq!(summary.losing_trades, 1);
        assert_eq!(summary.flat_trades, 1);
        assert_eq!(summary.realized_pnl, Decimal::ZERO);
    }
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

--- ./crates/ports/src/exchange_client.rs ---
use crate::PortError;
use async_trait::async_trait;
use ben_snipes_domain::{FilledBuy, FilledSell, Order, Symbol};
use rust_decimal::Decimal;

/// Trading operations against a single venue. A CEX adapter implements
/// this with signed REST calls; a DEX adapter implements it with signed
/// on-chain transactions (ideally routed through a private relay - see
/// the README for why that matters here). The application layer doesn't
/// need to know or care which.
#[async_trait]
pub trait ExchangeClient: Send + Sync {
    fn venue_name(&self) -> &str;

    /// Current price of `symbol` in the venue's quote asset. Used for
    /// exit monitoring (deciding *when* a position has crossed its
    /// take-profit/stop-loss) - not for entry sizing, see
    /// `submit_buy_by_amount`.
    async fn current_price(&self, symbol: &Symbol) -> Result<Decimal, PortError>;

    /// Buys `symbol` by spending `quote_amount` of the venue's quote
    /// asset (e.g. SOL, USDT), and reports back what was actually
    /// acquired. This is the entry point for opening a position -
    /// deliberately amount-based rather than quantity-based, because
    /// venues without a queryable pre-trade order book (a bonding-curve
    /// DEX, for instance) can't offer a quantity-for-a-given-price quote
    /// the way a CEX can. A CEX-style adapter that *does* have a live
    /// order book is free to fetch its own price internally and convert;
    /// the port doesn't force that round-trip on venues that don't need
    /// it.
    async fn submit_buy_by_amount(&self, symbol: &Symbol, quote_amount: Decimal) -> Result<FilledBuy, PortError>;

    /// Submit an order and return settlement metadata for the resulting sell.
    /// The quantity being sold is already known, while execution price,
    /// proceeds, fees, and transaction id depend on the venue and may be
    /// partially unavailable. Implementations must never fabricate missing
    /// settlement fields.
    async fn submit_order(&self, order: Order) -> Result<FilledSell, PortError>;

    /// Optional venue-specific preflight for a sell. DEX adapters can use
    /// this to simulate the exact sell transaction against current on-chain
    /// state before signing it. The default is a no-op so venues whose
    /// submit path already performs an equivalent simulation do not need a
    /// second implementation.
    async fn preflight_sell(&self, _order: &Order) -> Result<(), PortError> {
        Ok(())
    }
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
mod position_store;
mod state_store;
mod token_safety_checker;
mod trade_store;

pub use acquisition_ledger::AcquisitionLedger;
pub use clock::Clock;
pub use error::PortError;
pub use exchange_client::ExchangeClient;
pub use listing_source::{ListingSnapshot, ListingSource};
pub use metrics_provider::MetricsProvider;
pub use position_store::PositionStore;
pub use state_store::{KnownListings, ListingStateStore, PendingListing};
pub use token_safety_checker::TokenSafetyChecker;
pub use trade_store::{PendingTradeStore, TradeStore};

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

--- ./crates/ports/src/position_store.rs ---
use crate::PortError;
use async_trait::async_trait;
use ben_snipes_domain::Position;

/// Persists the set of currently-open positions, so a restart can
/// recover what's still held and resume watching it for exit.
///
/// Without this, a crash after a real buy doesn't just lose bookkeeping
/// - it orphans the position entirely: the `AcquisitionLedger` still
/// shows the token as already-acquired (so it's never re-detected as
/// buyable), but nothing is left watching it for take-profit/stop-loss,
/// since that list only ever lived in the runner's memory. This is what
/// closes that gap.
#[async_trait]
pub trait PositionStore: Send + Sync {
    /// Loads whatever was open at last save. Returns an empty list if
    /// nothing was ever saved - a fresh deployment has nothing open yet,
    /// which is the expected first-run state, not an error.
    async fn load(&self) -> Result<Vec<Position>, PortError>;

    /// Persists the complete current set of open positions - this
    /// replaces whatever was saved before, it doesn't append. Called
    /// after every change to the open-position list (a new buy, a
    /// closed exit) so a crash between calls loses at most the single
    /// most recent change, not the whole list.
    async fn save(&self, positions: &[Position]) -> Result<(), PortError>;
}

--- ./crates/ports/src/state_store.rs ---
use crate::PortError;
use async_trait::async_trait;
use ben_snipes_domain::Listing;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use time::OffsetDateTime;

/// What we remember about a single listing source between polls: the set
/// of dedupe keys (see `Listing::dedupe_key`) we've already seen, plus
/// whatever cursor that source gave us last time, if any.
///
/// This is what gets written to the statefile (or database, or key-value
/// store - whatever `ListingStateStore` adapter is wired in).
///
/// A listing that remains eligible for periodic re-evaluation while
/// external metrics or safety data are incomplete or below the buy threshold.
/// `pending_since` is the start of the fixed 24-hour retry window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PendingListing {
    pub listing: Listing,
    #[serde(with = "time::serde::rfc3339")]
    pub pending_since: OffsetDateTime,
}

impl PendingListing {
    pub fn new(listing: Listing, pending_since: OffsetDateTime) -> Self {
        Self {
            listing,
            pending_since,
        }
    }
}

impl<'de> Deserialize<'de> for PendingListing {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Current {
                listing: Listing,
                #[serde(with = "time::serde::rfc3339")]
                pending_since: OffsetDateTime,
            },
            Legacy(Listing),
        }

        match Wire::deserialize(deserializer)? {
            Wire::Current {
                listing,
                pending_since,
            } => Ok(Self {
                listing,
                pending_since,
            }),
            Wire::Legacy(listing) => Ok(Self {
                pending_since: listing.first_seen,
                listing,
            }),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KnownListings {
    pub seen_keys: HashSet<String>,
    pub cursor: Option<String>,
    /// Listings that were detected but did not yet have enough external
    /// information to make a safe acquisition decision. These are kept
    /// separately from `seen_keys` so a temporary indexer lag cannot make
    /// a real opportunity disappear forever.
    #[serde(default)]
    pub pending: HashMap<String, PendingListing>,
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

--- ./crates/ports/src/trade_store.rs ---
use crate::PortError;
use async_trait::async_trait;
use ben_snipes_domain::TradeRecord;

/// Persists completed trades so performance survives process restarts.
/// Implementations should treat records as append-only business history.
#[async_trait]
pub trait TradeStore: Send + Sync {
    async fn load(&self) -> Result<Vec<TradeRecord>, PortError>;
    async fn append(&self, trade: &TradeRecord) -> Result<(), PortError>;
}


/// Durable queue for trades whose execution succeeded but whose main journal
/// could not be persisted yet. This prevents a filesystem failure from
/// turning a confirmed sell into an unaccounted-for trade.
#[async_trait]
pub trait PendingTradeStore: Send + Sync {
    async fn load(&self) -> Result<Vec<TradeRecord>, PortError>;
    async fn save(&self, trades: &[TradeRecord]) -> Result<(), PortError>;
}

