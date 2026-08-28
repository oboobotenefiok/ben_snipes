//! A file-backed `AcquisitionLedger`: a single JSON file holding the set
//! of canonical token IDs we've already acted on, guarded by an
//! in-process async mutex so `try_reserve` is atomic within this
//! process.
//!
//! That last qualifier matters: this only guarantees "no double-reserve
//! within one running instance" - it does **not** coordinate across
//! multiple bot processes sharing the same ledger file. Running more
//! than one instance of ben_snipes against the same state directory
//! needs a real concurrent store (e.g. a database with a unique
//! constraint) instead of this one. See the README.

use async_trait::async_trait;
use ben_snipes_ports::{AcquisitionLedger, PortError};
use std::collections::HashSet;
use std::path::PathBuf;
use tokio::fs;
use tokio::sync::Mutex;

pub struct FileAcquisitionLedger {
    path: PathBuf,
    reserved: Mutex<HashSet<String>>,
}

impl FileAcquisitionLedger {
    /// Loads the ledger from `path` if it exists, or starts empty if it
    /// doesn't (a fresh deployment has nothing reserved yet - that's the
    /// expected first-run state, not an error). Unlike `StatefileStore`,
    /// this does its I/O at construction time rather than lazily,
    /// because the ledger's whole contract depends on having the
    /// complete set loaded before the first `try_reserve` call.
    pub async fn load(path: impl Into<PathBuf>) -> Result<Self, PortError> {
        let path = path.into();

        let reserved = match fs::read_to_string(&path).await {
            Ok(raw) => serde_json::from_str(&raw).map_err(|e| PortError::MalformedResponse {
                venue: "acquisition-ledger".to_string(),
                reason: e.to_string(),
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
            Err(e) => return Err(PortError::Storage(Box::new(e))),
        };

        Ok(Self {
            path,
            reserved: Mutex::new(reserved),
        })
    }

    async fn persist(&self, snapshot: &HashSet<String>) -> Result<(), PortError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| PortError::Storage(Box::new(e)))?;
        }

        let tmp_path = self.path.with_extension("json.tmp");
        let serialised = serde_json::to_vec_pretty(snapshot).map_err(|e| PortError::Storage(Box::new(e)))?;

        // Same atomic temp-file-then-rename pattern as StatefileStore -
        // a crash mid-write can never corrupt the ledger, worst case we
        // lose the very last reservation and re-derive it on retry.
        fs::write(&tmp_path, serialised)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;
        fs::rename(&tmp_path, &self.path)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;

        Ok(())
    }
}

#[async_trait]
impl AcquisitionLedger for FileAcquisitionLedger {
    async fn try_reserve(&self, canonical_id: &str) -> Result<bool, PortError> {
        let mut guard = self.reserved.lock().await;
        if guard.contains(canonical_id) {
            return Ok(false);
        }
        guard.insert(canonical_id.to_string());
        self.persist(&guard).await?;
        Ok(true)
    }

    async fn release(&self, canonical_id: &str) -> Result<(), PortError> {
        let mut guard = self.reserved.lock().await;
        if guard.remove(canonical_id) {
            self.persist(&guard).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_ledger_path() -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should never be before the epoch in CI")
            .as_nanos();
        std::env::temp_dir().join(format!("ben_snipes-ledger-test-{nanos}.json"))
    }

    #[tokio::test]
    async fn first_reservation_succeeds_second_is_rejected() {
        let path = temp_ledger_path();
        let ledger = FileAcquisitionLedger::load(&path)
            .await
            .expect("fresh path should load as empty");

        let first = ledger.try_reserve("solana:abc").await.expect("reserve should not fail");
        let second = ledger.try_reserve("solana:abc").await.expect("reserve should not fail");

        assert!(first);
        assert!(!second);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn released_reservation_can_be_reclaimed() {
        let path = temp_ledger_path();
        let ledger = FileAcquisitionLedger::load(&path)
            .await
            .expect("fresh path should load as empty");

        assert!(ledger.try_reserve("solana:abc").await.expect("reserve should not fail"));
        ledger.release("solana:abc").await.expect("release should not fail");
        assert!(ledger.try_reserve("solana:abc").await.expect("reserve should not fail"));

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn reservations_persist_across_a_reload() {
        let path = temp_ledger_path();
        {
            let ledger = FileAcquisitionLedger::load(&path)
                .await
                .expect("fresh path should load as empty");
            ledger.try_reserve("solana:abc").await.expect("reserve should not fail");
        }

        let reloaded = FileAcquisitionLedger::load(&path)
            .await
            .expect("existing file should load");
        let can_reserve_again = reloaded
            .try_reserve("solana:abc")
            .await
            .expect("reserve should not fail");

        assert!(!can_reserve_again, "reservation from before the reload should still hold");

        let _ = std::fs::remove_file(&path);
    }
}
