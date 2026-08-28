//! Composition root for ben_snipes. This is where concrete adapters
//! (currently mocks - see the README for what a real integration needs)
//! get wired into the traits that `ben_snipes_application` depends on,
//! and where the actual scan -> filter -> buy -> hold -> exit loop runs.
//!
//! Everything upstream of this file (domain, ports, application) has no
//! idea these adapters are mocks. Swapping `MockCexSource` for a real
//! MEXC adapter means changing the lines in `build_venues` below that
//! construct it - nothing else in the workspace needs to know.

use ben_snipes_adapter_cex_mock::{MockCexClient, MockCexSource};
use ben_snipes_adapter_dex_mock::{MockDexClient, MockDexSource};
use ben_snipes_adapter_statefile::StatefileStore;
use ben_snipes_application::{AcquisitionEngine, NewListingDetector, PositionManager, SafetyGate};
use ben_snipes_config::AppConfig;
use ben_snipes_domain::{
    AcquisitionCriteria, ListingMetrics, Position, ProfitTarget, SafetyCriteria, SafetyReport,
    StopLoss,
};
use ben_snipes_ports::ListingSource;
use rust_decimal::Decimal;
use std::fmt::Display;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// Everything needed to watch one venue end-to-end: detect new listings
/// on it, decide whether to buy them, and later check any resulting
/// position for exit. Bundling these together (rather than three
/// separate parallel collections keyed by venue name) is what lets the
/// exit-checking loop find the right `PositionManager` for a position
/// without a lookup table that can silently drift out of sync.
struct VenueHandle {
    source: Box<dyn ListingSource>,
    acquisition: AcquisitionEngine,
    position_manager: PositionManager,
}

/// Config values that violate a domain rule (e.g. a zero take-profit
/// percent) are a startup-time problem, not a recoverable one - we can't
/// meaningfully run without valid risk parameters. This prints a clear
/// reason and exits, the same pattern `AppConfig::load` uses, rather
/// than an `.expect()` that would just print a bare panic message.
fn expect_valid_config<T, E: Display>(result: Result<T, E>, what: &str) -> T {
    match result {
        Ok(value) => value,
        Err(e) => {
            eprintln!("invalid {what} in config: {e}");
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

async fn build_venues(config: &AppConfig, risk: &RiskParams) -> Vec<VenueHandle> {
    // --- CEX demo venue -----------------------------------------------
    // Mimics an exchange that only exposes "list all tradable symbols"
    // (no cursor), so it exercises the full-snapshot-diff path.
    //
    // No SafetyGate here: a CEX listing can't be an unsellable honeypot
    // contract the way a DEX token can, because the exchange's own order
    // book controls execution, not a smart contract the token author
    // wrote. If you're integrating a real CEX and want an extra layer of
    // caution anyway (e.g. against a spoofed/compromised listing feed),
    // it's still a legitimate SafetyGate to add per-venue.
    let cex_client = Arc::new(MockCexClient::new("mexc-demo", Decimal::ONE));
    let cex_source = MockCexSource::new(
        "mexc-demo",
        vec!["BTCUSDT".to_string(), "ETHUSDT".to_string()],
    );

    // Seed metrics for a symbol we'll simulate as newly listed below.
    // Deliberately a high market cap (50M) paired with strong volume, to
    // demonstrate the current rule: active volume is what qualifies a
    // listing, not a low market cap - a real adapter would report both
    // from the exchange's own 24h ticker data, not a hand-set value.
    cex_client
        .set_metrics(
            "NEWTOKENUSDT",
            ListingMetrics {
                volume_24h: Decimal::from(120_000),
                market_cap: Decimal::from(50_000_000),
            },
        )
        .await;

    let cex_handle = VenueHandle {
        acquisition: AcquisitionEngine::new(
            cex_client.clone(),
            cex_client.clone(),
            risk.criteria,
            risk.take_profit,
            risk.stop_loss,
            config.risk.max_position_size,
            None, // no safety gate for CEX - see comment above
        ),
        position_manager: PositionManager::new(cex_client.clone()),
        source: Box::new(cex_source),
    };

    // --- DEX demo venue -------------------------------------------------
    // Mimics a venue with cursor-based incremental fetching (a block
    // number standing in for "pools created after block N"), so it
    // exercises the Incremental path instead. This is where honeypot/rug
    // risk is real, so it gets a SafetyGate.
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

    // A clean-looking safety report for the demo token: low sell tax,
    // ownership renounced, liquidity locked, not mintable. A real
    // adapter would get this by simulating a sell against the token
    // contract and reading its on-chain state, not a hand-set value.
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

    // Purely for demo visibility - simulates a pool appearing so the
    // first poll after the CEX baseline has something on the DEX side to
    // detect and buy. Remove once a real chain adapter is wired in.
    dex_source.simulate_new_pool("NEWCOIN-SOL").await;

    let dex_safety_gate = SafetyGate::new(dex_client.clone(), risk.safety_criteria);

    let dex_handle = VenueHandle {
        acquisition: AcquisitionEngine::new(
            dex_client.clone(),
            dex_client.clone(),
            risk.criteria,
            risk.take_profit,
            risk.stop_loss,
            config.risk.max_position_size,
            Some(dex_safety_gate),
        ),
        position_manager: PositionManager::new(dex_client.clone()),
        source: Box::new(dex_source),
    };

    vec![cex_handle, dex_handle]
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = match AppConfig::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            // eprintln, not tracing::error: the subscriber's own config
            // might be what's broken, so this is the one place we can't
            // yet assume tracing works.
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
        poll_interval_seconds = config.risk.poll_interval_seconds,
        "ben_snipes starting up"
    );

    let state_store = Arc::new(StatefileStore::new(&config.storage.state_dir));
    let detector = NewListingDetector::new(state_store);
    let venues = build_venues(&config, &risk).await;

    let mut open_positions: Vec<Position> = Vec::new();
    let mut interval = tokio::time::interval(Duration::from_secs(config.risk.poll_interval_seconds));
    let mut shutdown = std::pin::pin!(tokio::signal::ctrl_c());

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // 1. Scan every venue for newly-appeared listings and
                //    autonomously buy the ones that pass acquisition
                //    criteria and (where configured) the safety gate.
                for venue in &venues {
                    let new_listings = match detector.poll(venue.source.as_ref()).await {
                        Ok(listings) => listings,
                        Err(e) => {
                            warn!(source = venue.source.source_id(), error = %e, "poll failed, will retry next tick");
                            continue;
                        }
                    };

                    for listing in new_listings {
                        info!(symbol = listing.symbol.as_str(), venue = %listing.venue, "new listing detected");

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
