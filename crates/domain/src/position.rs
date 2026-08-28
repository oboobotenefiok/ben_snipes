use crate::{DomainError, Symbol, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// A take-profit rule expressed as a percentage above entry price, e.g.
/// `ProfitTarget::from_percent(10)` for "sell at +10%".
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

/// A stop-loss rule expressed as a percentage below entry price, e.g.
/// `StopLoss::from_percent(5)` for "sell at -5%".
///
/// This exists because a take-profit target alone is a one-way bet: a
/// position that never reaches +10% just sits there forever, and a
/// single bad listing (a slow rug, a dead market with no buyers left)
/// can erase the gains from several good ones. Every position gets both
/// a ceiling and a floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopLoss(Decimal);

impl StopLoss {
    pub fn from_percent(percent: Decimal) -> Result<Self, DomainError> {
        if percent <= Decimal::ZERO {
            return Err(DomainError::InvalidStopLoss(percent.to_string()));
        }
        Ok(Self(percent))
    }

    pub fn percent(&self) -> Decimal {
        self.0
    }

    pub fn exit_price(&self, entry_price: Decimal) -> Decimal {
        entry_price - (entry_price * self.0 / Decimal::ONE_HUNDRED)
    }

    pub fn is_triggered(&self, entry_price: Decimal, current_price: Decimal) -> bool {
        current_price <= self.exit_price(entry_price)
    }
}

/// Why a position was (or should be) closed. Kept separate from a plain
/// `bool` so `PositionManager` can log and act on the actual reason
/// rather than just "something said sell" - a take-profit and a
/// stop-loss are very different outcomes worth telling apart in logs
/// and, eventually, in P&L reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    TakeProfit,
    StopLoss,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub venue: Venue,
    pub symbol: Symbol,
    pub entry_price: Decimal,
    pub quantity: Decimal,
    pub target: ProfitTarget,
    pub stop_loss: StopLoss,
}

impl Position {
    pub fn new(
        venue: Venue,
        symbol: Symbol,
        entry_price: Decimal,
        quantity: Decimal,
        target: ProfitTarget,
        stop_loss: StopLoss,
    ) -> Self {
        Self {
            venue,
            symbol,
            entry_price,
            quantity,
            target,
            stop_loss,
        }
    }

    /// Checks the take-profit first, then the stop-loss. If somehow both
    /// would trigger on the same price read (only possible with a
    /// pathological config where the stop-loss percent exceeds the
    /// take-profit percent), take-profit wins - exiting at a gain is
    /// never the wrong call.
    pub fn exit_reason(&self, current_price: Decimal) -> Option<ExitReason> {
        if self.target.is_reached(self.entry_price, current_price) {
            Some(ExitReason::TakeProfit)
        } else if self.stop_loss.is_triggered(self.entry_price, current_price) {
            Some(ExitReason::StopLoss)
        } else {
            None
        }
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

    #[test]
    fn five_percent_stop_loss_computes_correct_exit_price() {
        let stop_loss =
            StopLoss::from_percent(Decimal::from(5)).expect("five percent is a valid stop-loss");
        let exit = stop_loss.exit_price(Decimal::ONE_HUNDRED);
        assert_eq!(exit, Decimal::from(95));
    }

    #[test]
    fn rejects_non_positive_stop_loss() {
        assert!(StopLoss::from_percent(Decimal::ZERO).is_err());
    }

    fn sample_position() -> Position {
        let venue = crate::Venue::new(crate::VenueKind::Cex, "mexc").expect("literal venue is valid");
        let symbol = crate::Symbol::new("PEPEUSDT").expect("literal symbol is valid");
        Position::new(
            venue,
            symbol,
            Decimal::ONE_HUNDRED,
            Decimal::TEN,
            ProfitTarget::from_percent(Decimal::TEN).expect("valid target"),
            StopLoss::from_percent(Decimal::from(5)).expect("valid stop-loss"),
        )
    }

    #[test]
    fn exit_reason_is_none_between_the_two_thresholds() {
        let position = sample_position();
        assert_eq!(position.exit_reason(Decimal::from(102)), None);
    }

    #[test]
    fn exit_reason_is_take_profit_at_or_above_target() {
        let position = sample_position();
        assert_eq!(position.exit_reason(Decimal::from(110)), Some(ExitReason::TakeProfit));
    }

    #[test]
    fn exit_reason_is_stop_loss_at_or_below_floor() {
        let position = sample_position();
        assert_eq!(position.exit_reason(Decimal::from(95)), Some(ExitReason::StopLoss));
    }
}
