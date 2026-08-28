use ben_snipes_domain::{ExitReason, Order, OrderSide, Position};
use ben_snipes_ports::{ExchangeClient, PortError};
use std::sync::Arc;
use tracing::info;

/// Watches a single open position and exits it once either the
/// take-profit target or the stop-loss floor is reached - see
/// `Position::exit_reason` for which one wins if both would somehow
/// trigger on the same price read.
pub struct PositionManager {
    exchange: Arc<dyn ExchangeClient>,
}

impl PositionManager {
    pub fn new(exchange: Arc<dyn ExchangeClient>) -> Self {
        Self { exchange }
    }

    /// Checks the current price against the position's take-profit and
    /// stop-loss. Returns `Some((order, reason))` if an exit order was
    /// submitted, `None` if neither threshold has been reached yet.
    pub async fn check_and_exit(
        &self,
        position: &Position,
    ) -> Result<Option<(Order, ExitReason)>, PortError> {
        let current_price = self.exchange.current_price(&position.symbol).await?;

        let Some(reason) = position.exit_reason(current_price) else {
            return Ok(None);
        };

        info!(
            symbol = position.symbol.as_str(),
            entry = %position.entry_price,
            current = %current_price,
            reason = ?reason,
            "exit threshold reached, submitting exit order"
        );

        let order = Order::new(
            position.venue.clone(),
            position.symbol.clone(),
            OrderSide::Sell,
            position.quantity,
        )?;

        let filled = self.exchange.submit_order(order).await?;
        Ok(Some((filled, reason)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ben_snipes_domain::{OrderStatus, ProfitTarget, StopLoss, Symbol, Venue, VenueKind};
    use rust_decimal::Decimal;

    struct StubExchange {
        price: Decimal,
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
            order.status = OrderStatus::Filled;
            Ok(order)
        }
    }

    fn sample_position() -> Position {
        let venue = Venue::new(VenueKind::Cex, "mexc").expect("literal venue is valid");
        let symbol = Symbol::new("PEPEUSDT").expect("literal symbol is valid");
        Position::new(
            venue,
            symbol,
            Decimal::ONE_HUNDRED,
            Decimal::TEN,
            ProfitTarget::from_percent(Decimal::TEN).expect("valid target"),
            StopLoss::from_percent(Decimal::from(5)).expect("valid stop-loss"),
        )
    }

    #[tokio::test]
    async fn holds_when_price_is_between_thresholds() {
        let manager = PositionManager::new(Arc::new(StubExchange {
            price: Decimal::from(102),
        }));

        let result = manager
            .check_and_exit(&sample_position())
            .await
            .expect("stub exchange cannot fail");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn exits_on_take_profit() {
        let manager = PositionManager::new(Arc::new(StubExchange {
            price: Decimal::from(115),
        }));

        let (_order, reason) = manager
            .check_and_exit(&sample_position())
            .await
            .expect("stub exchange cannot fail")
            .expect("price is above target, should exit");
        assert_eq!(reason, ExitReason::TakeProfit);
    }

    #[tokio::test]
    async fn exits_on_stop_loss() {
        let manager = PositionManager::new(Arc::new(StubExchange {
            price: Decimal::from(90),
        }));

        let (_order, reason) = manager
            .check_and_exit(&sample_position())
            .await
            .expect("stub exchange cannot fail")
            .expect("price is below floor, should exit");
        assert_eq!(reason, ExitReason::StopLoss);
    }
}
