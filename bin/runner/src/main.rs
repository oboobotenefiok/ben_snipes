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
//! `dex-mock` demo venue is kept
//! alongside both so `cargo run` still demonstrates the full pipeline
//! end to end with synthetic data, independent of any real network
//! access or funded wallet.

use ben_snipes_adapter_dex_mock::{MockDexClient, MockDexSource};
use ben_snipes_adapter_evm_onchain::{
    DexScreenerEvmMetrics, EvmFactoryConfig, EvmFactoryLogSource, EvmUniswapV2Exchange,
    HoneypotEvmSafetyChecker, NoWalletEvmExchange,
};
use ben_snipes_adapter_pumpfun::{
    load_wallet, wallet_pubkey_string, DexScreenerMetricsProvider, NoWalletExchange,
    PumpPortalExchangeClient, PumpPortalSource, RugCheckSafetyChecker,
};
use ben_snipes_adapter_statefile::{FileAcquisitionLedger, FilePositionStore, StatefileStore};
use ben_snipes_application::{AcquisitionDecision, AcquisitionEngine, NewListingDetector, PositionManager, SafetyGate};
use ben_snipes_config::AppConfig;
use ben_snipes_domain::{
    AcquisitionCriteria, ListingMetrics, Position, ProfitTarget, SafetyCriteria, SafetyReport,
};
use ben_snipes_ports::{AcquisitionLedger, ExchangeClient, ListingSource, PositionStore};
use rust_decimal::Decimal;
use std::fmt::Display;
use std::path::Path;
use std::sync::Arc;
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
                sell_tax_bps: Some(150),
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

    // Wallet is optional at startup, deliberately: absence of a key
    // should disable trading, not crash a bot that's otherwise perfectly
    // capable of running in detection-only mode. See execution.rs for
    // why this is the single highest-risk code path in the project if a
    // wallet *is* configured.
    let solana_exchange: Arc<dyn ExchangeClient> = match load_wallet() {
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
    };

    // Real, network-backed data sources - see each crate's module doc
    // comments for confidence caveats on RugCheck's field mapping and
    // Jupiter's SOL-denomination conversion specifically.
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

        let evm_exchange: Arc<dyn ExchangeClient> = match EvmUniswapV2Exchange::from_env(
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
        safety_criteria: SafetyCriteria::new(config.safety.max_sell_tax_bps),
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

    let position_store = FilePositionStore::new(format!("{}/open-positions.json", config.storage.state_dir));
    let mut open_positions: Vec<Position> = expect_valid_config(position_store.load().await, "open positions file");
    if !open_positions.is_empty() {
        info!(count = open_positions.len(), "recovered open positions from a previous run");
    }

    let venues = build_venues(&config, &risk, ledger).await;

    let mut interval = tokio::time::interval(Duration::from_secs(config.risk.poll_interval_seconds));
    let mut shutdown = std::pin::pin!(tokio::signal::ctrl_c());
    let mut consecutive_failures = 0_u32;
    let mut last_pending_retry = Instant::now()
        .checked_sub(Duration::from_secs(config.risk.pending_listing_retry_seconds))
        .unwrap_or_else(|| Instant::now());

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // 1. Scan every venue for newly-appeared listings. New entries
                //    are blocked when an operator kill switch exists, the
                //    concurrent-position cap is full, a listing burst exceeds
                //    the configured threshold, or the failure circuit is open.
                let kill_switch_active = Path::new(&config.risk.entry_kill_switch_file).exists();
                let mut detected_this_cycle = 0_usize;
                let entries_paused = kill_switch_active
                    || consecutive_failures >= config.risk.max_consecutive_failures;

                if kill_switch_active {
                    warn!(path = %config.risk.entry_kill_switch_file, "entry kill switch active; new buys are paused");
                }
                if consecutive_failures >= config.risk.max_consecutive_failures {
                    warn!(consecutive_failures, "entry circuit breaker open; new buys are paused");
                }

                let retry_pending = last_pending_retry.elapsed() >= Duration::from_secs(config.risk.pending_listing_retry_seconds);
                if retry_pending {
                    last_pending_retry = Instant::now();
                }

                for venue in &venues {
                    let new_listings = match detector.poll(venue.source.as_ref(), retry_pending).await {
                        Ok(listings) => listings,
                        Err(e) => {
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            warn!(source = venue.source.source_id(), error = %e, "poll failed, will retry next tick");
                            continue;
                        }
                    };

                    detected_this_cycle = detected_this_cycle.saturating_add(new_listings.len());
                    if detected_this_cycle > config.risk.max_new_listings_per_cycle {
                        warn!(
                            detected = detected_this_cycle,
                            limit = config.risk.max_new_listings_per_cycle,
                            "listing burst exceeded configured threshold; blocking new entries for this cycle"
                        );
                        break;
                    }

                    if entries_paused || detected_this_cycle > config.risk.max_new_listings_per_cycle {
                        continue;
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

                        match venue.acquisition.evaluate_and_buy(&listing).await {
                            Ok(AcquisitionDecision::Opened(position)) => {
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
                                if let Err(e) = position_store.save(&open_positions).await {
                                    consecutive_failures = consecutive_failures.saturating_add(1);
                                    warn!(error = %e, "failed to persist open positions after a buy; position is still tracked in memory this run");
                                }
                            }
                            Ok(AcquisitionDecision::Pending) => {
                                consecutive_failures = 0;
                                info!(symbol = listing.symbol.as_str(), "listing lacks required external data yet; retained for retry");
                            }
                            Ok(AcquisitionDecision::Rejected) => {
                                consecutive_failures = 0;
                                if let Err(e) = detector.resolve(venue.source.as_ref(), &listing, false).await {
                                    consecutive_failures = consecutive_failures.saturating_add(1);
                                    warn!(symbol = listing.symbol.as_str(), error = %e, "failed to resolve rejected listing state");
                                }
                                info!(symbol = listing.symbol.as_str(), "listing did not qualify for acquisition");
                            }
                            Err(e) => {
                                consecutive_failures = consecutive_failures.saturating_add(1);
                                warn!(symbol = listing.symbol.as_str(), error = %e, "acquisition attempt failed; retaining listing for retry");
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
                        Ok(Some(_filled_order)) => {
                            consecutive_failures = 0;
                            info!(symbol = position.symbol.as_str(), "take-profit reached, position closed");
                        }
                        Ok(None) => still_open.push(position),
                        Err(e) => {
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            warn!(symbol = position.symbol.as_str(), error = %e, "exit check failed, will retry next tick");
                            still_open.push(position);
                        }
                    }
                }
                open_positions = still_open;
                if let Err(e) = position_store.save(&open_positions).await {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    warn!(error = %e, "failed to persist open positions after exit checks");
                }
            }
            _ = &mut shutdown => {
                info!(open_positions = open_positions.len(), "shutdown signal received, exiting cleanly");
                break;
            }
        }
    }
}
