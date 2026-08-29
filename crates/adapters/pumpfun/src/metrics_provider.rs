//! Real `MetricsProvider` for Solana tokens via DexScreener's
//! single-token lookup endpoint (`/latest/dex/tokens/<address>`) - free,
//! keyless, and well-corroborated across independent sources at the
//! time of writing.
//!
//! **This is a different endpoint from the one this project deliberately
//! avoided for listing detection.** The new-pairs/discovery endpoint
//! that capped out at ~30 results with no real pagination is a
//! different concern (a live firehose) from this one (a single lookup
//! by an address you already have) - there's no pagination problem to
//! begin with when you're asking about one specific token.

use async_trait::async_trait;
use ben_snipes_domain::{ListingMetrics, Symbol};
use ben_snipes_ports::{MetricsProvider, PortError};
use rust_decimal::Decimal;
use serde::Deserialize;

const TOKENS_URL: &str = "https://api.dexscreener.com/latest/dex/tokens";

#[derive(Debug, Deserialize)]
struct TokensResponse {
    #[serde(default)]
    pairs: Option<Vec<PairInfo>>,
}

#[derive(Debug, Deserialize)]
struct PairInfo {
    #[serde(default)]
    volume: Option<VolumeInfo>,
    #[serde(default, rename = "marketCap")]
    market_cap: Option<f64>,
    #[serde(default)]
    fdv: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct VolumeInfo {
    #[serde(default, rename = "h24")]
    h24: Option<f64>,
}

pub struct DexScreenerMetricsProvider {
    http: reqwest::Client,
}

impl DexScreenerMetricsProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for DexScreenerMetricsProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MetricsProvider for DexScreenerMetricsProvider {
    async fn metrics(&self, symbol: &Symbol) -> Result<Option<ListingMetrics>, PortError> {
        let url = format!("{TOKENS_URL}/{}", symbol.as_str());

        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| PortError::Network {
                venue: "dexscreener".to_string(),
                source: Box::new(e),
            })?;

        if !response.status().is_success() {
            // A 404-shaped "no pairs yet" is expected for a brand-new
            // token, not a hard failure - treat any non-success the
            // same way: not enough information yet.
            return Ok(None);
        }

        let body: TokensResponse = response.json().await.map_err(|e| PortError::MalformedResponse {
            venue: "dexscreener".to_string(),
            reason: e.to_string(),
        })?;

        let Some(pairs) = body.pairs else {
            return Ok(None);
        };

        // A token can have multiple pairs (different pools/DEXes) -
        // take the one with the most volume, since that's the most
        // representative of "is there a real market here".
        let Some(best) = pairs.iter().max_by(|a, b| {
            let a_vol = a.volume.as_ref().and_then(|v| v.h24).unwrap_or(0.0);
            let b_vol = b.volume.as_ref().and_then(|v| v.h24).unwrap_or(0.0);
            a_vol.partial_cmp(&b_vol).unwrap_or(std::cmp::Ordering::Equal)
        }) else {
            return Ok(None);
        };

        let volume_24h = best.volume.as_ref().and_then(|v| v.h24).unwrap_or(0.0);
        // Prefer marketCap; DexScreener sometimes only populates fdv
        // (fully-diluted valuation) for very new tokens before
        // circulating-supply data is available.
        let market_cap = best.market_cap.or(best.fdv).unwrap_or(0.0);

        let volume_24h = Decimal::try_from(volume_24h).map_err(|e| PortError::MalformedResponse {
            venue: "dexscreener".to_string(),
            reason: format!("volume was not a valid decimal: {e}"),
        })?;
        let market_cap = Decimal::try_from(market_cap).map_err(|e| PortError::MalformedResponse {
            venue: "dexscreener".to_string(),
            reason: format!("market cap was not a valid decimal: {e}"),
        })?;

        Ok(Some(ListingMetrics { volume_24h, market_cap }))
    }
}
