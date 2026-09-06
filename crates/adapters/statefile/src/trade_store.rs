//! File-backed completed-trade journal. The whole journal is rewritten
//! through a temporary file and atomic rename so a crash cannot leave a
//! partially-written JSON document.

use async_trait::async_trait;
use ben_snipes_domain::TradeRecord;
use ben_snipes_ports::{PendingTradeStore, PortError, TradeStore};
use std::path::PathBuf;
use tokio::fs;

pub struct FileTradeStore {
    path: PathBuf,
}

impl FileTradeStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl TradeStore for FileTradeStore {
    async fn load(&self) -> Result<Vec<TradeRecord>, PortError> {
        let raw = match fs::read_to_string(&self.path).await {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(PortError::Storage(Box::new(e))),
        };

        serde_json::from_str(&raw).map_err(|e| PortError::MalformedResponse {
            venue: "trade-store".to_string(),
            reason: e.to_string(),
        })
    }

    async fn append(&self, trade: &TradeRecord) -> Result<(), PortError> {
        let mut trades = self.load().await?;
        if trades.iter().any(|existing| existing.key() == trade.key()) {
            return Ok(());
        }
        trades.push(trade.clone());

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| PortError::Storage(Box::new(e)))?;
        }

        let tmp_path = self.path.with_extension("json.tmp");
        let serialised = serde_json::to_vec_pretty(&trades)
            .map_err(|e| PortError::Storage(Box::new(e)))?;

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
    use ben_snipes_domain::{Position, ProfitTarget, Symbol, Venue, VenueKind};
    use rust_decimal::Decimal;
    use time::OffsetDateTime;

    fn temp_path() -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should never be before the epoch in CI")
            .as_nanos();
        std::env::temp_dir().join(format!("ben_snipes-trades-test-{nanos}.json"))
    }

    fn sample_trade() -> TradeRecord {
        let venue = Venue::new(VenueKind::Dex, "pumpfun").expect("literal venue is valid");
        let symbol = Symbol::new("TOKEN").expect("literal symbol is valid");
        let position = Position::new(
            venue,
            symbol,
            Decimal::ONE,
            Decimal::TEN,
            ProfitTarget::from_percent(Decimal::TEN).expect("valid target"),
        );
        TradeRecord::from_position(&position, Decimal::from(2), OffsetDateTime::UNIX_EPOCH)
    }

    #[tokio::test]
    async fn missing_file_loads_as_empty() {
        let store = FileTradeStore::new(temp_path());
        let trades = store.load().await.expect("missing file is not an error");
        assert!(trades.is_empty());
    }

    #[tokio::test]
    async fn appends_and_reloads_trade_history() {
        let path = temp_path();
        let store = FileTradeStore::new(&path);
        let trade = sample_trade();

        store.append(&trade).await.expect("append should succeed");
        let loaded = store.load().await.expect("load should succeed");
        assert_eq!(loaded, vec![trade]);

        let _ = std::fs::remove_file(path);
    }
}


pub struct FilePendingTradeStore {
    path: PathBuf,
}

impl FilePendingTradeStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl PendingTradeStore for FilePendingTradeStore {
    async fn load(&self) -> Result<Vec<TradeRecord>, PortError> {
        let raw = match fs::read_to_string(&self.path).await {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(PortError::Storage(Box::new(e))),
        };
        serde_json::from_str(&raw).map_err(|e| PortError::MalformedResponse {
            venue: "pending-trade-store".to_string(),
            reason: e.to_string(),
        })
    }

    async fn save(&self, trades: &[TradeRecord]) -> Result<(), PortError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| PortError::Storage(Box::new(e)))?;
        }
        let tmp_path = self.path.with_extension("json.tmp");
        let serialised = serde_json::to_vec_pretty(trades)
            .map_err(|e| PortError::Storage(Box::new(e)))?;
        fs::write(&tmp_path, serialised)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;
        fs::rename(&tmp_path, &self.path)
            .await
            .map_err(|e| PortError::Storage(Box::new(e)))?;
        Ok(())
    }
}
