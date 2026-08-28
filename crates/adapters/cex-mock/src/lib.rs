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
