use crate::PortError;
use async_trait::async_trait;
use ben_snipes_domain::{SafetyReport, Symbol};

/// Supplies the honeypot/rug safety signals `SafetyCriteria` checks
/// before a DEX buy. Separate from `MetricsProvider` because the two
/// concerns come from genuinely different data sources in a real
/// deployment - volume/market-cap usually comes from a price API,
/// contract safety usually comes from simulating a sell or from a
/// contract-analysis service (e.g. a token scanner) - and a venue could
/// reasonably have one without the other.
#[async_trait]
pub trait TokenSafetyChecker: Send + Sync {
    /// Returns `Ok(None)` if a safety assessment isn't available yet.
    /// `AcquisitionEngine` treats `None` the same as a failed check -
    /// not enough information to buy - never as permission to skip the
    /// gate.
    async fn assess(&self, symbol: &Symbol) -> Result<Option<SafetyReport>, PortError>;
}
