use ben_snipes_domain::{
    AcquisitionCriteria, Listing, Order, OrderSide, Position, ProfitTarget, SafetyCriteria,
    StopLoss,
};
use ben_snipes_ports::{ExchangeClient, MetricsProvider, PortError, TokenSafetyChecker};
use rust_decimal::Decimal;
use std::sync::Arc;
use tracing::{debug, info};

/// Bundles a `TokenSafetyChecker` with the `SafetyCriteria` it's judged
/// against. Kept as its own type (rather than two loose fields on
/// `AcquisitionEngine`) so the two can never be set independently of
/// each other - a checker with no criteria, or criteria with no
/// checker, isn't a state that should be representable.
///
/// Only construct this for venues where it's meaningful. A CEX venue
/// generally shouldn't have one at all - see the README for why.
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
/// with no human in the loop - this is the "it scans and enters and
/// exits on its own" piece of the spec.
///
/// The decision flow is deliberately linear and each step can bail out
/// cleanly with `Ok(None)`: no metrics yet, doesn't meet criteria,
/// fails the safety gate, and "bought successfully" are the only
/// outcomes, so callers never have to distinguish "we decided not to
/// buy" from "something went wrong" - only genuine I/O failures come
/// back as `Err`.
pub struct AcquisitionEngine {
    metrics_provider: Arc<dyn MetricsProvider>,
    exchange: Arc<dyn ExchangeClient>,
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
        criteria: AcquisitionCriteria,
        take_profit: ProfitTarget,
        stop_loss: StopLoss,
        position_size: Decimal,
        safety_gate: Option<SafetyGate>,
    ) -> Self {
        Self {
            metrics_provider,
            exchange,
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
                market_cap = %metrics.market_cap,
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

        let price = self.exchange.current_price(&listing.symbol).await?;
        if price <= Decimal::ZERO {
            debug!(symbol = listing.symbol.as_str(), "non-positive price quoted, skipping");
            return Ok(None);
        }

        let quantity = self.position_size / price;

        let order = Order::new(
            listing.venue.clone(),
            listing.symbol.clone(),
            OrderSide::Buy,
            quantity,
        )?;

        let filled = self.exchange.submit_order(order).await?;

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ben_snipes_domain::{ListingMetrics, OrderStatus, SafetyReport, Symbol, Venue, VenueKind};
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
            *self.orders_submitted.lock().await += 1;
            order.status = OrderStatus::Filled;
            Ok(order)
        }
    }

    fn sample_listing() -> Listing {
        let venue = Venue::new(VenueKind::Dex, "raydium-test").expect("literal venue is valid");
        let symbol = Symbol::new("NEWCOIN").expect("literal symbol is valid");
        Listing::new(symbol, venue, OffsetDateTime::UNIX_EPOCH)
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
    ) -> AcquisitionEngine {
        AcquisitionEngine::new(
            Arc::new(StubMetricsProvider { report: metrics }),
            exchange,
            AcquisitionCriteria::new(Decimal::from(50_000))
                .expect("literal criteria is valid"),
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
        });
        let engine = build_engine(Some(passing_metrics()), None, exchange.clone());

        let result = engine
            .evaluate_and_buy(&sample_listing())
            .await
            .expect("stub dependencies cannot fail");

        assert!(result.is_some());
        assert_eq!(*exchange.orders_submitted.lock().await, 1);
    }

    #[tokio::test]
    async fn skips_when_safety_report_is_not_yet_available() {
        let exchange = Arc::new(StubExchange {
            price: Decimal::ONE,
            orders_submitted: Mutex::new(0),
        });
        let gate = SafetyGate::new(
            Arc::new(StubSafetyChecker { report: None }),
            SafetyCriteria::new(1_000),
        );
        let engine = build_engine(Some(passing_metrics()), Some(gate), exchange.clone());

        let result = engine
            .evaluate_and_buy(&sample_listing())
            .await
            .expect("stub dependencies cannot fail");

        assert!(result.is_none());
        assert_eq!(*exchange.orders_submitted.lock().await, 0);
    }

    #[tokio::test]
    async fn skips_a_listing_that_fails_the_safety_gate() {
        let exchange = Arc::new(StubExchange {
            price: Decimal::ONE,
            orders_submitted: Mutex::new(0),
        });
        let dangerous_report = SafetyReport {
            sell_tax_bps: 9_000,
            ownership_renounced: false,
            liquidity_locked: false,
            is_mintable: true,
        };
        let gate = SafetyGate::new(
            Arc::new(StubSafetyChecker {
                report: Some(dangerous_report),
            }),
            SafetyCriteria::new(1_000),
        );
        let engine = build_engine(Some(passing_metrics()), Some(gate), exchange.clone());

        let result = engine
            .evaluate_and_buy(&sample_listing())
            .await
            .expect("stub dependencies cannot fail");

        assert!(result.is_none());
        assert_eq!(*exchange.orders_submitted.lock().await, 0);
    }

    #[tokio::test]
    async fn buys_when_safety_report_passes() {
        let exchange = Arc::new(StubExchange {
            price: Decimal::ONE,
            orders_submitted: Mutex::new(0),
        });
        let clean_report = SafetyReport {
            sell_tax_bps: 100,
            ownership_renounced: true,
            liquidity_locked: true,
            is_mintable: false,
        };
        let gate = SafetyGate::new(
            Arc::new(StubSafetyChecker {
                report: Some(clean_report),
            }),
            SafetyCriteria::new(1_000),
        );
        let engine = build_engine(Some(passing_metrics()), Some(gate), exchange.clone());

        let result = engine
            .evaluate_and_buy(&sample_listing())
            .await
            .expect("stub dependencies cannot fail");

        assert!(result.is_some());
        assert_eq!(*exchange.orders_submitted.lock().await, 1);
    }
}
