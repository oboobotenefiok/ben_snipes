use crate::PortError;
use async_trait::async_trait;
use ben_snipes_domain::{ListingMetrics, Symbol};

/// Supplies the volume/market-cap snapshot a listing needs before
/// `AcquisitionCriteria` can be applied to it. Kept separate from
/// `ExchangeClient` in the trait definition (even though in practice a
/// single adapter usually implements both, since exchanges bundle ticker
/// stats with price data) because not every venue that can price a
/// symbol can also tell you its market cap - a DEX adapter, for
/// instance, may need a different upstream (a token info API) for that.
#[async_trait]
pub trait MetricsProvider: Send + Sync {
    /// Returns `Ok(None)` if this provider has no metrics for the
    /// symbol yet (common right after a listing appears - volume/market
    /// cap data can lag the listing itself by a few seconds). Callers
    /// should treat `None` as "not enough information to buy", not as
    /// an error.
    async fn metrics(&self, symbol: &Symbol) -> Result<Option<ListingMetrics>, PortError>;
}
