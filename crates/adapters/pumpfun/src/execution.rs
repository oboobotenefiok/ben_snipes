//! Signing and broadcast for PumpPortal's non-custodial Local
//! Transaction API (`/api/trade-local`): they build an unsigned
//! transaction, we sign it locally and broadcast it ourselves, so the
//! private key never leaves this process. Verified against PumpPortal's
//! published docs and multiple independent third-party examples at the
//! time of writing - request shape, and the fact the response is raw
//! transaction bytes rather than JSON, are both cross-confirmed.
//!
//! # The one section to re-verify before running with real funds
//!
//! `solana-sdk` went through a major breaking restructuring recently
//! (the Anza fork, v3 -> v4: `Keypair::from_bytes` was replaced by
//! `Keypair::try_from`, `Pubkey` became a type alias for a new
//! `Address` type, and the crate split into many granular sub-crates).
//! That means my working knowledge of this specific API has a real
//! chance of being stale in exactly the way that matters most here.
//!
//! Rather than reach for higher-level convenience constructors I
//! couldn't independently confirm still exist with the same shape, the
//! signing step below is built on the most fundamental, least-likely-
//! to-have-changed primitives: deserialize the raw bincode bytes into a
//! `VersionedTransaction`, sign the message bytes directly via the
//! `Signer` trait's `sign_message`, and place the resulting signature at
//! the matching index in `signatures`. Broadcast uses a raw JSON-RPC
//! `sendTransaction` call via `reqwest` rather than the `solana-client`
//! crate, specifically to avoid a second axis of API-version
//! uncertainty on top of the signing step - the JSON-RPC wire protocol
//! itself is far more stable than any one crate's Rust bindings to it.
//!
//! **Before running this against real funds:** open docs.rs for the
//! exact `solana-sdk` version pinned in this crate's `Cargo.toml` and
//! confirm `VersionedTransaction`, `VersionedMessage::static_account_keys`,
//! and `VersionedMessage::serialize` still have the shapes assumed
//! below, and that `bincode::deserialize`/`bincode::serialize` (this
//! crate pins `bincode = "1"`, the classic serde-based API) still
//! round-trip `VersionedTransaction` correctly for the current
//! solana-sdk version - if that assumption is wrong, `cargo build` will
//! fail with a clear trait-bound error rather than silently misbehave,
//! which is the safer of the two failure modes, but it does mean this
//! specific file is the most likely one to need a fix on first build.
//! This is the single highest-risk block of code in this project - it
//! moves money.

use rust_decimal::Decimal;
use solana_sdk::signer::keypair::Keypair;
use solana_sdk::signer::Signer;
use solana_sdk::transaction::VersionedTransaction;
use std::env;

const TRADE_LOCAL_URL: &str = "https://pumpportal.fun/api/trade-local";

/// Loads the wallet keypair from the `SOLANA_PRIVATE_KEY` environment
/// variable. Never reads from a file this codebase writes, never logs
/// the value (not even in error messages), and never falls back to a
/// default - there is no safe default for a private key. Expects the
/// base58-encoded 64-byte secret key format that `solana-keygen` and
/// most wallet exports use.
pub fn load_wallet() -> Result<Keypair, String> {
    let raw = env::var("SOLANA_PRIVATE_KEY")
        .map_err(|_| "SOLANA_PRIVATE_KEY environment variable is not set".to_string())?;

    let bytes = bs58::decode(raw.trim())
        .into_vec()
        .map_err(|e| format!("SOLANA_PRIVATE_KEY is not valid base58: {e}"))?;

    Keypair::try_from(bytes.as_slice())
        .map_err(|e| format!("SOLANA_PRIVATE_KEY did not decode to a valid keypair: {e}"))
}

/// Convenience for callers that just want to log/display the wallet's
/// address without depending on `solana_sdk::signer::Signer` themselves
/// - keeps that dependency an implementation detail of this crate.
pub fn wallet_pubkey_string(wallet: &Keypair) -> String {
    wallet.pubkey().to_string()
}

/// A trade to submit through PumpPortal's Local Transaction API.
///
/// Note this is deliberately **not** shaped like `ExchangeClient::submit_order`
/// (which takes a token quantity) - see this crate's top-level docs for
/// why. PumpPortal's own interface is "spend this much SOL" for a buy,
/// or "sell this many tokens / this % of holdings" for a sell, and
/// forcing that into a pre-computed-quantity shape would mean either
/// fabricating a price (bonding-curve math not implemented here) or
/// silently mismatching what PumpPortal is actually asked to do.
pub struct TradeRequest {
    pub action: TradeAction,
    pub mint: String,
    /// For a buy: amount of SOL to spend, as a decimal string (e.g.
    /// "0.05"). For a sell: amount of tokens, or a percentage string
    /// like "100%" to sell the whole balance - PumpPortal accepts both
    /// shapes for `amount` on a sell.
    pub amount: String,
    pub slippage_percent: u32,
    pub priority_fee_sol: Decimal,
}

#[derive(Debug, Clone, Copy)]
pub enum TradeAction {
    Buy,
    Sell,
}

impl TradeAction {
    fn as_str(&self) -> &'static str {
        match self {
            TradeAction::Buy => "buy",
            TradeAction::Sell => "sell",
        }
    }

    /// PumpPortal's `denominatedInSol` flag: a buy's `amount` is a SOL
    /// figure, a sell's `amount` is a token figure (or percentage).
    fn denominated_in_sol(&self) -> &'static str {
        match self {
            TradeAction::Buy => "true",
            TradeAction::Sell => "false",
        }
    }
}

/// Requests, signs, and broadcasts one trade. Returns the transaction
/// signature (base58) on success.
pub async fn execute_trade(
    http: &reqwest::Client,
    wallet: &Keypair,
    rpc_url: &str,
    request: &TradeRequest,
) -> Result<String, String> {
    let body = serde_json::json!({
        "publicKey": wallet.pubkey().to_string(),
        "action": request.action.as_str(),
        "mint": request.mint,
        "denominatedInSol": request.action.denominated_in_sol(),
        // Sent as a JSON string unconditionally (covers both "0.05" and
        // "100%"). PumpPortal's own examples show amount as a bare
        // number in some places and a quoted string in others, which
        // reads as lenient/coercing parsing on their end rather than a
        // strict schema - if a trade gets rejected specifically citing
        // the amount field, that assumption is the first thing to check.
        "amount": request.amount,
        "slippage": request.slippage_percent,
        "priorityFee": request.priority_fee_sol.to_string(),
        "pool": "auto",
    });

    let response = http
        .post(TRADE_LOCAL_URL)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| format!("trade-local request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("trade-local returned {status}: {text}"));
    }

    let raw_tx_bytes = response
        .bytes()
        .await
        .map_err(|e| format!("failed to read trade-local response body: {e}"))?;

    let signed_bytes = sign_transaction(wallet, &raw_tx_bytes)?;
    broadcast(http, rpc_url, &signed_bytes).await
}

/// Deserializes PumpPortal's unsigned transaction bytes, signs the
/// message with `wallet`, and re-serializes. See this module's top
/// doc comment - this is the block to re-verify against the pinned
/// solana-sdk version's docs.rs page before trusting it with real funds.
fn sign_transaction(wallet: &Keypair, raw_tx_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut tx: VersionedTransaction = bincode::deserialize(raw_tx_bytes)
        .map_err(|e| format!("failed to deserialize transaction from trade-local: {e}"))?;

    let account_keys = tx.message.static_account_keys();
    let signer_index = account_keys
        .iter()
        .position(|key| *key == wallet.pubkey())
        .ok_or_else(|| "wallet public key not found among the transaction's required signers".to_string())?;

    let message_bytes = tx.message.serialize();
    let signature = wallet.sign_message(&message_bytes);
    tx.signatures[signer_index] = signature;

    bincode::serialize(&tx).map_err(|e| format!("failed to re-serialize signed transaction: {e}"))
}

/// Broadcasts already-signed transaction bytes via a raw JSON-RPC
/// `sendTransaction` call. Deliberately not using the `solana-client`
/// crate - see this module's top doc comment for why.
async fn broadcast(http: &reqwest::Client, rpc_url: &str, signed_bytes: &[u8]) -> Result<String, String> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(signed_bytes);

    let rpc_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendTransaction",
        "params": [encoded, { "encoding": "base64", "skipPreflight": false, "maxRetries": 3 }],
    });

    let response = http
        .post(rpc_url)
        .header("Content-Type", "application/json")
        .body(rpc_body.to_string())
        .send()
        .await
        .map_err(|e| format!("RPC sendTransaction request failed: {e}"))?;

    let response_json: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("failed to parse RPC response: {e}"))?;

    if let Some(error) = response_json.get("error") {
        return Err(format!("RPC rejected the transaction: {error}"));
    }

    response_json
        .get("result")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("RPC response had no result field: {response_json}"))
}
