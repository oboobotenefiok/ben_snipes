use crate::Listing;
use serde::{Deserialize, Serialize};
use std::fmt;

/// The identity a token has regardless of which source detected it or
/// which specific DEX its pool lives on: its chain, plus its lowercased
/// contract/mint address.
///
/// This is deliberately coarser than `Listing::dedupe_key` (which is
/// per-venue, so it can track "is this new to source X"). Two different
/// sources - our own on-chain watcher and a third-party indexer, say,
/// both watching Solana - can each produce a `Listing` with a different
/// `Venue` for the exact same token. Those two listings have different
/// `dedupe_key()`s (correctly - each source needs its own "have I seen
/// this" memory) but the *same* `CanonicalTokenId`, because they're
/// describing the same underlying asset.
///
/// `AcquisitionEngine` reserves a `CanonicalTokenId` in the
/// `AcquisitionLedger` immediately before buying, so whichever source
/// gets there first wins the reservation and every other source's report
/// of the same token becomes a no-op - this is the actual mechanism that
/// stops the bot from buying a token twice because two sources both
/// noticed it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CanonicalTokenId(String);

impl CanonicalTokenId {
    pub fn from_listing(listing: &Listing) -> Self {
        Self(format!(
            "{}:{}",
            listing.chain.as_str(),
            listing.symbol.as_str().to_lowercase()
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CanonicalTokenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Chain, Symbol, Venue, VenueKind};
    use time::OffsetDateTime;

    fn listing_with(venue_name: &str, symbol: &str) -> Listing {
        let venue = Venue::new(VenueKind::Dex, venue_name).expect("literal venue is valid");
        let chain = Chain::new("solana").expect("literal chain is valid");
        let symbol = Symbol::new(symbol).expect("literal symbol is valid");
        Listing::new(symbol, venue, chain, OffsetDateTime::UNIX_EPOCH)
    }

    #[test]
    fn two_different_venues_reporting_the_same_token_share_a_canonical_id() {
        let from_source_a = listing_with("pumpfun", "SomeMintAddress111");
        let from_source_b = listing_with("birdeye-poller", "somemintaddress111");

        assert_eq!(
            CanonicalTokenId::from_listing(&from_source_a),
            CanonicalTokenId::from_listing(&from_source_b)
        );
    }

    #[test]
    fn different_tokens_on_the_same_venue_have_different_canonical_ids() {
        let a = listing_with("pumpfun", "MintAddressA");
        let b = listing_with("pumpfun", "MintAddressB");
        assert_ne!(CanonicalTokenId::from_listing(&a), CanonicalTokenId::from_listing(&b));
    }
}
