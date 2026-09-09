//! Batched live price lookups via Jupiter Price API v3.
//!
//! Prices are returned in SOL terms because live Solana positions store their
//! entry price in SOL per token. The cache and circuit breaker live here so
//! every exit cycle shares the same Jupiter state.

use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::debug;

const PRICE_API_URL: &str = "https://lite-api.jup.ag/price/v3";
const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize)]
struct PriceEntry {
    #[serde(rename = "usdPrice")]
    usd_price: f64,
}

#[derive(Debug)]
enum PriceFetchError {
    NotIndexed(String),
    RateLimited(String),
    Other(String),
}

impl PriceFetchError {
    fn message(&self) -> &str {
        match self {
            Self::NotIndexed(message) | Self::RateLimited(message) | Self::Other(message) => message,
        }
    }
}

#[derive(Debug)]
pub struct JupiterCircuitBreaker {
    consecutive_failures: AtomicU32,
    last_failure: Mutex<Option<Instant>>,
    max_failures: u32,
    cooldown: Duration,
}

impl JupiterCircuitBreaker {
    pub fn new(max_failures: u32, cooldown: Duration) -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            last_failure: Mutex::new(None),
            max_failures: max_failures.max(1),
            cooldown,
        }
    }

    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    pub async fn record_failure(&self) {
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        let mut last_failure = self.last_failure.lock().await;
        *last_failure = Some(Instant::now());
    }

    pub async fn is_open(&self) -> bool {
        if self.consecutive_failures.load(Ordering::Relaxed) < self.max_failures {
            return false;
        }

        let last_failure = self.last_failure.lock().await;
        match *last_failure {
            Some(at) => at.elapsed() < self.cooldown,
            None => false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PriceCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub batches: u64,
    pub requested_prices: u64,
    pub latency_ms_total: u64,
    pub latency_samples: u64,
    pub average_latency_ms: u64,
}

#[derive(Debug)]
struct CacheCounters {
    hits: AtomicU64,
    misses: AtomicU64,
    batches: AtomicU64,
    requested_prices: AtomicU64,
    latency_ms_total: AtomicU64,
    latency_samples: AtomicU64,
}

impl Default for CacheCounters {
    fn default() -> Self {
        Self {
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            batches: AtomicU64::new(0),
            requested_prices: AtomicU64::new(0),
            latency_ms_total: AtomicU64::new(0),
            latency_samples: AtomicU64::new(0),
        }
    }
}

#[derive(Debug)]
pub struct CachedPrice {
    pub price: Decimal,
    pub cached_at: Instant,
}

#[derive(Debug)]
pub struct PriceCache {
    cache: Arc<Mutex<HashMap<String, CachedPrice>>>,
    ttl: Duration,
    max_retries: u32,
    circuit_breaker: Arc<JupiterCircuitBreaker>,
    counters: Arc<CacheCounters>,
}

impl PriceCache {
    pub fn new(ttl: Duration, max_retries: u32, circuit_breaker: Arc<JupiterCircuitBreaker>) -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
            ttl,
            max_retries: max_retries.max(1),
            circuit_breaker,
            counters: Arc::new(CacheCounters::default()),
        }
    }

    pub async fn get_or_fetch_batch(
        &self,
        http: &reqwest::Client,
        mints: &[String],
    ) -> Result<HashMap<String, Decimal>, String> {
        let unique: Vec<String> = mints
            .iter()
            .filter(|mint| !mint.trim().is_empty())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();

        if unique.is_empty() {
            return Ok(HashMap::new());
        }

        let now = Instant::now();
        let mut result = HashMap::new();
        let mut missing = Vec::new();
        {
            let cache = self.cache.lock().await;
            for mint in &unique {
                if let Some(entry) = cache.get(mint) {
                    if now.duration_since(entry.cached_at) <= self.ttl {
                        result.insert(mint.clone(), entry.price);
                        continue;
                    }
                }
                missing.push(mint.clone());
            }
        }

        let hits = unique.len().saturating_sub(missing.len());
        self.counters.hits.fetch_add(hits as u64, Ordering::Relaxed);
        self.counters.misses.fetch_add(missing.len() as u64, Ordering::Relaxed);

        if missing.is_empty() {
            return Ok(result);
        }

        if self.circuit_breaker.is_open().await {
            return Err("Jupiter circuit breaker open; skipping price request".to_string());
        }

        let mut backoff = INITIAL_BACKOFF;
        let mut last_error = String::from("Jupiter price request failed");
        let mut last_not_indexed = false;
        for attempt in 1..=self.max_retries {
            let started = Instant::now();
            match fetch_prices_batch_once(http, &missing).await {
                Ok(prices) => {
                    self.counters.latency_ms_total.fetch_add(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                    self.counters.latency_samples.fetch_add(1, Ordering::Relaxed);
                    self.circuit_breaker.record_success();
                    self.counters.batches.fetch_add(1, Ordering::Relaxed);
                    self.counters.requested_prices.fetch_add(missing.len() as u64, Ordering::Relaxed);
                    {
                        let mut cache = self.cache.lock().await;
                        let now = Instant::now();
                        for (mint, price) in &prices {
                            cache.insert(mint.clone(), CachedPrice { price: *price, cached_at: now });
                        }
                    }
                    for mint in &missing {
                        if let Some(price) = prices.get(mint) {
                            result.insert(mint.clone(), *price);
                        }
                    }
                    return Ok(result);
                }
                Err(error) => {
                    self.counters.latency_ms_total.fetch_add(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                    self.counters.latency_samples.fetch_add(1, Ordering::Relaxed);
                    last_error = error.message().to_string();
                    last_not_indexed = matches!(error, PriceFetchError::NotIndexed(_));
                    let retryable = true;
                    debug!(attempt, max_retries = self.max_retries, error = %last_error, retryable, "Jupiter batch request failed");
                    if !retryable || attempt == self.max_retries {
                        break;
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }

        if !last_not_indexed {
            self.circuit_breaker.record_failure().await;
        }
        Err(last_error)
    }

    pub fn stats(&self) -> PriceCacheStats {
        PriceCacheStats {
            hits: self.counters.hits.load(Ordering::Relaxed),
            misses: self.counters.misses.load(Ordering::Relaxed),
            batches: self.counters.batches.load(Ordering::Relaxed),
            requested_prices: self.counters.requested_prices.load(Ordering::Relaxed),
            latency_ms_total: self.counters.latency_ms_total.load(Ordering::Relaxed),
            latency_samples: self.counters.latency_samples.load(Ordering::Relaxed),
            average_latency_ms: {
                let samples = self.counters.latency_samples.load(Ordering::Relaxed);
                if samples == 0 { 0 } else { self.counters.latency_ms_total.load(Ordering::Relaxed) / samples }
            },
        }
    }
}

async fn fetch_prices_batch_once(
    http: &reqwest::Client,
    mints: &[String],
) -> Result<HashMap<String, Decimal>, PriceFetchError> {
    let mut ids = mints.to_vec();
    if !ids.iter().any(|mint| mint == SOL_MINT) {
        ids.push(SOL_MINT.to_string());
    }
    let url = format!("{PRICE_API_URL}?ids={}", ids.join(","));

    let response = http.get(&url).send().await.map_err(|e| PriceFetchError::Other(format!("Jupiter price request failed: {e}")))?;
    let status = response.status();
    if status.as_u16() == 429 {
        return Err(PriceFetchError::RateLimited(format!("Jupiter price API rate limited request: {status}")));
    }
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(PriceFetchError::Other(format!("Jupiter price API returned {status}: {text}")));
    }

    let body: HashMap<String, PriceEntry> = response
        .json()
        .await
        .map_err(|e| PriceFetchError::Other(format!("failed to parse Jupiter price response: {e}")))?;

    let sol_usd = body.get(SOL_MINT).map(|entry| entry.usd_price).ok_or_else(|| {
        PriceFetchError::Other("Jupiter returned no wrapped SOL price; cannot convert to SOL terms".to_string())
    })?;
    if sol_usd <= 0.0 {
        return Err(PriceFetchError::Other("Jupiter returned a non-positive SOL price".to_string()));
    }

    let mut prices = HashMap::new();
    let mut missing = Vec::new();
    for mint in mints {
        if mint == SOL_MINT {
            prices.insert(mint.clone(), Decimal::ONE);
            continue;
        }
        match body.get(mint) {
            Some(entry) if entry.usd_price > 0.0 => {
                let price = Decimal::try_from(entry.usd_price / sol_usd)
                    .map_err(|e| PriceFetchError::Other(format!("invalid Jupiter price for {mint}: {e}")))?;
                prices.insert(mint.clone(), price);
            }
            _ => missing.push(mint.clone()),
        }
    }

    if !missing.is_empty() {
        return Err(PriceFetchError::NotIndexed(format!("Jupiter has no price data yet for: {}", missing.join(", "))));
    }

    Ok(prices)
}

/// Fetch prices for multiple mints in one Jupiter request.
pub async fn fetch_prices_batch(
    http: &reqwest::Client,
    mints: &[String],
) -> Result<HashMap<String, Decimal>, String> {
    fetch_prices_batch_once(http, mints)
        .await
        .map_err(|error| error.message().to_string())
}

/// Fetch one price without adding another Jupiter request for SOL. This is
/// retained for compatibility with the single-price ExchangeClient port.
pub async fn fetch_price(http: &reqwest::Client, mint: &str) -> Result<Decimal, String> {
    let mints = vec![mint.to_string()];
    let prices = fetch_prices_batch(http, &mints).await?;
    prices.get(mint).copied().ok_or_else(|| format!("Jupiter returned no price for {mint}"))
}
