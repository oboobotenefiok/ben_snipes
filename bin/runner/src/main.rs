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
//! EVM execution hasn't been built yet. A `dex-mock` demo venue is kept
//! alongside both so `cargo run` still demonstrates the full pipeline
//! end to end with synthetic data, independent of any real network
//! access or funded wallet.

use ben_snipes_adapter_dex_mock::{MockDexClient, MockDexSource};
use ben_snipes_adapter_evm_onchain::{
    EvmFactoryConfig, EvmFactoryLogSource, NotYetImplementedExchange as EvmNotYetImplementedExchange,
    NotYetImplementedMetrics as EvmNotYetImplementedMetrics,
};
use ben_snipes_adapter_pumpfun::{
    load_wallet, wallet_pubkey_string, DexScreenerMetricsProvider, NoWalletExchange,
    PumpPortalExchangeClient, PumpPortalSource, RugCheckSafetyChecker,
};
use ben_snipes_adapter_statefile::{FileAcquisitionLedger, FilePositionStore, StatefileStore};
use ben_snipes_application::{AcquisitionEngine, NewListingDetector, PositionManager, SafetyGate};
use ben_snipes_config::AppConfig;
use ben_snipes_domain::{
    AcquisitionCriteria, ListingMetrics, Position, ProfitTarget, SafetyCriteria, SafetyReport,
};
use ben_snipes_ports::{AcquisitionLedger, ExchangeClient, ListingSource, PositionStore};
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
                                if let Err(e) = position_store.save(&open_positions).await {
                                    warn!(error = %e, "failed to persist open positions after a buy - position is still tracked in memory this run");
                                }
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
                        Ok(Some(_filled_order)) => {
                            info!(symbol = position.symbol.as_str(), "take-profit reached, position closed");
                        }
                        Ok(None) => still_open.push(position),
                        Err(e) => {
                            warn!(symbol = position.symbol.as_str(), error = %e, "exit check failed, will retry next tick");
                            still_open.push(position);
                        }
                    }
                }
                open_positions = still_open;
                if let Err(e) = position_store.save(&open_positions).await {
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
