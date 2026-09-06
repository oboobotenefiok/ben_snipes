use ben_snipes_domain::{FilledBuy, FilledSell, Order, Symbol};
use ben_snipes_ports::{ExchangeClient, PortError};
use rust_decimal::Decimal;
use std::sync::Arc;
use tracing::info;

/// An execution adapter that preserves real market-data reads while never
/// submitting a live order. Buys are priced from the venue's current-price
/// port and sells are recorded as filled at the application's requested
/// quantity. This makes the strategy executable end to end without a funded
/// wallet or a signing key.
pub struct PaperExchange {
    inner: Arc<dyn ExchangeClient>,
}

impl PaperExchange {
    pub fn new(inner: Arc<dyn ExchangeClient>) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl ExchangeClient for PaperExchange {
    fn venue_name(&self) -> &str {
        self.inner.venue_name()
    }

    async fn current_price(&self, symbol: &Symbol) -> Result<Decimal, PortError> {
        self.inner.current_price(symbol).await
    }

    async fn submit_buy_by_amount(
        &self,
        symbol: &Symbol,
        quote_amount: Decimal,
    ) -> Result<FilledBuy, PortError> {
        if quote_amount <= Decimal::ZERO {
            return Err(PortError::Rejected("paper buy amount must be positive".to_string()));
        }
        let price = self.current_price(symbol).await?;
        if price <= Decimal::ZERO {
            return Err(PortError::Rejected("paper price must be positive".to_string()));
        }
        let quantity = quote_amount / price;
        info!(
            symbol = symbol.as_str(),
            quote_amount = %quote_amount,
            price = %price,
            quantity = %quantity,
            "paper buy simulated; no transaction submitted"
        );
        Ok(FilledBuy {
            quantity,
            entry_price: price,
        })
    }

    async fn submit_order(&self, order: Order) -> Result<FilledSell, PortError> {
        let price = self.current_price(&order.symbol).await?;
        if price <= Decimal::ZERO {
            return Err(PortError::Rejected("paper price must be positive".to_string()));
        }
        info!(
            symbol = order.symbol.as_str(),
            quantity = %order.quantity,
            price = %price,
            "paper sell simulated; no transaction submitted"
        );
        Ok(FilledSell {
            quantity: order.quantity,
            execution_price: Some(price),
            quote_proceeds: Some(price * order.quantity),
            fee_quote: None,
            tx_id: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ben_snipes_domain::{OrderSide, Venue, VenueKind};

    struct StubExchange;

    #[async_trait]
    impl ExchangeClient for StubExchange {
        fn venue_name(&self) -> &str { "paper-test" }

        async fn current_price(&self, _symbol: &Symbol) -> Result<Decimal, PortError> {
            Ok(Decimal::from(2))
        }

        async fn submit_buy_by_amount(&self, _symbol: &Symbol, _quote_amount: Decimal) -> Result<FilledBuy, PortError> {
            Err(PortError::Rejected("underlying execution should never be called".to_string()))
        }

        async fn submit_order(&self, _order: Order) -> Result<FilledSell, PortError> {
            Err(PortError::Rejected("underlying execution should never be called".to_string()))
        }
    }

    #[tokio::test]
    async fn paper_buy_uses_read_only_price_and_does_not_execute() {
        let exchange = PaperExchange::new(Arc::new(StubExchange));
        let symbol = match Symbol::new("0x0000000000000000000000000000000000000001") {
            Ok(symbol) => symbol,
            Err(error) => panic!("test symbol should be valid: {error}"),
        };
        let filled = match exchange.submit_buy_by_amount(&symbol, Decimal::from(10)).await {
            Ok(filled) => filled,
            Err(error) => panic!("paper execution should succeed: {error}"),
        };
        assert_eq!(filled.entry_price, Decimal::from(2));
        assert_eq!(filled.quantity, Decimal::from(5));
    }

    #[tokio::test]
    async fn paper_sell_returns_a_filled_order_without_execution() {
        let exchange = PaperExchange::new(Arc::new(StubExchange));
        let venue = match Venue::new(VenueKind::Dex, "paper-test") {
            Ok(venue) => venue,
            Err(error) => panic!("test venue should be valid: {error}"),
        };
        let symbol = match Symbol::new("0x0000000000000000000000000000000000000001") {
            Ok(symbol) => symbol,
            Err(error) => panic!("test symbol should be valid: {error}"),
        };
        let order = match Order::new(venue, symbol, OrderSide::Sell, Decimal::ONE) {
            Ok(order) => order,
            Err(error) => panic!("test order should be valid: {error}"),
        };
        let filled = match exchange.submit_order(order).await {
            Ok(filled) => filled,
            Err(error) => panic!("paper sell should succeed: {error}"),
        };
        assert_eq!(filled.quantity, Decimal::ONE);
        assert_eq!(filled.execution_price, Some(Decimal::ONE));
        assert_eq!(filled.quote_proceeds, Some(Decimal::ONE));
    }
}
