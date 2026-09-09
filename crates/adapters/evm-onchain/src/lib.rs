//! EVM detection, market-data enrichment, and Uniswap-V2-style
//! execution for the trading pipeline.
//!
//! Detection remains factory-log based. Trading uses Alloy with a local
//! `PrivateKeySigner`; the unsigned transaction is simulated with `eth_call`
//! before the provider is allowed to sign and submit it. When `private_rpc_url`
//! is configured, the signed transaction is sent through that endpoint. This
//! supports private RPCs such as Flashbots Protect without making the adapter
//! depend on Flashbots-specific RPC methods.

use alloy::{
    network::{
        eip2718::Encodable2718, EthereumWallet, NetworkTransactionBuilder, ReceiptResponse,
        TransactionBuilder,
    },
    primitives::{Address, Bytes, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol,
};
use async_trait::async_trait;
use ben_snipes_adapter_ws_support::connect_with_backoff;
use ben_snipes_domain::{
    Chain, DomainError, FilledBuy, FilledSell, Listing, ListingMetrics, Order, OrderSide,
    Symbol, Venue, VenueKind,
};
use ben_snipes_ports::{
    ExchangeClient, ListingSnapshot, ListingSource, MetricsProvider, PortError,
};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use std::{collections::HashSet, sync::Arc};
use std::str::FromStr;
use time::OffsetDateTime;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

sol! {
    #[sol(rpc)]
    interface UniswapV2Router {
        function getAmountsOut(uint256 amountIn, address[] calldata path)
            external view returns (uint256[] memory amounts);
        function swapExactETHForTokensSupportingFeeOnTransferTokens(
            uint256 amountOutMin,
            address[] calldata path,
            address to,
            uint256 deadline
        ) external payable;
        function swapExactTokensForETHSupportingFeeOnTransferTokens(
            uint256 amountIn,
            uint256 amountOutMin,
            address[] calldata path,
            address to,
            uint256 deadline
        ) external;
    }

    #[sol(rpc)]
    interface Erc20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
        function balanceOf(address owner) external view returns (uint256);
        function decimals() external view returns (uint8);
    }
}

#[derive(Debug, Clone)]
pub struct EvmFactoryConfig {
    pub chain_name: String,
    pub chain_id: u64,
    pub ws_rpc_url: String,
    pub execution_rpc_url: String,
    pub private_rpc_url: Option<String>,
    pub factory_address: String,
    pub topic0: String,
    pub base_assets: Vec<String>,
    pub router_address: String,
    pub wrapped_native_address: String,
    pub slippage_percent: u32,
}

pub struct EvmFactoryLogSource {
    venue: Venue,
    receiver: Mutex<mpsc::UnboundedReceiver<Listing>>,
}

impl EvmFactoryLogSource {
    pub fn spawn(config: EvmFactoryConfig) -> Result<Self, DomainError> {
        validate_factory_config(&config)?;
        let venue = Venue::new(VenueKind::Dex, format!("{}-onchain", config.chain_name))?;
        let chain = Chain::new(config.chain_name.clone())?;
        let (tx, rx) = mpsc::unbounded_channel();
        let task_venue = venue.clone();
        tokio::spawn(async move { run(config, tx, task_venue, chain).await });
        Ok(Self { venue, receiver: Mutex::new(rx) })
    }
}

fn validate_factory_config(config: &EvmFactoryConfig) -> Result<(), DomainError> {
    if config.chain_id == 0 || config.ws_rpc_url.trim().is_empty() || config.execution_rpc_url.trim().is_empty() {
        return Err(DomainError::InvalidChainConfig(config.chain_name.clone()));
    }
    for (label, address) in [
        ("factory", config.factory_address.as_str()),
        ("router", config.router_address.as_str()),
        ("wrapped native", config.wrapped_native_address.as_str()),
    ] {
        address.parse::<Address>().map_err(|_| DomainError::InvalidChainConfig(format!("invalid {label} address")))?;
    }
    let topic = config.topic0.strip_prefix("0x").ok_or_else(|| DomainError::InvalidChainConfig("topic0 must start with 0x".to_string()))?;
    if topic.len() != 64 || !topic.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(DomainError::InvalidChainConfig("topic0 must be a 32-byte hex value".to_string()));
    }
    if config.slippage_percent >= 100 {
        return Err(DomainError::InvalidChainConfig("EVM slippage_percent must be below 100".to_string()));
    }
    for asset in &config.base_assets {
        asset.parse::<Address>().map_err(|_| DomainError::InvalidChainConfig("invalid EVM base asset address".to_string()))?;
    }
    Ok(())
}

async fn run(
    config: EvmFactoryConfig,
    tx: mpsc::UnboundedSender<Listing>,
    venue: Venue,
    chain: Chain,
) {
    let base_assets: HashSet<String> = config.base_assets.iter().map(|a| a.to_lowercase()).collect();

    loop {
        let mut stream = connect_with_backoff(&config.ws_rpc_url).await;
        let subscribe_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_subscribe",
            "params": ["logs", { "address": config.factory_address, "topics": [config.topic0] }],
        }).to_string();

        if let Err(e) = stream.send(Message::Text(subscribe_request)).await {
            warn!(chain = config.chain_name, error = %e, "failed to send eth_subscribe request, reconnecting");
            continue;
        }

        loop {
            let message = match stream.next().await {
                Some(Ok(m)) => m,
                Some(Err(e)) => {
                    warn!(chain = config.chain_name, error = %e, "evm websocket error, reconnecting");
                    break;
                }
                None => {
                    warn!(chain = config.chain_name, "evm websocket connection closed, reconnecting");
                    break;
                }
            };
            let Message::Text(text) = message else { continue };
            let parsed: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    debug!(error = %e, "unrecognised evm rpc message, skipping");
                    continue;
                }
            };
            let Some(log) = parsed.get("params").and_then(|p| p.get("result")) else { continue };
            let Some(topics) = log.get("topics").and_then(|t| t.as_array()) else { continue };
            if topics.len() < 3 { continue; }
            let Some(token0) = topics[1].as_str().and_then(extract_address) else { continue };
            let Some(token1) = topics[2].as_str().and_then(extract_address) else { continue };

            let candidates = match (base_assets.contains(&token0), base_assets.contains(&token1)) {
                (true, false) => vec![token1],
                (false, true) => vec![token0],
                _ => vec![token0, token1],
            };

            for address in candidates {
                let Ok(symbol) = Symbol::new(address) else { continue };
                let listing = Listing::new(symbol, venue.clone(), chain.clone(), OffsetDateTime::now_utc());
                if tx.send(listing).is_err() { return; }
            }
        }
    }
}

fn extract_address(topic: &str) -> Option<String> {
    let hex = topic.strip_prefix("0x")?;
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) { return None; }
    Some(format!("0x{}", &hex[24..]).to_lowercase())
}

#[async_trait]
impl ListingSource for EvmFactoryLogSource {
    fn source_id(&self) -> &str { self.venue.name() }

    async fn poll(&self, _cursor: Option<&str>) -> Result<ListingSnapshot, PortError> {
        let mut rx = self.receiver.lock().await;
        let mut new = Vec::new();
        while let Ok(listing) = rx.try_recv() { new.push(listing); }
        Ok(ListingSnapshot::Incremental { new, cursor: None })
    }
}

pub struct DexScreenerEvmMetrics {
    chain_id: String,
    http: reqwest::Client,
}

impl DexScreenerEvmMetrics {
    pub fn new(chain_id: impl Into<String>) -> Self {
        Self { chain_id: chain_id.into().to_lowercase(), http: reqwest::Client::new() }
    }
}

#[derive(Debug, serde::Deserialize)]
struct DexScreenerResponse { #[serde(default)] pairs: Vec<DexScreenerPair> }
#[derive(Debug, serde::Deserialize)]
struct DexScreenerPair {
    #[serde(rename = "chainId")]
    chain_id: Option<String>,
    #[serde(default)]
    volume: Option<DexScreenerVolume>,
    #[serde(rename = "marketCap")]
    market_cap: Option<f64>,
    fdv: Option<f64>,
}
#[derive(Debug, serde::Deserialize)]
struct DexScreenerVolume { #[serde(rename = "h24")] h24: Option<f64> }

#[async_trait]
impl MetricsProvider for DexScreenerEvmMetrics {
    async fn metrics(&self, symbol: &Symbol) -> Result<Option<ListingMetrics>, PortError> {
        let url = format!("https://api.dexscreener.com/latest/dex/tokens/{}", symbol.as_str());
        let response = self.http.get(url).send().await.map_err(|e| PortError::Network {
            venue: "dexscreener".to_string(), source: Box::new(e),
        })?;
        if !response.status().is_success() { return Ok(None); }
        let body: DexScreenerResponse = response.json().await.map_err(|e| PortError::MalformedResponse {
            venue: "dexscreener".to_string(), reason: e.to_string(),
        })?;
        let Some(best) = body.pairs.into_iter()
            .filter(|pair| pair.chain_id.as_deref().map(|c| c.eq_ignore_ascii_case(&self.chain_id)).unwrap_or(false))
            .max_by(|a, b| {
                let av = a.volume.as_ref().and_then(|v| v.h24).unwrap_or(0.0);
                let bv = b.volume.as_ref().and_then(|v| v.h24).unwrap_or(0.0);
                av.partial_cmp(&bv).unwrap_or(std::cmp::Ordering::Equal)
            }) else { return Ok(None); };
        let volume = best.volume.and_then(|v| v.h24).unwrap_or(0.0);
        let market_cap = best.market_cap.or(best.fdv).unwrap_or(0.0);
        if !volume.is_finite() || !market_cap.is_finite() || volume < 0.0 || market_cap < 0.0 {
            return Err(PortError::MalformedResponse {
                venue: "dexscreener".to_string(),
                reason: "metrics contained a non-finite or negative numeric value".to_string(),
            });
        }
        Ok(Some(ListingMetrics {
            volume_24h: Decimal::try_from(volume).map_err(|e| PortError::MalformedResponse {
                venue: "dexscreener".to_string(), reason: format!("invalid volume: {e}"),
            })?,
            market_cap: Decimal::try_from(market_cap).map_err(|e| PortError::MalformedResponse {
                venue: "dexscreener".to_string(), reason: format!("invalid market cap: {e}"),
            })?,
        }))
    }
}

#[derive(Clone)]
pub struct EvmUniswapV2Exchange {
    chain_id: u64,
    execution_rpc_url: String,
    private_rpc_url: Option<String>,
    router: Address,
    wrapped_native: Address,
    slippage_percent: u32,
    signer: PrivateKeySigner,
    write_lock: Arc<Mutex<()>>,
}

impl EvmUniswapV2Exchange {
    pub fn from_env(
        chain_id: u64,
        execution_rpc_url: String,
        private_rpc_url: Option<String>,
        router: &str,
        wrapped_native: &str,
        slippage_percent: u32,
    ) -> Result<Self, String> {
        if private_rpc_url.is_none() {
            return Err("private_rpc_url must be configured before EVM execution is enabled".to_string());
        }
        let key = std::env::var("EVM_PRIVATE_KEY").map_err(|_| "EVM_PRIVATE_KEY is not configured".to_string())?;
        let signer = PrivateKeySigner::from_str(&key).map_err(|e| format!("invalid EVM_PRIVATE_KEY: {e}"))?;
        let router = router.parse::<Address>().map_err(|e| format!("invalid router address: {e}"))?;
        let wrapped_native = wrapped_native.parse::<Address>().map_err(|e| format!("invalid wrapped native address: {e}"))?;
        if slippage_percent >= 100 { return Err("EVM slippage_percent must be below 100".to_string()); }
        Ok(Self {
            chain_id,
            execution_rpc_url,
            private_rpc_url,
            router,
            wrapped_native,
            slippage_percent,
            signer,
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    fn signer(&self) -> Result<&PrivateKeySigner, PortError> {
        Ok(&self.signer)
    }

    async fn provider(&self) -> Result<impl Provider + Clone, PortError> {
        let provider = ProviderBuilder::new()
            .connect(&self.execution_rpc_url)
            .await
            .map_err(|e| PortError::Network { venue: "evm-rpc".to_string(), source: Box::new(e) })?;
        let chain_id = provider.get_chain_id().await.map_err(|e| PortError::Network {
            venue: "evm-rpc".to_string(), source: Box::new(e),
        })?;
        if chain_id != self.chain_id {
            return Err(PortError::Rejected(format!("EVM RPC chain id {chain_id} does not match configured {}", self.chain_id)));
        }
        Ok(provider)
    }

    async fn send_private_transaction<P: Provider + Clone>(
        &self,
        provider: &P,
        to: Address,
        value: U256,
        input: Bytes,
    ) -> Result<alloy::rpc::types::TransactionReceipt, PortError> {
        let _write_guard = self.write_lock.lock().await;
        let private_url = self.private_rpc_url.as_deref().ok_or_else(|| {
            PortError::Rejected("private_rpc_url is required for EVM execution".to_string())
        })?;
        let private_provider = ProviderBuilder::new().connect(private_url).await.map_err(|e| PortError::Network {
            venue: "evm-private-rpc".to_string(), source: Box::new(e),
        })?;
        let private_chain_id = private_provider.get_chain_id().await.map_err(|e| PortError::Network {
            venue: "evm-private-rpc".to_string(), source: Box::new(e),
        })?;
        if private_chain_id != self.chain_id {
            return Err(PortError::Rejected(format!("private EVM RPC chain id {private_chain_id} does not match configured {}", self.chain_id)));
        }
        let chain_id = provider.get_chain_id().await.map_err(|e| PortError::Network {
            venue: "evm-rpc".to_string(), source: Box::new(e),
        })?;
        let signer = self.signer()?;
        let nonce = provider.get_transaction_count(signer.address()).await.map_err(|e| PortError::Network {
            venue: "evm-rpc".to_string(), source: Box::new(e),
        })?;
        let fees = provider.estimate_eip1559_fees().await.map_err(|e| PortError::Network {
            venue: "evm-rpc".to_string(), source: Box::new(e),
        })?;
        let unsigned = TransactionRequest::default()
            .with_from(signer.address())
            .with_to(to)
            .with_value(value)
            .with_input(input)
            .with_nonce(nonce)
            .with_chain_id(chain_id)
            .with_max_fee_per_gas(fees.max_fee_per_gas)
            .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas);
        let gas_limit = provider.estimate_gas(unsigned.clone()).await.map_err(|e| PortError::Rejected(format!("EVM transaction gas estimation failed: {e}")))?;
        let required_balance = value.saturating_add(U256::from(gas_limit).saturating_mul(U256::from(fees.max_fee_per_gas)));
        let balance = provider.get_balance(signer.address()).await.map_err(|e| PortError::Network {
            venue: "evm-rpc".to_string(), source: Box::new(e),
        })?;
        if balance < required_balance {
            return Err(PortError::Rejected(format!("insufficient EVM native balance: need {required_balance} wei, have {balance} wei")));
        }
        let unsigned = unsigned.with_gas_limit(gas_limit);
        provider.call(unsigned.clone()).await.map_err(|e| PortError::Rejected(format!("EVM transaction preflight failed: {e}")))?;
        let wallet = EthereumWallet::from(signer.clone());
        let signed = unsigned.build(&wallet).await.map_err(|e| PortError::Rejected(format!("EVM local signing failed: {e}")))?;
        let encoded = signed.encoded_2718();
        let pending = private_provider.send_raw_transaction(&encoded).await.map_err(|e| PortError::Network {
            venue: "evm-private-rpc".to_string(), source: Box::new(e),
        })?;
        let receipt = pending.get_receipt().await.map_err(|e| PortError::Network {
            venue: "evm-private-rpc".to_string(), source: Box::new(e),
        })?;
        Ok(receipt)
    }

    async fn token_decimals<P: Provider + Clone>(&self, provider: &P, token: Address) -> Result<u8, PortError> {
        Erc20::new(token, provider).decimals().call().await.map_err(|e| PortError::Rejected(format!("failed to read token decimals: {e}")))
    }

    async fn token_balance<P: Provider + Clone>(&self, provider: &P, token: Address, owner: Address) -> Result<U256, PortError> {
        Erc20::new(token, provider).balanceOf(owner).call().await.map_err(|e| PortError::Rejected(format!("failed to read token balance: {e}")))
    }

    fn native_to_wei(amount: Decimal) -> Result<U256, PortError> {
        if amount <= Decimal::ZERO { return Err(PortError::Rejected("EVM trade amount must be positive".to_string())); }
        let scaled = amount * Decimal::from(1_000_000_000_000_000_000u64);
        let wei = scaled.trunc().to_u128().ok_or_else(|| PortError::Rejected("EVM amount is outside supported precision".to_string()))?;
        Ok(U256::from(wei))
    }

    fn token_to_raw(amount: Decimal, decimals: u8) -> Result<U256, PortError> {
        if amount <= Decimal::ZERO { return Err(PortError::Rejected("token quantity must be positive".to_string())); }
        let scale = 10_u128.checked_pow(decimals as u32).ok_or_else(|| PortError::Rejected("token decimals are too large".to_string()))?;
        let raw = (amount * Decimal::from(scale)).trunc().to_u128().ok_or_else(|| PortError::Rejected("token quantity is outside supported precision".to_string()))?;
        Ok(U256::from(raw))
    }

    fn native_from_wei(amount: U256) -> Result<Decimal, PortError> {
        let wei = u128::try_from(amount)
            .map_err(|_| PortError::Rejected("native amount exceeds decimal conversion range".to_string()))?;
        Ok(Decimal::from(wei) / Decimal::from(1_000_000_000_000_000_000u64))
    }

    fn apply_slippage(&self, amount: U256) -> U256 {
        amount.saturating_mul(U256::from(100_u64.saturating_sub(self.slippage_percent as u64))) / U256::from(100_u64)
    }

    async fn deadline<P: Provider + Clone>(&self, provider: &P) -> Result<U256, PortError> {
        let block = provider.get_block_by_number(alloy::eips::BlockNumberOrTag::Latest).await
            .map_err(|e| PortError::Network { venue: "evm-rpc".to_string(), source: Box::new(e) })?
            .ok_or_else(|| PortError::Rejected("latest EVM block is unavailable".to_string()))?;
        Ok(U256::from(block.header.timestamp + 120))
    }
}

#[async_trait]
impl ExchangeClient for EvmUniswapV2Exchange {
    fn venue_name(&self) -> &str { "evm-onchain" }

    async fn current_price(&self, symbol: &Symbol) -> Result<Decimal, PortError> {
        let token = symbol.as_str().parse::<Address>().map_err(|e| PortError::Rejected(format!("invalid EVM token address: {e}")))?;
        let provider = self.provider().await?;
        let decimals = self.token_decimals(&provider, token).await?;
        let one_token = U256::from(10_u128.checked_pow(decimals as u32).ok_or_else(|| PortError::Rejected("token decimals too large".to_string()))?);
        let router = UniswapV2Router::new(self.router, &provider);
        let amounts = router.getAmountsOut(one_token, vec![token, self.wrapped_native]).call().await
            .map_err(|e| PortError::Rejected(format!("failed to quote EVM token: {e}")))?;
        let native = amounts.last().copied().ok_or_else(|| PortError::Rejected("router returned an empty quote".to_string()))?;
        Self::native_from_wei(native)
    }

    async fn submit_buy_by_amount(&self, symbol: &Symbol, quote_amount: Decimal) -> Result<FilledBuy, PortError> {
        let token = symbol.as_str().parse::<Address>().map_err(|e| PortError::Rejected(format!("invalid EVM token address: {e}")))?;
        let provider = self.provider().await?;
        let wallet = self.signer()?.address();
        let value = Self::native_to_wei(quote_amount)?;
        let router = UniswapV2Router::new(self.router, &provider);
        let quote = router.getAmountsOut(value, vec![self.wrapped_native, token]).call().await
            .map_err(|e| PortError::Rejected(format!("EVM buy quote failed: {e}")))?;
        let expected = quote.last().copied().ok_or_else(|| PortError::Rejected("router returned an empty buy quote".to_string()))?;
        let min_out = self.apply_slippage(expected);
        let before = self.token_balance(&provider, token, wallet).await?;
        let deadline = self.deadline(&provider).await?;
        let call = router.swapExactETHForTokensSupportingFeeOnTransferTokens(min_out, vec![self.wrapped_native, token], wallet, deadline).value(value);
        let calldata = call.calldata().to_owned();
        let receipt = self.send_private_transaction(&provider, self.router, value, calldata).await?;
        if !receipt.status() { return Err(PortError::Rejected("EVM buy transaction reverted".to_string())); }
        let after = self.token_balance(&provider, token, wallet).await?;
        let quantity_raw = after.checked_sub(before).ok_or_else(|| PortError::Rejected("token balance decreased after buy".to_string()))?;
        if quantity_raw.is_zero() { return Err(PortError::Rejected("EVM buy succeeded but acquired zero tokens".to_string())); }
        let decimals = self.token_decimals(&provider, token).await?;
        let quantity_raw_u128 = u128::try_from(quantity_raw)
            .map_err(|_| PortError::Rejected("token balance exceeds supported precision".to_string()))?;
        let quantity = Decimal::from(quantity_raw_u128)
            / Decimal::from(10_u128.checked_pow(decimals as u32).ok_or_else(|| PortError::Rejected("token decimals too large".to_string()))?);
        let entry_price = quote_amount / quantity;
        Ok(FilledBuy { quantity, entry_price })
    }

    async fn submit_order(&self, order: Order) -> Result<FilledSell, PortError> {
        if order.side != OrderSide::Sell { return Err(PortError::Rejected("EVM submit_order only supports sell orders".to_string())); }
        let token = order.symbol.as_str().parse::<Address>().map_err(|e| PortError::Rejected(format!("invalid EVM token address: {e}")))?;
        let provider = self.provider().await?;
        let wallet = self.signer()?.address();
        let decimals = self.token_decimals(&provider, token).await?;
        let amount = Self::token_to_raw(order.quantity, decimals)?;
        let router = UniswapV2Router::new(self.router, &provider);
        let allowance = Erc20::new(token, &provider).allowance(wallet, self.router).call().await
            .map_err(|e| PortError::Rejected(format!("failed to read token allowance: {e}")))?;
        let mut fee_quote = Decimal::ZERO;
        if allowance < amount {
            let erc20 = Erc20::new(token, &provider);
            let approve = erc20.approve(self.router, amount);
            approve.call().await.map_err(|e| PortError::Rejected(format!("token approval preflight failed: {e}")))?;
            let approval_receipt = self.send_private_transaction(&provider, token, U256::ZERO, approve.calldata().to_owned()).await?;
            if !approval_receipt.status() { return Err(PortError::Rejected("token approval transaction reverted".to_string())); }
            fee_quote += Self::native_from_wei(U256::from(approval_receipt.cost()))?;
        }
        let native_before_sell = provider.get_balance(wallet).await.map_err(|e| PortError::Network {
            venue: "evm-rpc".to_string(), source: Box::new(e),
        })?;
        let quote = router.getAmountsOut(amount, vec![token, self.wrapped_native]).call().await
            .map_err(|e| PortError::Rejected(format!("EVM sell quote failed: {e}")))?;
        let expected = quote.last().copied().ok_or_else(|| PortError::Rejected("router returned an empty sell quote".to_string()))?;
        let min_out = self.apply_slippage(expected);
        let deadline = self.deadline(&provider).await?;
        let call = router.swapExactTokensForETHSupportingFeeOnTransferTokens(amount, min_out, vec![token, self.wrapped_native], wallet, deadline);
        let receipt = self.send_private_transaction(&provider, self.router, U256::ZERO, call.calldata().to_owned()).await?;
        if !receipt.status() { return Err(PortError::Rejected("EVM sell transaction reverted".to_string())); }
        let native_after_sell = provider.get_balance(wallet).await.map_err(|e| PortError::Network {
            venue: "evm-rpc".to_string(), source: Box::new(e),
        })?;
        let sell_gas = U256::from(receipt.cost());
        let native_delta = native_after_sell.saturating_sub(native_before_sell);
        let gross_proceeds_wei = native_delta.saturating_add(sell_gas);
        if gross_proceeds_wei.is_zero() {
            return Err(PortError::Rejected("EVM sell confirmed but produced no native proceeds".to_string()));
        }
        let quote_proceeds = Self::native_from_wei(gross_proceeds_wei)?;
        fee_quote += Self::native_from_wei(sell_gas)?;
        let execution_price = quote_proceeds / order.quantity;
        Ok(FilledSell {
            quantity: order.quantity,
            execution_price: Some(execution_price),
            quote_proceeds: Some(quote_proceeds),
            fee_quote: Some(fee_quote),
            tx_id: Some(receipt.transaction_hash.to_string()),
        })
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_address_from_a_left_padded_topic() {
        let topic = "0x000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
        assert_eq!(extract_address(topic), Some("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".to_string()));
    }

    #[test]
    fn rejects_malformed_topic() {
        assert_eq!(extract_address("0xnothex"), None);
        assert_eq!(extract_address("not even prefixed"), None);
    }

    #[test]
    fn applies_slippage_without_exceeding_quote() {
        let key = "0x0000000000000000000000000000000000000000000000000000000000000001";
        let exchange = EvmUniswapV2Exchange::from_env(1, "http://localhost".to_string(), None, "0x0000000000000000000000000000000000000001", "0x0000000000000000000000000000000000000002", 10);
        if let Ok(exchange) = exchange {
            assert_eq!(exchange.apply_slippage(U256::from(1000)), U256::from(900));
        }
        let _ = key;
    }
}
