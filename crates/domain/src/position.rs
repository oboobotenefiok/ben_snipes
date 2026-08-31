use crate::{DomainError, Symbol, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// A take-profit rule expressed as a percentage above entry price, e.g.
/// `ProfitTarget::from_percent(10)` for "sell at +10%".
///
/// This is the *only* exit condition this bot uses, by design: it holds
/// a position until the target is reached, however long that takes,
/// rather than cutting losses early. That's a deliberate strategy
/// choice, not an oversight - see `Position::should_exit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfitTarget(Decimal);

impl ProfitTarget {
    pub fn from_percent(percent: Decimal) -> Result<Self, DomainError> {
        if percent <= Decimal::ZERO {
            return Err(DomainError::InvalidProfitTarget(percent.to_string()));
        }
        Ok(Self(percent))
    }

    pub fn percent(&self) -> Decimal {
        self.0
    }

    /// Given an entry price, what exit price hits this target.
    pub fn exit_price(&self, entry_price: Decimal) -> Decimal {
        entry_price + (entry_price * self.0 / Decimal::ONE_HUNDRED)
    }

    /// Whether the current price has reached this target relative to the
    /// given entry price.
    pub fn is_reached(&self, entry_price: Decimal, current_price: Decimal) -> bool {
        current_price >= self.exit_price(entry_price)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub venue: Venue,
    pub symbol: Symbol,
    pub entry_price: Decimal,
    pub quantity: Decimal,
    pub target: ProfitTarget,
}

impl Position {
    pub fn new(venue: Venue, symbol: Symbol, entry_price: Decimal, quantity: Decimal, target: ProfitTarget) -> Self {
        Self {
            venue,
            symbol,
            entry_price,
            quantity,
            target,
        }
    }

    /// The single exit condition: has this position reached its
    /// take-profit target. There is no stop-loss - this bot holds until
    /// the target is reached, full stop, by explicit design.
    pub fn should_exit(&self, current_price: Decimal) -> bool {
        self.target.is_reached(self.entry_price, current_price)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_percent_target_computes_correct_exit_price() {
        let target =
            ProfitTarget::from_percent(Decimal::TEN).expect("ten percent is a valid target");
        let exit = target.exit_price(Decimal::ONE_HUNDRED);
        assert_eq!(exit, Decimal::from(110));
    }

    #[test]
    fn rejects_non_positive_target() {
        assert!(ProfitTarget::from_percent(Decimal::ZERO).is_err());
    }

    fn sample_position() -> Position {
        let venue = crate::Venue::new(crate::VenueKind::Dex, "pumpfun").expect("literal venue is valid");
        let symbol = crate::Symbol::new("PEPEUSDT").expect("literal symbol is valid");
        Position::new(
            venue,
            symbol,
            Decimal::ONE_HUNDRED,
            Decimal::TEN,
            ProfitTarget::from_percent(Decimal::TEN).expect("valid target"),
        )
    }

    #[test]
    fn does_not_exit_below_target() {
        let position = sample_position();
        assert!(!position.should_exit(Decimal::from(105)));
    }

    #[test]
    fn does_not_exit_far_below_entry_either() {
        // The whole point: no stop-loss. A price crash doesn't trigger
        // an exit - only reaching the take-profit target does.
        let position = sample_position();
        assert!(!position.should_exit(Decimal::from(10)));
    }

    #[test]
    fn exits_at_or_above_target() {
        let position = sample_position();
        assert!(position.should_exit(Decimal::from(110)));
    }
}
