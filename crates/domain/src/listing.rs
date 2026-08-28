use crate::{DomainError, Venue};
use serde::{Deserialize, Serialize};
use std::fmt;
use time::OffsetDateTime;

/// A ticker, pair, or token identifier as the venue names it. We keep this
/// as an opaque string rather than parsing it into base/quote assets here,
/// because that parsing is venue-specific (a CEX gives you "BTCUSDT", a
/// Solana DEX gives you a base58 mint address) and belongs in the adapter
/// that produced it, not in the domain.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Symbol(String);

impl Symbol {
    pub fn new(raw: impl Into<String>) -> Result<Self, DomainError> {
        let raw = raw.into();
        if raw.trim().is_empty() {
            return Err(DomainError::EmptySymbol);
        }
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A symbol observed as tradable on a venue, at the time we first saw it.
/// This is the unit that flows out of listing detection and into the
/// "should we buy this" decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub symbol: Symbol,
    pub venue: Venue,
    pub first_seen: OffsetDateTime,
}

impl Listing {
    pub fn new(symbol: Symbol, venue: Venue, first_seen: OffsetDateTime) -> Self {
        Self {
            symbol,
            venue,
            first_seen,
        }
    }

    /// A stable key for diffing snapshots against a state store. Two
    /// listings with the same key are "the same listing" even if other
    /// metadata about them differs between polls.
    pub fn dedupe_key(&self) -> String {
        format!("{}::{}", self.venue, self.symbol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_symbol() {
        assert_eq!(Symbol::new(""), Err(DomainError::EmptySymbol));
    }

    #[test]
    fn dedupe_key_combines_venue_and_symbol() {
        let venue = Venue::new(crate::VenueKind::Cex, "mexc").expect("literal name is valid");
        let symbol = Symbol::new("PEPEUSDT").expect("literal symbol is valid");
        let listing = Listing::new(symbol, venue, OffsetDateTime::UNIX_EPOCH);
        assert_eq!(listing.dedupe_key(), "cex:mexc::PEPEUSDT");
    }
}
