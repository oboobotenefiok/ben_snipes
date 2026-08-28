use crate::{Chain, DomainError, Venue};
use serde::{Deserialize, Serialize};
use std::fmt;
use time::OffsetDateTime;

/// A ticker, pair, or token identifier as the venue names it. We keep this
/// as an opaque string rather than parsing it into base/quote assets here,
/// because that parsing is venue-specific (a CEX gives you "BTCUSDT", a
/// Solana DEX gives you a base58 mint address) and belongs in the adapter
/// that produced it, not in the domain.
///
/// For DEX listings, adapters should set this to the token's actual
/// contract/mint address (lowercased), not a display ticker - the
/// address is what `CanonicalTokenId` keys on, and tickers can collide
/// or be spoofed in a way an address can't.
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
    pub chain: Chain,
    pub first_seen: OffsetDateTime,
}

impl Listing {
    pub fn new(symbol: Symbol, venue: Venue, chain: Chain, first_seen: OffsetDateTime) -> Self {
        Self {
            symbol,
            venue,
            chain,
            first_seen,
        }
    }

    /// A stable key for diffing snapshots against a single source's
    /// state store. Two listings with the same key are "the same
    /// listing" *as far as that one source is concerned* - this is
    /// intentionally per-venue, not per-chain, so it stays correct even
    /// before `CanonicalTokenId` gets involved. See
    /// `crate::CanonicalTokenId` for the cross-source identity used to
    /// avoid buying the same token twice via two different sources.
    pub fn dedupe_key(&self) -> String {
        format!("{}::{}", self.venue, self.symbol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VenueKind;

    #[test]
    fn rejects_empty_symbol() {
        assert_eq!(Symbol::new(""), Err(DomainError::EmptySymbol));
    }

    #[test]
    fn dedupe_key_combines_venue_and_symbol() {
        let venue = Venue::new(VenueKind::Dex, "pumpfun").expect("literal name is valid");
        let chain = Chain::new("solana").expect("literal chain is valid");
        let symbol = Symbol::new("someMintAddress111").expect("literal symbol is valid");
        let listing = Listing::new(symbol, venue, chain, OffsetDateTime::UNIX_EPOCH);
        assert_eq!(listing.dedupe_key(), "dex:pumpfun::someMintAddress111");
    }
}
