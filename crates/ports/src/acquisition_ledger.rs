use crate::PortError;
use async_trait::async_trait;

/// The cross-source "have we already acted on this token" store.
///
/// This is what actually prevents a double-buy when more than one
/// `ListingSource` reports the same underlying token (via
/// `CanonicalTokenId`) - each source's own dedupe state in
/// `ListingStateStore` only knows "is this new to me", not "has anyone
/// already bought this". `AcquisitionEngine` calls `try_reserve`
/// immediately before submitting a buy order.
#[async_trait]
pub trait AcquisitionLedger: Send + Sync {
    /// Attempts to claim `canonical_id` for acquisition. Returns `true`
    /// if this call successfully claimed it - the caller now "owns" it
    /// and should proceed with the buy. Returns `false` if it was
    /// already claimed (by this call or an earlier one) - the caller
    /// must not buy.
    async fn try_reserve(&self, canonical_id: &str) -> Result<bool, PortError>;

    /// Releases a reservation. Used when a reserved buy never actually
    /// happened (e.g. the order submission failed after the reservation
    /// succeeded) - without this, a single transient failure would
    /// permanently block ever buying that token, since the reservation
    /// would still show as claimed forever.
    async fn release(&self, canonical_id: &str) -> Result<(), PortError>;
}
