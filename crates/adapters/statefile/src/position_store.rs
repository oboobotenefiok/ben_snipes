//! A file-backed `PositionStore`: one JSON file holding the complete
//! list of currently-open positions, same atomic temp-file+rename
//! pattern as `StatefileStore` and `FileAcquisitionLedger`.

use async_trait::async_trait;
use ben_snipes_domain::Position;
use ben_snipes_ports::{PortError, PositionStore};
use std::path::PathBuf;
use tokio::fs;

pub struct FilePositionStore {
    path: PathBuf,
}

impl FilePositionStore {
    /// Doesn't touch the filesystem at construction - same convention
    /// as `StatefileStore`. `load` handles a not-yet-existing file as
    /// the expected first-run state.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl PositionStore for FilePositionStore {
    async fn load(&self) -> Result<Vec<Position>, PortError> {
        let raw = match fs::read_to_string(&self.path).await {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(PortError::Storage(Box::new(e))),
        };

        serde_json::from_str(&raw).map_err(|e| PortError::MalformedResponse {
            venue: "position-store".to_string(),
            reason: e.to_string(),
        })
    }

    async fn save(&self, positions: &[Position]) -> Result<(), PortError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| PortError::Storage(Box::new(e)))?;
        }

        let tmp_path = self.path.with_extension("json.tmp");
        let serialised = serde_json::to_vec_pretty(positions).map_err(|e| PortError::Storage(Box::new(e)))?;

        fs::write(&tmp_path, serialised)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;
        fs::rename(&tmp_path, &self.path)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ben_snipes_domain::{ProfitTarget, Symbol, Venue, VenueKind};
    use rust_decimal::Decimal;

    fn temp_path() -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should never be before the epoch in CI")
            .as_nanos();
        std::env::temp_dir().join(format!("ben_snipes-positions-test-{nanos}.json"))
    }

    fn sample_position() -> Position {
        let venue = Venue::new(VenueKind::Dex, "pumpfun").expect("literal venue is valid");
        let symbol = Symbol::new("someMint").expect("literal symbol is valid");
        Position::new(
            venue,
            symbol,
            Decimal::ONE,
            Decimal::TEN,
            ProfitTarget::from_percent(Decimal::TEN).expect("valid target"),
        )
    }

    #[tokio::test]
    async fn missing_file_loads_as_empty() {
        let store = FilePositionStore::new(temp_path());
        let loaded = store.load().await.expect("missing file is not an error");
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn round_trips_open_positions() {
        let path = temp_path();
        let store = FilePositionStore::new(&path);

        let positions = vec![sample_position()];
        store.save(&positions).await.expect("save should not fail");

        let loaded = store.load().await.expect("load should not fail");
        assert_eq!(loaded, positions);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn save_replaces_rather_than_appends() {
        let path = temp_path();
        let store = FilePositionStore::new(&path);

        store.save(&[sample_position()]).await.expect("save should not fail");
        store.save(&[]).await.expect("save should not fail");

        let loaded = store.load().await.expect("load should not fail");
        assert!(loaded.is_empty(), "second save should have replaced, not appended to, the first");

        let _ = std::fs::remove_file(&path);
    }
}
