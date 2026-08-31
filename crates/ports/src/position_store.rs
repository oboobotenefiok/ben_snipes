use crate::PortError;
use async_trait::async_trait;
use ben_snipes_domain::Position;

/// Persists the set of currently-open positions, so a restart can
/// recover what's still held and resume watching it for exit.
///
/// Without this, a crash after a real buy doesn't just lose bookkeeping
/// - it orphans the position entirely: the `AcquisitionLedger` still
/// shows the token as already-acquired (so it's never re-detected as
/// buyable), but nothing is left watching it for take-profit/stop-loss,
/// since that list only ever lived in the runner's memory. This is what
/// closes that gap.
#[async_trait]
pub trait PositionStore: Send + Sync {
    /// Loads whatever was open at last save. Returns an empty list if
    /// nothing was ever saved - a fresh deployment has nothing open yet,
    /// which is the expected first-run state, not an error.
    async fn load(&self) -> Result<Vec<Position>, PortError>;

    /// Persists the complete current set of open positions - this
    /// replaces whatever was saved before, it doesn't append. Called
    /// after every change to the open-position list (a new buy, a
    /// closed exit) so a crash between calls loses at most the single
    /// most recent change, not the whole list.
    async fn save(&self, positions: &[Position]) -> Result<(), PortError>;
}
