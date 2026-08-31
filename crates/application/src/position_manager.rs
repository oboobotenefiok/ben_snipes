use ben_snipes_domain::{Order, OrderSide, Position};
use ben_snipes_ports::{ExchangeClient, PortError};
use std::sync::Arc;
use tracing::info;

/// Watches a single open position and exits it once the take-profit
/// target is reached. There is no stop-loss in this bot, by explicit
/// design: a position is held until it hits +10% (or whatever
/// `risk.take_profit_percent` is configured to), however long that
/// takes - it never exits at a loss.
pub struct PositionManager {
    exchange: Arc<dyn ExchangeClient>,
}

impl PositionManager {
    pub fn new(exchange: Arc<dyn ExchangeClient>) -> Self {
        Self { exchange }
    }

    /// Checks the current price against the position's take-profit
    /// target. Returns `Some(order)` if an exit order was submitted,
    /// `None` if the target hasn't been reached yet - which, absent a
    /// stop-loss, just means "keep holding".
    pub async fn check_and_exit(&self, position: &Position) -> Result<Option<Order>, PortError> {
        let current_price = self.exchange.current_price(&position.symbol).await?;

        if !position.should_exit(current_price) {
            return Ok(None);
        }

        info!(
            symbol = position.symbol.as_str(),
            entry = %position.entry_price,
            current = %current_price,
            "take-profit target reached, submitting exit order"
        );

        let order = Order::new(
            position.venue.clone(),
            position.symbol.clone(),
            OrderSide::Sell,
            position.quantity,
        )?;

        let filled = self.exchange.submit_order(order).await?;
        Ok(Some(filled))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ben_snipes_domain::{FilledBuy, OrderStatus, ProfitTarget, Symbol, Venue, VenueKind};
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

        async fn submit_buy_by_amount(&self, _symbol: &Symbol, _quote_amount: Decimal) -> Result<FilledBuy, PortError> {
            unreachable!("PositionManager only ever calls current_price/submit_order, never submit_buy_by_amount")
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
        )
    }

    #[tokio::test]
    async fn holds_below_target() {
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
    async fn holds_even_on_a_large_price_drop() {
        // The whole point of dropping stop-loss: no price, however low,
        // triggers an exit on its own.
        let manager = PositionManager::new(Arc::new(StubExchange {
            price: Decimal::from(10),
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

        let result = manager
            .check_and_exit(&sample_position())
            .await
            .expect("stub exchange cannot fail");
        assert!(result.is_some());
    }
}
