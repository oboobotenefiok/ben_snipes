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
    /// for the "+10%" strategy this bot is built around. This is the
    /// only exit condition - there is deliberately no stop-loss. A
    /// position is held until it hits this target, however long that
    /// takes; it is never sold at a loss.
    pub take_profit_percent: Decimal,

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
    /// used to disqualify a listing either way. "Active volume" here
    /// means the smallest threshold that's still meaningful - enough to
    /// indicate real trading beyond a single initial buy, not a high bar
    /// that delays detection until a token has already built up
    /// substantial volume. See `config/default.toml` for the reasoning
    /// behind the specific default chosen.
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
    /// snapshots, and where the acquisition ledger's file lives.
    pub state_dir: String,
}

/// Solana execution settings. Deliberately has no field for the wallet
/// key - see `ben_snipes-adapter-pumpfun::execution::load_wallet`,
/// which reads the `SOLANA_PRIVATE_KEY` environment variable directly,
/// never config.
#[derive(Debug, Clone, Deserialize)]
pub struct SolanaConfig {
    /// PumpPortal's data websocket URL. Defaults to their public free
    /// endpoint - no API key needed for `subscribeNewToken`.
    pub pumpportal_ws_url: String,

    /// A Solana JSON-RPC HTTP endpoint used for broadcasting signed
    /// transactions and checking balances/confirmations. Unlike
    /// `pumpportal_ws_url`, this should be your own provider (public
    /// endpoints are typically rate-limited too aggressively for
    /// trading use).
    pub rpc_url: String,

    /// Slippage tolerance, as a percent, passed through to PumpPortal's
    /// trade-local API on every buy/sell.
    pub slippage_percent: u32,

    /// Priority fee in SOL, passed through to PumpPortal's trade-local
    /// API on every buy/sell - helps transactions land faster under
    /// network congestion.
    pub priority_fee_sol: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EvmChainConfig {
    /// e.g. "ethereum", "base" - becomes this source's chain identity.
    pub chain_name: String,
    /// A websocket RPC endpoint that supports `eth_subscribe`, with your
    /// own provider API key included. There is no usable default here -
    /// this must be supplied per deployment.
    pub ws_rpc_url: String,
    /// The DEX factory contract address to watch on this chain.
    pub factory_address: String,
    /// keccak256 topic hash of the pair/pool-creation event for this
    /// factory. Compute it yourself against the factory's actual ABI -
    /// see `ben_snipes-adapter-evm-onchain`'s docs for why this is never
    /// hardcoded as a default.
    pub topic0: String,
    /// Lowercased addresses of well-known base/quote assets on this
    /// chain (WETH, USDC, USDT, ...), used to identify which side of a
    /// new pair is the actual new listing.
    pub base_assets: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub risk: RiskConfig,
    pub safety: SafetyConfig,
    pub storage: StorageConfig,
    pub solana: SolanaConfig,
    /// Zero or more EVM chains to watch - one `EvmFactoryLogSource` gets
    /// spawned per entry. Empty by default; add entries in
    /// `config/default.toml` (or via env) per chain you want to watch.
    #[serde(default)]
    pub evm_chains: Vec<EvmChainConfig>,
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
