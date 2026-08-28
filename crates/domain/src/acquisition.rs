use crate::DomainError;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// The 24h volume and market cap of a symbol at the moment we're
/// deciding whether to buy it. This is deliberately a separate type from
/// `Listing` rather than fields bolted onto it - detecting that a symbol
/// exists is a different concern from assessing whether it's worth
/// buying, and a venue adapter can support one without the other.
///
/// `market_cap` is carried here for logging/sizing context even though
/// `AcquisitionCriteria` doesn't gate on it - see that type's docs for
/// why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListingMetrics {
    pub volume_24h: Decimal,
    pub market_cap: Decimal,
}

/// The filter that decides which newly-detected listings are worth
/// acquiring: active volume, full stop. A high market cap with real,
/// active trading volume is just as tradeable as a low one - what
/// actually matters for "can I get back out at +10%" is whether there's
/// a live market, not how big the token is. So this deliberately does
/// **not** gate on market cap at all; a listing qualifies purely on
/// whether its 24h volume clears the bar, whatever its market cap is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquisitionCriteria {
    min_volume_24h: Decimal,
}

impl AcquisitionCriteria {
    pub fn new(min_volume_24h: Decimal) -> Result<Self, DomainError> {
        if min_volume_24h < Decimal::ZERO {
            return Err(DomainError::InvalidMinVolume(min_volume_24h.to_string()));
        }
        Ok(Self { min_volume_24h })
    }

    pub fn matches(&self, metrics: &ListingMetrics) -> bool {
        metrics.volume_24h >= self.min_volume_24h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn criteria() -> AcquisitionCriteria {
        AcquisitionCriteria::new(Decimal::from(50_000)).expect("literal value here is valid")
    }

    #[test]
    fn rejects_negative_min_volume() {
        assert!(AcquisitionCriteria::new(Decimal::from(-1)).is_err());
    }

    #[test]
    fn accepts_zero_min_volume() {
        // Zero is a legitimate (if permissive) threshold - "any volume
        // at all counts" - so it should not be rejected the way a
        // negative value is.
        assert!(AcquisitionCriteria::new(Decimal::ZERO).is_ok());
    }

    #[test]
    fn matches_low_market_cap_listing_with_sufficient_volume() {
        let metrics = ListingMetrics {
            volume_24h: Decimal::from(200_000),
            market_cap: Decimal::from(500_000),
        };
        assert!(criteria().matches(&metrics));
    }

    #[test]
    fn matches_high_market_cap_listing_with_sufficient_volume() {
        // The whole point of this filter: a big market cap should not
        // disqualify a listing on its own, as long as volume is real.
        let metrics = ListingMetrics {
            volume_24h: Decimal::from(200_000),
            market_cap: Decimal::from(50_000_000),
        };
        assert!(criteria().matches(&metrics));
    }

    #[test]
    fn rejects_listing_with_insufficient_volume_regardless_of_market_cap() {
        let low_cap = ListingMetrics {
            volume_24h: Decimal::from(1_000),
            market_cap: Decimal::from(500_000),
        };
        let high_cap = ListingMetrics {
            volume_24h: Decimal::from(1_000),
            market_cap: Decimal::from(50_000_000),
        };
        assert!(!criteria().matches(&low_cap));
        assert!(!criteria().matches(&high_cap));
    }
}
