//! Typed configuration for ben_snipes. Loads `config/default.toml` and
//! then lets environment variables prefixed `BEN_SNIPES_` override any
//! value, which is the layering you want for a bot: sane defaults in
//! version control, secrets and per-deployment tuning in the
//! environment, never the other way around.

use rust_decimal::Decimal;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to load configuration: {0}")]
    Load(#[from] config::ConfigError),
}

#[derive(Debug, Clone, Deserialize)]
pub struct RiskConfig {
    /// Take-profit target as a percentage above entry price, e.g. 10.0
    /// for the "+10%" strategy this bot is built around.
    pub take_profit_percent: Decimal,

    /// Stop-loss floor as a percentage below entry price, e.g. 5.0 to
    /// exit at -5%. Without this, a position that never reaches
    /// take-profit just sits open indefinitely.
    pub stop_loss_percent: Decimal,

    /// How often, in seconds, each listing source gets polled.
    pub poll_interval_seconds: u64,

    /// Maximum quote-currency amount to spend on a single new listing.
    /// This is the single most important number in the whole config for
    /// keeping a bad listing from being an expensive mistake.
    pub max_position_size: Decimal,

    /// A listing is only bought if its 24h volume is at or above this -
    /// the sole acquisition gate. Deliberately not paired with a market
    /// cap ceiling: a high-market-cap listing with genuinely active
    /// volume is just as tradeable as a low-cap one, so market cap isn't
    /// used to disqualify a listing either way.
    pub min_volume_24h: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SafetyConfig {
    /// Maximum acceptable sell tax, in basis points (100 = 1%), for a
    /// DEX listing to pass the honeypot/rug safety gate. Only applies to
    /// venues that have a `SafetyGate` configured - see the README.
    pub max_sell_tax_bps: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig {
    /// Directory where the statefile adapter keeps its per-source JSON
    /// snapshots.
    pub state_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub risk: RiskConfig,
    pub safety: SafetyConfig,
    pub storage: StorageConfig,
}

impl AppConfig {
    /// Loads `config/default.toml` relative to the current working
    /// directory, then applies any `BEN_SNIPES_*` environment variable
    /// overrides (e.g. `BEN_SNIPES_RISK__MAX_POSITION_SIZE=50`).
    pub fn load() -> Result<Self, ConfigError> {
        let raw = config::Config::builder()
            .add_source(config::File::with_name("config/default"))
            .add_source(config::Environment::with_prefix("BEN_SNIPES").separator("__"))
            .build()?;

        Ok(raw.try_deserialize()?)
    }
}
