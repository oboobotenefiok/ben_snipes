use crate::{DomainError, Symbol, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderStatus {
    Pending,
    Filled,
    PartiallyFilled,
    Rejected,
    Cancelled,
}

/// A single buy or sell instruction. Adapters translate this into
/// whatever the venue actually needs (a signed REST payload for a CEX, a
/// signed transaction for a DEX) - the domain only cares about the intent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Order {
    pub venue: Venue,
    pub symbol: Symbol,
    pub side: OrderSide,
    pub quantity: Decimal,
    pub status: OrderStatus,
}

impl Order {
    pub fn new(
        venue: Venue,
        symbol: Symbol,
        side: OrderSide,
        quantity: Decimal,
    ) -> Result<Self, DomainError> {
        if quantity <= Decimal::ZERO {
            return Err(DomainError::InvalidQuantity(quantity.to_string()));
        }
        Ok(Self {
            venue,
            symbol,
            side,
            quantity,
            status: OrderStatus::Pending,
        })
    }
}

/// The result of an amount-based buy (see `ExchangeClient::submit_buy_by_amount`):
/// how many units were actually acquired, and the effective price that
/// implies. Unlike a quantity-based `Order`, neither of these is known
/// until *after* the trade executes - a venue like a bonding-curve DEX
/// doesn't expose a pre-trade quote the way a CEX order book does, so
/// the caller spends a known amount and finds out what it bought.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FilledBuy {
    pub quantity: Decimal,
    pub entry_price: Decimal,
}

/// Settlement metadata returned after a quantity-based sell.
///
/// DEXes do not always expose the final quote proceeds through the same
/// interface that submits the transaction, so exact settlement fields are
/// optional. A transaction id is still recorded whenever the venue exposes
/// one, which makes the journal auditable without pretending a pre-trade
/// quote was an on-chain fill.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilledSell {
    pub quantity: Decimal,
    pub execution_price: Option<Decimal>,
    pub quote_proceeds: Option<Decimal>,
    pub fee_quote: Option<Decimal>,
    pub tx_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VenueKind;

    #[test]
    fn rejects_zero_or_negative_quantity() {
        let venue = Venue::new(VenueKind::Cex, "mexc").expect("literal name is valid");
        let symbol = Symbol::new("PEPEUSDT").expect("literal symbol is valid");
        let result = Order::new(venue, symbol, OrderSide::Buy, Decimal::ZERO);
        assert!(result.is_err());
    }
}
