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
            return Ok(None);
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

        Ok(Some(position))
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
        Chain, FilledBuy, ListingMetrics, Order, OrderStatus, SafetyReport, Symbol, Venue, VenueKind,
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

        async fn submit_order(&self, mut order: Order) -> Result<Order, PortError> {
            order.status = OrderStatus::Filled;
            Ok(order)
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

        assert!(result.is_some());
        assert_eq!(*exchange.buys_submitted.lock().await, 1);
    }

    #[tokio::test]
    async fn skips_a_listing_that_fails_the_safety_gate() {
        let exchange = Arc::new(StubExchange {
            buys_submitted: Mutex::new(0),
            fail_buy: false,
        });
        let dangerous_report = SafetyReport {
            sell_tax_bps: 9_000,
            ownership_renounced: false,
            liquidity_locked: false,
            is_mintable: true,
        };
        let gate = SafetyGate::new(
            Arc::new(StubSafetyChecker { report: Some(dangerous_report) }),
            SafetyCriteria::new(1_000),
        );
        let engine = build_engine(Some(passing_metrics()), Some(gate), exchange.clone(), Arc::new(InMemoryLedger::empty()));

        let result = engine
            .evaluate_and_buy(&sample_listing())
            .await
            .expect("stub dependencies cannot fail");

        assert!(result.is_none());
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

        assert!(first.is_some());
        assert!(second.is_none());
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
