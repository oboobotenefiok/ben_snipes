use ben_snipes_domain::{FilledSell, Order, OrderSide, Position};
use time::OffsetDateTime;
use ben_snipes_ports::{ExchangeClient, PortError};
use rust_decimal::Decimal;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::info;

/// Watches open positions and exits them when their take-profit target is reached.
/// Price reads are batched per venue so Solana/Jupiter is called once per cycle.
pub struct PositionManager {
    exchange: Arc<dyn ExchangeClient>,
}

#[derive(Debug)]
pub struct ExitResult {
    pub symbol: String,
    pub fill: FilledSell,
    pub reference_price: Decimal,
    pub closed_at: OffsetDateTime,
}

impl PositionManager {
    pub fn new(exchange: Arc<dyn ExchangeClient>) -> Self {
        Self { exchange }
    }

    pub async fn check_and_exit(&self, position: &Position) -> Result<Option<ExitResult>, PortError> {
        let mut exits = self.check_and_exit_batch(std::slice::from_ref(position)).await?;
        Ok(exits.pop())
    }

    /// Fetch all unique prices in one venue-specific batch, evaluate every
    /// position against its own target, then submit the required sells.
    pub async fn check_and_exit_batch(&self, positions: &[Position]) -> Result<Vec<ExitResult>, PortError> {
        if positions.is_empty() {
            return Ok(Vec::new());
        }

        let mut seen = HashSet::with_capacity(positions.len());
        let symbols: Vec<_> = positions
            .iter()
            .filter(|position| seen.insert(position.symbol.as_str().to_string()))
            .map(|position| position.symbol.clone())
            .collect();
        let prices = self.exchange.current_prices_batch(&symbols).await?;
        let mut exits = Vec::new();

        for position in positions {
            let current_price = prices.get(position.symbol.as_str()).copied().ok_or_else(|| {
                PortError::Rejected(format!("batched price response omitted {}", position.symbol.as_str()))
            })?;

            if !position.should_exit(current_price) {
                continue;
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
            self.exchange.preflight_sell(&order).await?;
            let fill = self.exchange.submit_order(order).await?;
            if fill.quantity <= Decimal::ZERO {
                return Err(PortError::Rejected(
                    "exit execution reported a non-positive filled quantity".to_string(),
                ));
            }
            if fill.quantity != position.quantity {
                return Err(PortError::Rejected(format!(
                    "exit execution was partial: requested={}, filled={}",
                    position.quantity, fill.quantity
                )));
            }

            exits.push(ExitResult {
                symbol: position.symbol.as_str().to_string(),
                fill,
                reference_price: current_price,
                closed_at: OffsetDateTime::now_utc(),
            });
        }

        Ok(exits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ben_snipes_domain::{FilledBuy, FilledSell, OrderStatus, ProfitTarget, Symbol, Venue, VenueKind};
    use std::collections::HashMap;

    struct StubExchange {
        price: Decimal,
        status: OrderStatus,
    }

    #[async_trait]
    impl ExchangeClient for StubExchange {
        fn venue_name(&self) -> &str { "stub" }

        async fn current_price(&self, _symbol: &Symbol) -> Result<Decimal, PortError> { Ok(self.price) }

        async fn current_prices_batch(&self, symbols: &[Symbol]) -> Result<HashMap<String, Decimal>, PortError> {
            Ok(symbols.iter().map(|symbol| (symbol.as_str().to_string(), self.price)).collect())
        }

        async fn submit_buy_by_amount(&self, _symbol: &Symbol, _quote_amount: Decimal) -> Result<FilledBuy, PortError> {
            unreachable!("PositionManager does not buy")
        }

        async fn submit_order(&self, order: Order) -> Result<FilledSell, PortError> {
            if self.status != OrderStatus::Filled {
                return Err(PortError::Rejected(format!("stub status={:?}", self.status)));
            }
            Ok(FilledSell {
                quantity: order.quantity,
                execution_price: Some(Decimal::from(111)),
                quote_proceeds: Some(Decimal::from(111) * order.quantity),
                fee_quote: None,
                tx_id: Some("stub-tx".to_string()),
            })
        }
    }

    fn sample_position() -> Position {
        let venue = Venue::new(VenueKind::Cex, "mexc").expect("literal venue is valid");
        let symbol = Symbol::new("PEPEUSDT").expect("literal symbol is valid");
        Position::new(venue, symbol, Decimal::from(100), Decimal::TEN, ProfitTarget::from_percent(Decimal::TEN).expect("valid target"))
    }

    #[tokio::test]
    async fn holds_below_target() {
        let manager = PositionManager::new(Arc::new(StubExchange { price: Decimal::from(102), status: OrderStatus::Filled }));
        assert!(manager.check_and_exit(&sample_position()).await.expect("stub exchange cannot fail").is_none());
    }

    #[tokio::test]
    async fn exits_on_take_profit() {
        let manager = PositionManager::new(Arc::new(StubExchange { price: Decimal::from(115), status: OrderStatus::Filled }));
        assert!(manager.check_and_exit(&sample_position()).await.expect("stub exchange cannot fail").is_some());
    }

    #[tokio::test]
    async fn does_not_treat_partial_fill_as_closed() {
        let manager = PositionManager::new(Arc::new(StubExchange { price: Decimal::from(115), status: OrderStatus::PartiallyFilled }));
        assert!(manager.check_and_exit(&sample_position()).await.is_err());
    }
}
