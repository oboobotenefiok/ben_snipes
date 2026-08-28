use crate::PortError;
use async_trait::async_trait;
use ben_snipes_domain::{Order, Symbol};
use rust_decimal::Decimal;

/// Trading operations against a single venue. A CEX adapter implements
/// this with signed REST calls; a DEX adapter implements it with signed
/// on-chain transactions (ideally routed through a private relay - see
/// the README for why that matters here). The application layer doesn't
/// need to know or care which.
#[async_trait]
pub trait ExchangeClient: Send + Sync {
    fn venue_name(&self) -> &str;

    /// Current price of `symbol` in the venue's quote asset.
    async fn current_price(&self, symbol: &Symbol) -> Result<Decimal, PortError>;

    /// Submit an order and return it with the venue's response applied
    /// (fill status, etc). Implementations are responsible for their own
    /// slippage/gas handling internally - the port only cares about intent
    /// in, result out.
    async fn submit_order(&self, order: Order) -> Result<Order, PortError>;
}
