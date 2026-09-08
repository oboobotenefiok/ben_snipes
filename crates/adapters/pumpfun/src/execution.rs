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

use crate::retry::with_retry;
use rust_decimal::Decimal;
use solana_sdk::signature::Signature;
use solana_sdk::signer::keypair::Keypair;
use solana_sdk::signer::{SeedDerivable, Signer};
use solana_sdk::transaction::VersionedTransaction;
use std::env;

const TRADE_LOCAL_URL: &str = "https://pumpportal.fun/api/trade-local";

/// Loads the wallet keypair from the `SOLANA_PRIVATE_KEY` environment
/// variable. Never reads from a file this codebase writes, never logs
/// the value (not even in error messages - every error path below
/// describes *what's wrong*, never echoes `raw` or the decoded bytes),
/// and never falls back to a default - there is no safe default for a
/// private key.
///
/// Two encodings are accepted, matching how the ecosystem actually
/// exports keys:
/// - base58 (no prefix) - what `solana-keygen` and most Solana wallet
///   exports use.
/// - hex, `0x`-prefixed - what some wallets/tooling export instead.
///   The `0x` prefix is required to disambiguate from base58 rather
///   than guessing from content: hex's alphabet is a subset of
///   base58's, so an un-prefixed string is always treated as base58.
///
/// Both the 64-byte full keypair representation (32-byte secret + its
/// matching 32-byte public key, what `solana-keygen` writes) and a
/// bare 32-byte secret seed are accepted - some wallets export only
/// the seed. Any other decoded length is a malformed key and fails
/// closed rather than guessing.
///
/// **Unverified until first compile** (see this file's module docs):
/// the 32-byte path uses `solana_sdk::signer::SeedDerivable::from_seed`,
/// confirmed present at that exact path as of solana-sdk 2.1.x's
/// published docs, but this crate pins solana-sdk 4.x - the same
/// Anza-fork restructuring this file already flags elsewhere means
/// that path should be re-checked against the actual pinned version's
/// docs.rs page before relying on it with real funds.
pub fn load_wallet() -> Result<Keypair, String> {
    let raw = env::var("SOLANA_PRIVATE_KEY")
        .map_err(|_| "SOLANA_PRIVATE_KEY environment variable is not set".to_string())?;

    let bytes = decode_key_bytes(raw.trim())?;

    match bytes.len() {
        64 => Keypair::try_from(bytes.as_slice())
            .map_err(|e| format!("SOLANA_PRIVATE_KEY did not decode to a valid keypair: {e}")),
        32 => Keypair::from_seed(&bytes)
            .map_err(|e| format!("SOLANA_PRIVATE_KEY (32-byte seed) did not produce a valid keypair: {e}")),
        other => Err(format!(
            "SOLANA_PRIVATE_KEY decoded to {other} bytes; expected 32 (a secret seed) or 64 (a full keypair)"
        )),
    }
}

/// Decodes `raw` as hex if it's `0x`-prefixed, otherwise as base58.
fn decode_key_bytes(raw: &str) -> Result<Vec<u8>, String> {
    if let Some(hex_digits) = raw.strip_prefix("0x") {
        return decode_hex(hex_digits);
    }
    bs58::decode(raw)
        .into_vec()
        .map_err(|e| format!("SOLANA_PRIVATE_KEY is not valid base58 (and has no 0x prefix for hex): {e}"))
}

fn decode_hex(digits: &str) -> Result<Vec<u8>, String> {
    if digits.is_empty() || digits.len() % 2 != 0 {
        return Err("SOLANA_PRIVATE_KEY has an odd number of hex digits after 0x".to_string());
    }
    (0..digits.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&digits[i..i + 2], 16)
                .map_err(|_| "SOLANA_PRIVATE_KEY contains a non-hex character after 0x".to_string())
        })
        .collect()
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
    let raw_tx_bytes = build_unsigned_transaction(http, wallet, request).await?;
    simulate_transaction(http, rpc_url, &raw_tx_bytes).await?;

    let signed_bytes = sign_transaction(wallet, &raw_tx_bytes)?;
    broadcast(http, rpc_url, &signed_bytes).await
}

/// Builds and simulates a PumpPortal transaction without signing or
/// broadcasting it. This is used immediately before exits, when the wallet
/// already owns the token, so the simulation can validate the actual current
/// account state rather than merely checking that a route exists.
pub async fn preflight_trade(
    http: &reqwest::Client,
    wallet: &Keypair,
    rpc_url: &str,
    request: &TradeRequest,
) -> Result<(), String> {
    let raw_tx_bytes = build_unsigned_transaction(http, wallet, request).await?;
    simulate_transaction(http, rpc_url, &raw_tx_bytes).await
}

async fn build_unsigned_transaction(
    http: &reqwest::Client,
    wallet: &Keypair,
    request: &TradeRequest,
) -> Result<Vec<u8>, String> {
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

    with_retry(3, || async {
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

        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|e| format!("failed to read trade-local response body: {e}"))
    })
    .await
}

/// Simulates the unsigned transaction before spending signing material or
/// sending it to the cluster. Solana explicitly permits unsigned simulation
/// when `sigVerify` is false, so this catches instruction-level failures
/// before the wallet signs a doomed transaction.
async fn simulate_transaction(
    http: &reqwest::Client,
    rpc_url: &str,
    raw_tx_bytes: &[u8],
) -> Result<(), String> {
    use base64::Engine;

    let encoded = base64::engine::general_purpose::STANDARD.encode(raw_tx_bytes);
    let rpc_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "simulateTransaction",
        "params": [
            encoded,
            {
                "encoding": "base64",
                "commitment": "confirmed",
                "sigVerify": false,
                "replaceRecentBlockhash": false,
            }
        ],
    });

    let response = http
        .post(rpc_url)
        .header("Content-Type", "application/json")
        .body(rpc_body.to_string())
        .send()
        .await
        .map_err(|e| format!("transaction simulation request failed: {e}"))?;

    let response_json: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("failed to parse transaction simulation response: {e}"))?;

    if let Some(error) = response_json.get("error") {
        return Err(format!("transaction simulation RPC returned an error: {error}"));
    }

    match response_json.pointer("/result/value/err") {
        Some(err) if !err.is_null() => {
            Err(format!("transaction simulation failed: {err}"))
        }
        Some(_) => Ok(()),
        None => Err(format!(
            "transaction simulation response had no result/value/err field: {response_json}"
        )),
    }
}

/// Deserializes PumpPortal's unsigned transaction bytes, signs the
/// message with `wallet`, and re-serializes. See this module's top
/// doc comment - this is the block to re-verify against the pinned
/// solana-sdk version's docs.rs page before trusting it with real funds.
fn sign_transaction(wallet: &Keypair, raw_tx_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut tx: VersionedTransaction = bincode::deserialize(raw_tx_bytes)
        .map_err(|e| format!("failed to deserialize transaction from trade-local: {e}"))?;

    tx.sanitize()
        .map_err(|e| format!("trade-local returned an invalid transaction: {e}"))?;

    let account_keys = tx.message.static_account_keys();
    let signer_index = account_keys
        .iter()
        .position(|key| *key == wallet.pubkey())
        .ok_or_else(|| "wallet public key not found among the transaction's required signers".to_string())?;

    let required_signatures = tx.message.header().num_required_signatures as usize;
    if signer_index >= required_signatures {
        return Err("wallet public key is present in the transaction but is not a required signer".to_string());
    }

    if tx.signatures.len() != required_signatures {
        return Err(format!(
            "transaction signature slot count mismatch: expected {required_signatures}, got {}",
            tx.signatures.len()
        ));
    }

    let message_bytes = tx.message.serialize();
    let signature = wallet.sign_message(&message_bytes);
    if signature == Signature::default() || !signature.verify(wallet.pubkey().as_ref(), &message_bytes) {
        return Err("wallet failed to produce a verifiable transaction signature".to_string());
    }

    tx.signatures[signer_index] = signature;

    bincode::serialize(&tx).map_err(|e| format!("failed to re-serialize signed transaction: {e}"))
}

/// Broadcasts already-signed transaction bytes via a raw JSON-RPC
/// `sendTransaction` call. Deliberately not using the `solana-client`
/// crate - see this module's top doc comment for why.
async fn broadcast(http: &reqwest::Client, rpc_url: &str, signed_bytes: &[u8]) -> Result<String, String> {    use base64::Engine;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_base58_without_a_prefix() {
        let bytes = decode_key_bytes(&bs58::encode([7u8; 32]).into_string()).expect("valid base58 should decode");
        assert_eq!(bytes, vec![7u8; 32]);
    }

    #[test]
    fn decodes_0x_prefixed_hex() {
        let hex_str = format!("0x{}", "ab".repeat(32));
        let bytes = decode_key_bytes(&hex_str).expect("valid 0x-prefixed hex should decode");
        assert_eq!(bytes, vec![0xabu8; 32]);
    }

    #[test]
    fn rejects_odd_length_hex() {
        let result = decode_key_bytes("0xabc");
        assert!(result.is_err(), "an odd number of hex digits is malformed and must not silently truncate");
    }

    #[test]
    fn rejects_non_hex_characters_after_0x_prefix() {
        let result = decode_key_bytes("0xzzzz");
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_string_that_is_neither_valid_hex_nor_valid_base58() {
        // '0', 'O', 'I', 'l' are all excluded from the base58 alphabet.
        let result = decode_key_bytes("0OIl-not-a-real-key");
        assert!(result.is_err());
    }

    #[test]
    fn a_bare_hex_looking_string_without_0x_is_still_treated_as_base58() {
        // Every character here is a valid base58 character (and happens
        // to also be a valid hex digit), so without the 0x prefix this
        // must go through the base58 decoder, not be silently
        // reinterpreted as hex. Hex-decoding "abcdef" would yield
        // exactly 3 bytes ([0xab, 0xcd, 0xef]); base58-decoding it does
        // not, which is what actually distinguishes the two paths here.
        let decoded = decode_key_bytes("abcdef").expect("should decode as base58");
        assert_ne!(decoded, vec![0xab, 0xcd, 0xef], "must not have been silently treated as hex");
        assert_eq!(
            decoded,
            bs58::decode("abcdef").into_vec().expect("test fixture should be valid base58")
        );
    }
}
