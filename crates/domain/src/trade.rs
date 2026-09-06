use crate::{FilledSell, Symbol, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// A completed position plus the settlement information available from the
/// venue. Some adapters can provide exact execution metadata while others
/// only expose confirmation and a transaction id, so reference-price
/// accounting is explicitly marked instead of being presented as exact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradeRecord {
    pub venue: Venue,
    pub symbol: Symbol,
    pub quantity: Decimal,
    pub entry_price: Decimal,
    pub exit_price: Decimal,
    pub quote_cost: Decimal,
    pub quote_proceeds: Decimal,
    #[serde(default)]
    pub fee_quote: Decimal,
    pub pnl: Decimal,
    pub closed_at: OffsetDateTime,
    #[serde(default)]
    pub execution_price_is_reference: bool,
    #[serde(default)]
    pub tx_id: Option<String>,
}

impl TradeRecord {
    /// Stable identity for one position lifecycle. The acquisition ledger
    /// prevents the same canonical token from being opened twice, so this
    /// key also lets recovery treat an already-journaled close as idempotent.
    pub fn key(&self) -> String {
        format!(
            "{}::{}::{}::{}",
            self.venue,
            self.symbol,
            self.entry_price,
            self.quantity
        )
    }

    pub fn key_for_position(position: &crate::Position) -> String {
        format!(
            "{}::{}::{}::{}",
            position.venue,
            position.symbol,
            position.entry_price,
            position.quantity
        )
    }

    pub fn from_position(
        position: &crate::Position,
        exit_price: Decimal,
        closed_at: OffsetDateTime,
    ) -> Self {
        let quote_cost = position.entry_price * position.quantity;
        let quote_proceeds = exit_price * position.quantity;
        let pnl = quote_proceeds - quote_cost;

        Self {
            venue: position.venue.clone(),
            symbol: position.symbol.clone(),
            quantity: position.quantity,
            entry_price: position.entry_price,
            exit_price,
            quote_cost,
            quote_proceeds,
            fee_quote: Decimal::ZERO,
            pnl,
            closed_at,
            execution_price_is_reference: true,
            tx_id: None,
        }
    }

    /// Builds a journal record from venue settlement metadata. If the venue
    /// does not expose an execution price or quote proceeds, the supplied
    /// reference price is used and the record is explicitly marked as such.
    pub fn from_fill(
        position: &crate::Position,
        fill: &FilledSell,
        reference_price: Decimal,
        closed_at: OffsetDateTime,
    ) -> Self {
        let execution_price = fill.execution_price.unwrap_or(reference_price);
        let quote_cost = position.entry_price * position.quantity;
        let quote_proceeds = fill
            .quote_proceeds
            .unwrap_or_else(|| execution_price * fill.quantity);
        let fee_quote = fill.fee_quote.unwrap_or(Decimal::ZERO);
        let pnl = quote_proceeds - quote_cost - fee_quote;

        Self {
            venue: position.venue.clone(),
            symbol: position.symbol.clone(),
            quantity: fill.quantity,
            entry_price: position.entry_price,
            exit_price: execution_price,
            quote_cost,
            quote_proceeds,
            fee_quote,
            pnl,
            closed_at,
            execution_price_is_reference: fill.execution_price.is_none(),
            tx_id: fill.tx_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PerformanceSummary {
    pub trade_count: u64,
    pub winning_trades: u64,
    pub losing_trades: u64,
    pub flat_trades: u64,
    pub total_quote_cost: Decimal,
    pub total_quote_proceeds: Decimal,
    pub total_fees: Decimal,
    pub realized_pnl: Decimal,
}

impl PerformanceSummary {
    pub fn from_trades(trades: &[TradeRecord]) -> Self {
        let mut summary = Self::default();
        for trade in trades {
            summary.trade_count = summary.trade_count.saturating_add(1);
            summary.total_quote_cost += trade.quote_cost;
            summary.total_quote_proceeds += trade.quote_proceeds;
            summary.total_fees += trade.fee_quote;
            summary.realized_pnl += trade.pnl;

            if trade.pnl > Decimal::ZERO {
                summary.winning_trades = summary.winning_trades.saturating_add(1);
            } else if trade.pnl < Decimal::ZERO {
                summary.losing_trades = summary.losing_trades.saturating_add(1);
            } else {
                summary.flat_trades = summary.flat_trades.saturating_add(1);
            }
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProfitTarget, VenueKind};

    fn position() -> crate::Position {
        let venue = Venue::new(VenueKind::Dex, "raydium").expect("literal venue is valid");
        let symbol = Symbol::new("TOKEN").expect("literal symbol is valid");
        crate::Position::new(
            venue,
            symbol,
            Decimal::from(10),
            Decimal::from(4),
            ProfitTarget::from_percent(Decimal::TEN).expect("valid target"),
        )
    }

    #[test]
    fn records_reference_exit_and_pnl() {
        let trade = TradeRecord::from_position(&position(), Decimal::from(11), OffsetDateTime::UNIX_EPOCH);
        assert_eq!(trade.quote_cost, Decimal::from(40));
        assert_eq!(trade.quote_proceeds, Decimal::from(44));
        assert_eq!(trade.pnl, Decimal::from(4));
    }

    #[test]
    fn from_fill_prefers_exact_settlement_metadata() {
        let position = position();
        let fill = FilledSell {
            quantity: Decimal::from(4),
            execution_price: Some(Decimal::from(11)),
            quote_proceeds: Some(Decimal::from(43)),
            fee_quote: Some(Decimal::from(1)),
            tx_id: Some("0xabc".to_string()),
        };
        let trade = TradeRecord::from_fill(
            &position,
            &fill,
            Decimal::from(999),
            OffsetDateTime::UNIX_EPOCH,
        );
        assert_eq!(trade.exit_price, Decimal::from(11));
        assert_eq!(trade.quote_proceeds, Decimal::from(43));
        assert_eq!(trade.fee_quote, Decimal::ONE);
        assert_eq!(trade.pnl, Decimal::from(2));
        assert!(!trade.execution_price_is_reference);
        assert_eq!(trade.tx_id.as_deref(), Some("0xabc"));
    }

    #[test]
    fn summarizes_wins_losses_and_flats() {
        let p = position();
        let trades = vec![
            TradeRecord::from_position(&p, Decimal::from(11), OffsetDateTime::UNIX_EPOCH),
            TradeRecord::from_position(&p, Decimal::from(9), OffsetDateTime::UNIX_EPOCH),
            TradeRecord::from_position(&p, Decimal::from(10), OffsetDateTime::UNIX_EPOCH),
        ];
        let summary = PerformanceSummary::from_trades(&trades);
        assert_eq!(summary.trade_count, 3);
        assert_eq!(summary.winning_trades, 1);
        assert_eq!(summary.losing_trades, 1);
        assert_eq!(summary.flat_trades, 1);
        assert_eq!(summary.realized_pnl, Decimal::ZERO);
    }
}
