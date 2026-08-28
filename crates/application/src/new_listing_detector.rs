use ben_snipes_domain::Listing;
use ben_snipes_ports::{ListingSnapshot, ListingSource, ListingStateStore, PortError};
use std::sync::Arc;
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
pub struct NewListingDetector {
    state_store: Arc<dyn ListingStateStore>,
}

impl NewListingDetector {
    pub fn new(state_store: Arc<dyn ListingStateStore>) -> Self {
        Self { state_store }
    }

    pub async fn poll(&self, source: &dyn ListingSource) -> Result<Vec<Listing>, PortError> {
        let source_id = source.source_id();
        let mut known = self.state_store.load(source_id).await?;

        let snapshot = source.poll(known.cursor.as_deref()).await?;

        let newly_seen = match snapshot {
            ListingSnapshot::Incremental { new, cursor } => {
                debug!(source_id, count = new.len(), "incremental poll");
                for listing in &new {
                    known.seen_keys.insert(listing.dedupe_key());
                }
                known.cursor = cursor;
                new
            }
            ListingSnapshot::Full(all) if !known.bootstrapped => {
                // First time we've ever seen this source's full symbol
                // list. Everything in it already existed before we
                // started watching - record it as the baseline and
                // report nothing as new. Buying "new listings" that were
                // actually already trading before the bot even started
                // is exactly the failure mode this branch exists to
                // prevent.
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
                    known.seen_keys.insert(listing.dedupe_key());
                }
                fresh
            }
        };

        self.state_store.save(source_id, &known).await?;

        if !newly_seen.is_empty() {
            info!(source_id, count = newly_seen.len(), "new listings detected");
        }

        Ok(newly_seen)
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
        let result = detector.poll(&source).await.expect("in-memory store cannot fail");
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn second_poll_with_same_snapshot_returns_nothing_new() {
        let store = Arc::new(InMemoryStateStore::empty());
        let detector = NewListingDetector::new(store);
        let source = FixedFullSnapshotSource {
            listings: vec![listing("AAAUSDT")],
        };

        let first = detector.poll(&source).await.expect("in-memory store cannot fail");
        assert!(first.is_empty(), "first poll is the baseline, not new listings");

        let second = detector.poll(&source).await.expect("in-memory store cannot fail");
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
            .poll(&first_source)
            .await
            .expect("in-memory store cannot fail");
        assert!(first.is_empty(), "first poll is the baseline, not new listings");

        let second_source = FixedFullSnapshotSource {
            listings: vec![listing("AAAUSDT"), listing("CCCUSDT")],
        };
        let second = detector
            .poll(&second_source)
            .await
            .expect("in-memory store cannot fail");

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].symbol.as_str(), "CCCUSDT");
    }
}
