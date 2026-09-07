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
    pub max_token_transfer_fee_bps: u32,
    pub take_profit_percent: Decimal,
    pub position_size: Decimal,
    pub max_open_positions: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestTrade {
    pub trade: TradeRecord,
    pub opened_at: OffsetDateTime,
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
            let mut remaining = Vec::with_capacity(open_positions.len());
            for (position, opened_at) in open_positions.drain(..) {
                if position.should_exit(event.price) {
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
}
