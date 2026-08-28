use crate::PortError;
use async_trait::async_trait;
use ben_snipes_domain::Listing;

/// What a listing source hands back on a single poll.
///
/// Some venues let you ask "give me everything new since cursor X" - a
/// paginated REST endpoint with a `since` param, or a WebSocket stream you
/// can resume from a sequence number. That's the cheap case: the venue
/// does the diffing for you, so we return `Incremental` and never need to
/// touch the full symbol list.
///
/// Other venues only expose "here is the full list of tradable symbols
/// right now" with no way to ask for just the delta. For those we return
/// `Full`, and the application layer (`NewListingDetector`) is responsible
/// for diffing it against what a `ListingStateStore` remembers from last
/// time.
///
/// Modelling both cases in one enum, rather than picking one strategy for
/// every adapter, is what lets a fast venue stay fast while a slow venue
/// still works correctly.
#[derive(Debug, Clone)]
pub enum ListingSnapshot {
    Full(Vec<Listing>),
    Incremental {
        new: Vec<Listing>,
        /// Opaque cursor to pass back on the next poll. Adapters define
        /// their own cursor format (a timestamp, a sequence number, a
        /// page token) - the application layer just stores and forwards
        /// it verbatim.
        cursor: Option<String>,
    },
}

#[async_trait]
pub trait ListingSource: Send + Sync {
    /// A short, unique name for this source, used as the key under which
    /// its state (known symbols / cursor) is persisted. E.g. "mexc" or
    /// "raydium".
    fn source_id(&self) -> &str;

    /// Poll for listings. `cursor` is whatever this source returned last
    /// time (`None` on the very first poll, or if this source doesn't do
    /// cursors at all). Implementations that don't support incremental
    /// fetching should just ignore the cursor and always return `Full`.
    async fn poll(&self, cursor: Option<&str>) -> Result<ListingSnapshot, PortError>;
}
