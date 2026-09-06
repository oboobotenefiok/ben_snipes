use crate::PortError;
use async_trait::async_trait;
use ben_snipes_domain::TradeRecord;

/// Persists completed trades so performance survives process restarts.
/// Implementations should treat records as append-only business history.
#[async_trait]
pub trait TradeStore: Send + Sync {
    async fn load(&self) -> Result<Vec<TradeRecord>, PortError>;
    async fn append(&self, trade: &TradeRecord) -> Result<(), PortError>;
}


/// Durable queue for trades whose execution succeeded but whose main journal
/// could not be persisted yet. This prevents a filesystem failure from
/// turning a confirmed sell into an unaccounted-for trade.
#[async_trait]
pub trait PendingTradeStore: Send + Sync {
    async fn load(&self) -> Result<Vec<TradeRecord>, PortError>;
    async fn save(&self, trades: &[TradeRecord]) -> Result<(), PortError>;
}
