use ben_snipes_domain::Listing;
use ben_snipes_ports::{
    KnownListings, ListingSnapshot, ListingSource, ListingStateStore, PendingListing, PortError,
};
use std::collections::HashSet;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use tracing::{debug, info};

/// Detects newly-appeared listings on a source, handling both strategies
/// a `ListingSource` can use:
///
/// - If the source supports incremental fetching (it returns
///   `ListingSnapshot::Incremental`), we trust it directly and just track
///   the cursor it hands back.
/// - If the source can only give us a full snapshot, we diff it against
///   the set of dedupe keys we saved last time and only surface what's
///   actually new.
///
/// Either way, callers get back exactly the same thing: a `Vec<Listing>`
/// of things they haven't seen before. Which strategy a given venue uses
/// is an adapter concern, invisible from here.
/// Pending candidates are retried continuously for one full day from the
/// moment they enter the pending queue. This deliberately outlives both
/// indexer lag and a temporary period of weak volume: a listing can become
/// tradeable later without being rediscovered by the source.
const PENDING_TTL: Duration = Duration::hours(24);

fn prune_expired_pending(known: &mut KnownListings, now: OffsetDateTime) {
    known.pending.retain(|key, pending| {
        let age = now - pending.pending_since;
        if age >= PENDING_TTL {
            debug!(
                listing = %key,
                age_seconds = age.whole_seconds(),
                "pending listing expired after 24-hour retry window"
            );
            false
        } else {
            true
        }
    });
}

pub struct NewListingDetector {
    state_store: Arc<dyn ListingStateStore>,
}

impl NewListingDetector {
    pub fn new(state_store: Arc<dyn ListingStateStore>) -> Self {
        Self { state_store }
    }

    pub async fn poll(
        &self,
        source: &dyn ListingSource,
        retry_pending: bool,
    ) -> Result<Vec<Listing>, PortError> {
        let source_id = source.source_id();
        let mut known = self.state_store.load(source_id).await?;

        let snapshot = source.poll(known.cursor.as_deref()).await?;

        let mut newly_seen = match snapshot {
            ListingSnapshot::Incremental { new, cursor } => {
                debug!(source_id, count = new.len(), "incremental poll");
                for listing in &new {
                    let key = listing.dedupe_key();
                    known.seen_keys.insert(key.clone());
                    known.pending.entry(key).or_insert_with(|| {
                        PendingListing::new(listing.clone(), OffsetDateTime::now_utc())
                    });
                }
                known.cursor = cursor;
                new
            }
            ListingSnapshot::Full(all) if !known.bootstrapped => {
                info!(
                    source_id,
                    count = all.len(),
                    "establishing baseline snapshot, nothing reported as new"
                );
                for listing in &all {
                    known.seen_keys.insert(listing.dedupe_key());
                }
                known.bootstrapped = true;
                Vec::new()
            }
            ListingSnapshot::Full(all) => {
                debug!(source_id, count = all.len(), "full snapshot poll, diffing");
                let fresh: Vec<Listing> = all
                    .into_iter()
                    .filter(|listing| !known.seen_keys.contains(&listing.dedupe_key()))
                    .collect();
                for listing in &fresh {
                    let key = listing.dedupe_key();
                    known.seen_keys.insert(key.clone());
                    known.pending.entry(key).or_insert_with(|| {
                        PendingListing::new(listing.clone(), OffsetDateTime::now_utc())
                    });
                }
                fresh
            }
        };

        let now = OffsetDateTime::now_utc();
        prune_expired_pending(&mut known, now);

        if retry_pending {
            let already_returned: HashSet<String> = newly_seen.iter().map(Listing::dedupe_key).collect();
            let pending: Vec<Listing> = known
                .pending
                .values()
                .filter(|pending| !already_returned.contains(&pending.listing.dedupe_key()))
                .map(|pending| pending.listing.clone())
                .collect();
            if !pending.is_empty() {
                debug!(source_id, count = pending.len(), "retrying pending listings");
                newly_seen.extend(pending);
            }
        }

        self.state_store.save(source_id, &known).await?;

        if !newly_seen.is_empty() {
            info!(source_id, count = newly_seen.len(), "new listings detected");
        }

        Ok(newly_seen)
    }

    /// Resolves a candidate after the acquisition engine has enough
    /// information to make a final decision. `keep_pending = true` is used
    /// for temporary information gaps such as DexScreener not having
    /// indexed a brand-new token yet.
    pub async fn resolve(
        &self,
        source: &dyn ListingSource,
        listing: &Listing,
        keep_pending: bool,
    ) -> Result<(), PortError> {
        if keep_pending {
            return Ok(());
        }

        let source_id = source.source_id();
        let mut known = self.state_store.load(source_id).await?;
        known.pending.remove(&listing.dedupe_key());
        self.state_store.save(source_id, &known).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ben_snipes_domain::{Chain, Symbol, Venue, VenueKind};
    use ben_snipes_ports::KnownListings;
    use std::sync::Mutex;
    use time::OffsetDateTime;

    /// An in-memory state store for tests, so we're not touching disk to
    /// verify diffing logic.
    struct InMemoryStateStore {
        state: Mutex<Option<KnownListings>>,
    }

    impl InMemoryStateStore {
        fn empty() -> Self {
            Self {
                state: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl ListingStateStore for InMemoryStateStore {
        async fn load(&self, _source_id: &str) -> Result<KnownListings, PortError> {
            let guard = self
                .state
                .lock()
                .expect("test-only mutex, poisoning here means a prior test panicked");
            Ok(guard.clone().unwrap_or_default())
        }

        async fn save(&self, _source_id: &str, new_state: &KnownListings) -> Result<(), PortError> {
            let mut guard = self
                .state
                .lock()
                .expect("test-only mutex, poisoning here means a prior test panicked");
            *guard = Some(new_state.clone());
            Ok(())
        }
    }

    struct FixedFullSnapshotSource {
        listings: Vec<Listing>,
    }

    #[async_trait]
    impl ListingSource for FixedFullSnapshotSource {
        fn source_id(&self) -> &str {
            "test-full"
        }

        async fn poll(&self, _cursor: Option<&str>) -> Result<ListingSnapshot, PortError> {
            Ok(ListingSnapshot::Full(self.listings.clone()))
        }
    }

    fn listing(symbol: &str) -> Listing {
        let venue = Venue::new(VenueKind::Dex, "raydium").expect("literal venue name is valid");
        let chain = Chain::new("solana").expect("literal chain is valid");
        let symbol = Symbol::new(symbol).expect("literal symbol is valid");
        Listing::new(symbol, venue, chain, OffsetDateTime::UNIX_EPOCH)
    }

    #[tokio::test]
    async fn first_poll_establishes_baseline_and_reports_nothing_new() {
        let store = Arc::new(InMemoryStateStore::empty());
        let detector = NewListingDetector::new(store);
        let source = FixedFullSnapshotSource {
            listings: vec![listing("AAAUSDT"), listing("BBBUSDT")],
        };

        // These symbols already existed before we started watching - the
        // very first poll must never report them as "new", or the bot
        // would try to buy every existing listing on startup.
        let result = detector.poll(&source, true).await.expect("in-memory store cannot fail");
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn second_poll_with_same_snapshot_returns_nothing_new() {
        let store = Arc::new(InMemoryStateStore::empty());
        let detector = NewListingDetector::new(store);
        let source = FixedFullSnapshotSource {
            listings: vec![listing("AAAUSDT")],
        };

        let first = detector.poll(&source, true).await.expect("in-memory store cannot fail");
        assert!(first.is_empty(), "first poll is the baseline, not new listings");

        let second = detector.poll(&source, true).await.expect("in-memory store cannot fail");
        assert!(second.is_empty());
    }

    #[tokio::test]
    async fn diff_only_surfaces_the_genuinely_new_symbol_after_baseline() {
        let store = Arc::new(InMemoryStateStore::empty());
        let detector = NewListingDetector::new(store);

        let first_source = FixedFullSnapshotSource {
            listings: vec![listing("AAAUSDT")],
        };
        let first = detector
            .poll(&first_source, true)
            .await
            .expect("in-memory store cannot fail");
        assert!(first.is_empty(), "first poll is the baseline, not new listings");

        let second_source = FixedFullSnapshotSource {
            listings: vec![listing("AAAUSDT"), listing("CCCUSDT")],
        };
        let second = detector
            .poll(&second_source, true)
            .await
            .expect("in-memory store cannot fail");

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].symbol.as_str(), "CCCUSDT");
    }
    #[tokio::test]
    async fn pending_listing_is_retried_without_becoming_a_new_listing_again() {
        let store = Arc::new(InMemoryStateStore::empty());
        let detector = NewListingDetector::new(store);
        let source = FixedFullSnapshotSource {
            listings: vec![listing("PENDING")],
        };

        let first = detector.poll(&source, true).await.expect("poll should succeed");
        assert_eq!(first.len(), 0, "the first full snapshot is the baseline");

        let second = detector.poll(&source, true).await.expect("poll should succeed");
        assert_eq!(second.len(), 0, "an unchanged full snapshot has no pending candidate after baseline");

        // Simulate a source that reports the candidate as genuinely new by
        // using a new source state. The detector should retain that listing
        // for later retries instead of losing it after the first metrics miss.
        let source = FixedFullSnapshotSource {
            listings: vec![listing("PENDING2")],
        };
        let fresh = detector.poll(&source, true).await.expect("poll should succeed");
        assert_eq!(fresh.len(), 1);

        let retry = detector.poll(&source, true).await.expect("poll should succeed");
        assert_eq!(retry.len(), 1);
        assert_eq!(retry[0].symbol.as_str(), "PENDING2");

        detector
            .resolve(&source, &retry[0], false)
            .await
            .expect("state update should succeed");
        let after_resolve = detector.poll(&source, true).await.expect("poll should succeed");
        assert!(after_resolve.is_empty());
    }

    #[test]
    fn pending_listing_expires_only_after_24_hours() {
        let listing = listing("EXPIRING");
        let pending_since = OffsetDateTime::UNIX_EPOCH;
        let mut known = KnownListings::default();
        known.pending.insert(
            listing.dedupe_key(),
            PendingListing::new(listing.clone(), pending_since),
        );

        prune_expired_pending(&mut known, pending_since + Duration::hours(23) + Duration::minutes(59));
        assert!(known.pending.contains_key(&listing.dedupe_key()));

        prune_expired_pending(&mut known, pending_since + Duration::hours(24));
        assert!(!known.pending.contains_key(&listing.dedupe_key()));
    }

    #[test]
    fn legacy_pending_state_uses_listing_first_seen_as_pending_start() {
        let listing = listing("LEGACY");
        let encoded = match serde_json::to_string(&listing) {
            Ok(value) => value,
            Err(error) => panic!("listing serialization failed: {error}"),
        };
        let decoded: PendingListing = match serde_json::from_str(&encoded) {
            Ok(value) => value,
            Err(error) => panic!("legacy pending state deserialization failed: {error}"),
        };

        assert_eq!(decoded.listing, listing);
        assert_eq!(decoded.pending_since, OffsetDateTime::UNIX_EPOCH);
    }

}
