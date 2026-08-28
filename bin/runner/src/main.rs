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
