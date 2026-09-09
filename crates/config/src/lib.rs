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

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    /// Detection and real execution when wallets are configured.
    #[default]
    Live,
    /// Run the complete strategy with real market data but simulate orders.
    Paper,
    /// Detect and evaluate listings, but never attempt acquisition or exits.
    DetectionOnly,
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

    /// How often listings that were detected before an indexer exposed
    /// their metrics are retried. This is intentionally independent from
    /// source polling so fast feeds do not hammer slower indexers.
    pub pending_listing_retry_seconds: u64,

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

    /// Hard cap on concurrently open positions across all venues. Once
    /// reached, remaining listings in a poll cycle are skipped rather
    /// than queued, so capital exposure never exceeds
    /// `max_open_positions * max_position_size`.
    pub max_open_positions: usize,

    /// Consecutive acquisition/exit failures (I/O errors, not ordinary
    /// "pending"/"rejected" outcomes) before the entry circuit breaker
    /// opens and new buys pause for the rest of that poll cycle.
    pub max_consecutive_failures: u32,

    /// Maximum number of newly-detected listings processed for
    /// acquisition in a single poll cycle across all venues. Protects
    /// against a listing burst (e.g. a source replaying its backlog)
    /// from spending capital faster than the operator can react.
    pub max_new_listings_per_cycle: usize,

    /// Path to an operator-controlled kill-switch file. While it exists,
    /// no new positions are opened, but existing positions are still
    /// watched and exited normally.
    pub entry_kill_switch_file: String,
}


#[derive(Debug, Clone, Deserialize, Default)]
pub struct ObservabilityConfig {
    /// Local HTTP bind address for the Prometheus-compatible `/metrics`
    /// endpoint. Keep this on loopback unless access control is provided
    /// by the deployment environment.
    #[serde(default = "default_metrics_bind")]
    pub metrics_bind: String,
}

fn default_metrics_bind() -> String {
    "127.0.0.1:9090".to_string()
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
    /// Numeric EVM chain ID. Used to prevent signing against the wrong network.
    pub chain_id: u64,
    /// A websocket RPC endpoint that supports `eth_subscribe`, with your
    /// own provider API key included. There is no usable default here -
    /// this must be supplied per deployment.
    pub ws_rpc_url: String,
    /// HTTP RPC endpoint used for reads and execution when no private RPC is configured.
    pub execution_rpc_url: String,
    /// Optional private transaction RPC, such as Flashbots Protect.
    #[serde(default)]
    pub private_rpc_url: Option<String>,
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
    /// Uniswap-V2-compatible router used for native-coin buys and token sells.
    pub router_address: String,
    /// Wrapped native token used as the router path endpoint.
    pub wrapped_native_address: String,
    /// Maximum execution slippage as a percentage.
    #[serde(default = "default_evm_slippage_percent")]
    pub slippage_percent: u32,
}

fn default_evm_slippage_percent() -> u32 { 10 }

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub execution_mode: ExecutionMode,
    pub risk: RiskConfig,
    #[serde(default)]
    pub observability: ObservabilityConfig,
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
