//! Live price lookups via Jupiter's Price API v3
//! (`lite-api.jup.ag/price/v3`), used by `PumpPortalExchangeClient::current_price`.
//!
//! Verified against Jupiter's own developer docs at the time of
//! writing, including a literal example response - this is the
//! best-confirmed of the three new external integrations added this
//! round. One thing worth flagging anyway: **the older
//! `quote-api.jup.ag/v6` endpoints (which several third-party guides,
//! including one shared during this project's development, still
//! reference) were retired on 2025-10-01.** If a future docs check
//! shows `price/v3` has similarly moved on, that's the first thing to
//! re-verify here.
//!
//! Uses the keyless `lite-api.jup.ag` host (rate-limited but free) by
//! default. For production volume, `api.jup.ag` with an `x-api-key`
//! header is the documented higher-throughput option - not wired in
//! here, since a single default is enough to get this working and a
//! key can be layered on without changing the response-parsing logic.
//!
//! **Unit note, easy to get wrong:** Jupiter's Price API returns
//! USD-denominated prices. `Position::entry_price` throughout this
//! codebase is SOL-denominated (`submit_buy_by_amount` computes it as
//! `quote_amount spent in SOL / quantity received`), because that's
//! what `AcquisitionEngine.position_size` and PumpPortal's own trade
//! API are denominated in. Returning a raw USD price here would silently
//! compare against a SOL-denominated take-profit/stop-loss target -
//! wrong by roughly the SOL/USD exchange rate, not a rounding error.
//! `fetch_price` converts to SOL terms internally (by also fetching
//! SOL's own USD price in the same batched call) specifically so every
//! caller gets a value in the same unit `Position` already uses.

use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::HashMap;

const PRICE_API_URL: &str = "https://lite-api.jup.ag/price/v3";

/// Wrapped SOL's mint address - this exact value appears directly in
/// Jupiter's own documented example response, so it's about as
/// verified as a constant can be.
const SOL_MINT: &str = "So11111111111111111111111111111111111111112";

#[derive(Debug, Deserialize)]
struct PriceEntry {
    #[serde(rename = "usdPrice")]
    usd_price: f64,
}

/// Fetches the current price of `mint`, denominated in SOL (see the
/// module doc comment for why, not USD). Returns an error if either
/// mint has no price data (an extremely fresh pump.fun token, for
/// instance, may not be indexed here yet even if it's tradable) -
/// callers should treat that as "not enough information", the same way
/// `MetricsProvider::metrics` returning `None` is handled elsewhere in
/// this codebase.
pub async fn fetch_price(http: &reqwest::Client, mint: &str) -> Result<Decimal, String> {
    // Batched into one call - Jupiter's ids param takes a comma-separated
    // list, so this is one request, not two.
    let url = format!("{PRICE_API_URL}?ids={mint},{SOL_MINT}");

    let response = http
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Jupiter price request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("Jupiter price API returned {status}: {text}"));
    }

    let body: HashMap<String, PriceEntry> = response
        .json()
        .await
        .map_err(|e| format!("failed to parse Jupiter price response: {e}"))?;

    let token_usd = body
        .get(mint)
        .map(|e| e.usd_price)
        .ok_or_else(|| format!("Jupiter has no price data for {mint} yet"))?;

    let sol_usd = body
        .get(SOL_MINT)
        .map(|e| e.usd_price)
        .ok_or_else(|| "Jupiter had no price data for wrapped SOL - cannot convert to SOL terms".to_string())?;

    if sol_usd <= 0.0 {
        return Err("Jupiter returned a non-positive SOL price, cannot convert".to_string());
    }

    let price_in_sol = token_usd / sol_usd;

    Decimal::try_from(price_in_sol).map_err(|e| format!("converted price was not a valid decimal: {e}"))
}
