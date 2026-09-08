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
use ben_snipes_adapter_statefile::{
    FileAcquisitionLedger, FilePendingTradeStore, FilePositionStore, FileTradeStore, InstanceLock,
    StatefileStore,
};
use ben_snipes_application::{AcquisitionDecision, AcquisitionEngine, NewListingDetector, PaperExchange, PositionManager, RuntimeMetrics, SafetyGate};
use ben_snipes_config::{AppConfig, ExecutionMode};
use ben_snipes_domain::{
    AcquisitionCriteria, ListingMetrics, PerformanceSummary, Position, ProfitTarget, SafetyCriteria, SafetyReport, TradeRecord,
};
use ben_snipes_ports::{
    AcquisitionLedger, ExchangeClient, ListingSource, PendingTradeStore, PositionStore,
    SystemClock, TradeStore,
};
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
    // Handled before anything else - no config, no network, no wallet
    // access - so `--version`/`--help` are instant and side-effect-free.
    // This matters beyond convenience: install.sh's post-install self
    // test runs `./ben_snipes --version` and relies on it actually
    // exiting immediately. Without this, the binary ignored all CLI
    // arguments entirely, so that line launched the *real* bot - full
    // startup, real RPC connections, and (since install.sh has already
    // sourced .env by that point, including a real SOLANA_PRIVATE_KEY
    // if the user provided one) potentially real trading - and then
    // never returned, since the main loop runs forever. That would hang
    // every fresh install on this exact line.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("ben_snipes {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("ben_snipes {}", env!("CARGO_PKG_VERSION"));
        println!("A Solana/EVM new-listing trading bot.");
        println!();
        println!("Configuration is via config/default.toml and environment");
        println!("variables (SOLANA_PRIVATE_KEY, etc.) - not CLI flags.");
        println!();
        println!("USAGE:");
        println!("    ben_snipes");
        println!();
        println!("OPTIONS:");
        println!("    -V, --version    Print version and exit");
        println!("    -h, --help       Print this help and exit");
        return;
    }

    // Must happen before any TLS connection is attempted (reqwest calls
    // in pumpfun/evm-onchain, websocket connects in evm-onchain). Two
    // different rustls backend crates (ring, aws-lc-rs) are reachable
    // through this workspace's dependency graph, and rustls refuses to
    // guess between them - the first TLS handshake panics instead.
    // Installing one explicitly, once, up front resolves that
    // deterministically regardless of which adapter makes the first
    // network call.
    if rustls::crypto::ring::default_provider().install_default().is_err() {
        // Only reachable if something else in-process already installed
        // a provider first - not an error, just means we were beaten to
        // it (e.g. under a future test harness that runs `main`'s setup
        // more than once in the same process).
        eprintln!("rustls crypto provider was already installed; continuing with the existing one");
    }

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

    // Must happen before any other state file is touched: two processes
    // racing on the same open-positions/trade-journal files could
    // double-buy, double-sell, or corrupt the journal. `_instance_lock`
    // is held for the remaining lifetime of `main` and released (lock
    // file removed) on drop - i.e. on normal process exit.
    let instance_lock_path = format!("{}/instance.lock", config.storage.state_dir);
    let _instance_lock = match InstanceLock::acquire(&instance_lock_path) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

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
    let detector = NewListingDetector::new(state_store, Arc::new(SystemClock));

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
