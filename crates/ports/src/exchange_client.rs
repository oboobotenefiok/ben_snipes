use crate::PortError;
use async_trait::async_trait;
use ben_snipes_domain::{FilledBuy, Order, Symbol};
use rust_decimal::Decimal;

/// Trading operations against a single venue. A CEX adapter implements
/// this with signed REST calls; a DEX adapter implements it with signed
/// on-chain transactions (ideally routed through a private relay - see
/// the README for why that matters here). The application layer doesn't
/// need to know or care which.
#[async_trait]
pub trait ExchangeClient: Send + Sync {
    fn venue_name(&self) -> &str;

    /// Current price of `symbol` in the venue's quote asset. Used for
    /// exit monitoring (deciding *when* a position has crossed its
    /// take-profit/stop-loss) - not for entry sizing, see
    /// `submit_buy_by_amount`.
    async fn current_price(&self, symbol: &Symbol) -> Result<Decimal, PortError>;

    /// Buys `symbol` by spending `quote_amount` of the venue's quote
    /// asset (e.g. SOL, USDT), and reports back what was actually
    /// acquired. This is the entry point for opening a position -
    /// deliberately amount-based rather than quantity-based, because
    /// venues without a queryable pre-trade order book (a bonding-curve
    /// DEX, for instance) can't offer a quantity-for-a-given-price quote
    /// the way a CEX can. A CEX-style adapter that *does* have a live
    /// order book is free to fetch its own price internally and convert;
    /// the port doesn't force that round-trip on venues that don't need
    /// it.
    async fn submit_buy_by_amount(&self, symbol: &Symbol, quote_amount: Decimal) -> Result<FilledBuy, PortError>;

    /// Submit an order and return it with the venue's response applied
    /// (fill status, etc). In practice this is the exit/sell path - the
    /// quantity being sold is already known (it's the position being
    /// closed), which is why selling stays quantity-based even on
    /// venues where buying is amount-based. Implementations are
    /// responsible for their own slippage/gas handling internally - the
    /// port only cares about intent in, result out.
    async fn submit_order(&self, order: Order) -> Result<Order, PortError>;
}
