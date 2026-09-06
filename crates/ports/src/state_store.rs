use crate::PortError;
use async_trait::async_trait;
use ben_snipes_domain::Listing;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use time::OffsetDateTime;

/// What we remember about a single listing source between polls: the set
/// of dedupe keys (see `Listing::dedupe_key`) we've already seen, plus
/// whatever cursor that source gave us last time, if any.
///
/// This is what gets written to the statefile (or database, or key-value
/// store - whatever `ListingStateStore` adapter is wired in).
///
/// A listing that remains eligible for periodic re-evaluation while
/// external metrics or safety data are incomplete or below the buy threshold.
/// `pending_since` is the start of the fixed 24-hour retry window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PendingListing {
    pub listing: Listing,
    pub pending_since: OffsetDateTime,
}

impl PendingListing {
    pub fn new(listing: Listing, pending_since: OffsetDateTime) -> Self {
        Self {
            listing,
            pending_since,
        }
    }
}

impl<'de> Deserialize<'de> for PendingListing {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Current {
                listing: Listing,
                pending_since: OffsetDateTime,
            },
            Legacy(Listing),
        }

        match Wire::deserialize(deserializer)? {
            Wire::Current {
                listing,
                pending_since,
            } => Ok(Self {
                listing,
                pending_since,
            }),
            Wire::Legacy(listing) => Ok(Self {
                pending_since: listing.first_seen,
                listing,
            }),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KnownListings {
    pub seen_keys: HashSet<String>,
    pub cursor: Option<String>,
    /// Listings that were detected but did not yet have enough external
    /// information to make a safe acquisition decision. These are kept
    /// separately from `seen_keys` so a temporary indexer lag cannot make
    /// a real opportunity disappear forever.
    #[serde(default)]
    pub pending: HashMap<String, PendingListing>,
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
