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
