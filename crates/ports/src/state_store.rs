use crate::PortError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// What we remember about a single listing source between polls: the set
/// of dedupe keys (see `Listing::dedupe_key`) we've already seen, plus
/// whatever cursor that source gave us last time, if any.
///
/// This is what gets written to the statefile (or database, or key-value
/// store - whatever `ListingStateStore` adapter is wired in).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KnownListings {
    pub seen_keys: HashSet<String>,
    pub cursor: Option<String>,
    /// Whether we've ever recorded a baseline for this source. `false`
    /// means the very next full snapshot should be treated as "this is
    /// everything that already existed" rather than "this is all new" -
    /// without that distinction, the first poll of any full-snapshot
    /// source would flag its entire existing symbol universe as newly
    /// listed.
    #[serde(default)]
    pub bootstrapped: bool,
}

#[async_trait]
pub trait ListingStateStore: Send + Sync {
    /// Load what we last knew about a given source. Returns a default
    /// (empty) `KnownListings` if this source has never been polled
    /// before - that's not an error, it's the expected state on first run.
    async fn load(&self, source_id: &str) -> Result<KnownListings, PortError>;

    /// Persist the updated state for a source after a poll.
    async fn save(&self, source_id: &str, state: &KnownListings) -> Result<(), PortError>;
}
