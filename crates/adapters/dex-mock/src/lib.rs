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
use ben_snipes_domain::{Listing, ListingMetrics, Order, OrderStatus, SafetyReport, Symbol, Venue, VenueKind};
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
        let pools = self.pools.lock().await;

        let mut new_listings = Vec::new();
        let mut latest_block = since_block;

        for pool in pools.iter().filter(|p| p.block > since_block) {
            let symbol = Symbol::new(pool.symbol.clone())?;
            new_listings.push(Listing::new(symbol, venue.clone(), OffsetDateTime::now_utc()));
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
