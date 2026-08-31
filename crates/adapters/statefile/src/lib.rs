//! A `ListingStateStore` backed by plain JSON files on disk. One file per
//! source (e.g. `state/mexc.json`, `state/raydium.json`), so unrelated
//! sources never contend for the same file.
//!
//! This is the simplest adapter that could possibly work, which makes it
//! a good default and a good reference for writing a fancier one later
//! (sqlite, redis, whatever scaling calls for). Swapping it out means
//! writing a new struct that implements `ListingStateStore` - nothing
//! upstream of the port needs to change.

use async_trait::async_trait;
use ben_snipes_ports::{KnownListings, ListingStateStore, PortError};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::debug;

mod ledger;
mod position_store;
pub use ledger::FileAcquisitionLedger;
pub use position_store::FilePositionStore;

pub struct StatefileStore {
    directory: PathBuf,
}

impl StatefileStore {
    /// `directory` is created if it doesn't already exist the first time
    /// `save` is called - we don't touch the filesystem in the
    /// constructor, since construction should never fail on its own.
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    fn path_for(&self, source_id: &str) -> PathBuf {
        // Source IDs are adapter-controlled short names (like "mexc"), not
        // user input, but we still guard against anything that would
        // escape the state directory if that ever changes.
        let sanitised: String = source_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        self.directory.join(format!("{sanitised}.json"))
    }

    async fn ensure_directory_exists(&self) -> Result<(), PortError> {
        fs::create_dir_all(&self.directory)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))
    }
}

#[async_trait]
impl ListingStateStore for StatefileStore {
    async fn load(&self, source_id: &str) -> Result<KnownListings, PortError> {
        let path = self.path_for(source_id);

        if !path_exists(&path).await {
            debug!(source_id, "no existing state file, starting fresh");
            return Ok(KnownListings::default());
        }

        let raw = fs::read_to_string(&path)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;

        serde_json::from_str(&raw).map_err(|e| PortError::MalformedResponse {
            venue: source_id.to_string(),
            reason: e.to_string(),
        })
    }

    async fn save(&self, source_id: &str, state: &KnownListings) -> Result<(), PortError> {
        self.ensure_directory_exists().await?;

        let path = self.path_for(source_id);
        let tmp_path = path.with_extension("json.tmp");

        let serialised = serde_json::to_vec_pretty(state).map_err(|e| PortError::Storage(Box::new(e)))?;

        // Write to a temp file and rename over the real one. Rename is
        // atomic on the same filesystem, so a crash mid-write can never
        // leave us with a half-written, unparseable state file - worst
        // case we lose the update and fall back to what was there before.
        fs::write(&tmp_path, serialised)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;
        fs::rename(&tmp_path, &path)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;

        Ok(())
    }
}

async fn path_exists(path: &Path) -> bool {
    fs::metadata(path).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[tokio::test]
    async fn round_trips_state_through_disk() {
        let dir = std::env::temp_dir().join(format!("ben_snipes-test-{}", uuid_like()));
        let store = StatefileStore::new(&dir);

        let mut seen = HashSet::new();
        seen.insert("cex:mexc::AAAUSDT".to_string());
        let state = KnownListings {
            seen_keys: seen,
            cursor: Some("cursor-123".to_string()),
            bootstrapped: true,
        };

        store.save("mexc", &state).await.expect("save to temp dir should not fail");
        let loaded = store.load("mexc").await.expect("load from temp dir should not fail");

        assert_eq!(loaded.cursor, Some("cursor-123".to_string()));
        assert!(loaded.seen_keys.contains("cex:mexc::AAAUSDT"));

        // Clean up after ourselves; not load-bearing for the test result.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_file_returns_default_state() {
        let dir = std::env::temp_dir().join(format!("ben_snipes-test-{}", uuid_like()));
        let store = StatefileStore::new(&dir);

        let loaded = store
            .load("never-seen-before")
            .await
            .expect("missing file is not an error, it's a fresh start");

        assert!(loaded.seen_keys.is_empty());
        assert!(loaded.cursor.is_none());
    }

    /// A tiny, dependency-free stand-in for a UUID so tests don't collide
    /// on temp directory names. Not for use outside tests.
    fn uuid_like() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should never be before the epoch in CI")
            .as_nanos()
    }
}
