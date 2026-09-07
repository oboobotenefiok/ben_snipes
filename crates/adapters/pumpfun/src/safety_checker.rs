//! Real `TokenSafetyChecker` for Solana via RugCheck (`api.rugcheck.xyz`).
//!
//! **Confidence varies significantly by field, and that's reflected in
//! how conservatively each one defaults.** `mintAuthority` /
//! `freezeAuthority` are confirmed - independently corroborated by both
//! an unofficial API wrapper's documented field list and an AI-skill
//! doc that explicitly describes `token.mintAuthority != null` as the
//! "can still mint" signal - so `is_mintable` and half of
//! `ownership_renounced` rest on solid ground. Liquidity-lock detection
//! (via the `lockers` field) and sell-tax are much less certain: RugCheck
//! doesn't appear to reliably expose a sell-tax figure at all (it's
//! fundamentally a different kind of check - authority/liquidity/holder
//! analysis, not a sell simulation), so `sell_tax_bps` here is **always
//! `None`, which means unverified and therefore blocked by the safety gate.** If accurate sell-tax
//! detection matters for your risk tolerance, that needs a real sell
//! simulation as a separate data source - don't read a `0` from this
//! checker as "no tax", read it as "this checker doesn't know."
//!
//! **A real limitation worth being direct about:** if `mintAuthority`/
//! `freezeAuthority` turn out not to be the actual field names RugCheck
//! uses (moderate but not total confidence - see above), those fields
//! deserialize to `None` the same way a genuinely-renounced authority
//! would, and `is_mintable`/`ownership_renounced` would silently read as
//! "safe" for every token. That's fail-*open*, the opposite of this
//! codebase's standing principle. The one thing this code *can* check at
//! runtime - whether the `token` sub-object exists at all - is checked
//! below and treated as "not enough information" (`None`) if it's
//! missing entirely, which catches a badly-wrong response shape. It
//! cannot catch "the token object is there but these two specific key
//! names are wrong." **Verify `mintAuthority`/`freezeAuthority` against
//! a real RugCheck response before trusting this for real funds** - the
//! same category of caveat as the solana-sdk signing code, for the same
//! reason: unverified assumption, safety-critical consequence if wrong.

use async_trait::async_trait;
use ben_snipes_domain::{SafetyReport, Symbol};
use ben_snipes_ports::{PortError, TokenSafetyChecker};
use serde::Deserialize;
use serde_json::Value;

const REPORT_URL: &str = "https://api.rugcheck.xyz/v1/tokens";

#[derive(Debug, Default, Deserialize)]
struct RugCheckReport {
    #[serde(default)]
    token: Option<TokenInfo>,
    /// Present when RugCheck has directly flagged the token as a
    /// confirmed rug - if this is `true`, nothing else in the report
    /// matters.
    #[serde(default)]
    rugged: bool,
    /// Left as a raw `Value` rather than a typed field - only its
    /// presence/non-emptiness is used (see module docs on why the exact
    /// lock-percentage shape isn't confidently known).
    #[serde(default)]
    lockers: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct TokenInfo {
    #[serde(default, rename = "mintAuthority")]
    mint_authority: Option<Value>,
    #[serde(default, rename = "freezeAuthority")]
    freeze_authority: Option<Value>,
}

const SPL_TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022_PROGRAM_ID: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

#[derive(Debug, Clone, Copy)]
struct MintPolicy {
    transfer_fee_bps: Option<u32>,
    permanent_delegate: bool,
    transfer_hook: bool,
}

fn max_transfer_fee_bps(value: &Value) -> Option<u32> {
    fn walk(value: &Value, max_bps: &mut Option<u32>) {
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    if key == "transferFeeBasisPoints" {
                        if let Some(raw) = child.as_u64().and_then(|value| u32::try_from(value).ok()) {
                            *max_bps = Some(max_bps.map_or(raw, |current| current.max(raw)));
                        }
                    }
                    walk(child, max_bps);
                }
            }
            Value::Array(array) => {
                for child in array {
                    walk(child, max_bps);
                }
            }
            _ => {}
        }
    }

    let mut max_bps = None;
    walk(value, &mut max_bps);
    max_bps
}

fn extension_named(value: &Value, extension_name: &str) -> bool {
    match value {
        Value::Object(object) => {
            if object.get("extension").and_then(Value::as_str) == Some(extension_name) {
                return true;
            }
            object.values().any(|child| extension_named(child, extension_name))
        }
        Value::Array(array) => array.iter().any(|child| extension_named(child, extension_name)),
        _ => false,
    }
}

impl RugCheckSafetyChecker {
    async fn inspect_mint_policy(&self, mint: &str) -> Result<Option<MintPolicy>, PortError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getAccountInfo",
            "params": [mint, { "encoding": "jsonParsed", "commitment": "confirmed" }],
        });

        let response = self
            .http
            .post(&self.rpc_url)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| PortError::Network {
                venue: "solana-rpc".to_string(),
                source: Box::new(e),
            })?;

        if !response.status().is_success() {
            return Ok(None);
        }

        let json: Value = response.json().await.map_err(|e| PortError::MalformedResponse {
            venue: "solana-rpc".to_string(),
            reason: e.to_string(),
        })?;

        let Some(account) = json.pointer("/result/value") else {
            return Ok(None);
        };
        if account.is_null() {
            return Ok(None);
        }

        let owner = account.get("owner").and_then(Value::as_str);
        match owner {
            Some(SPL_TOKEN_PROGRAM_ID) => Ok(Some(MintPolicy {
                transfer_fee_bps: Some(0),
                permanent_delegate: false,
                transfer_hook: false,
            })),
            Some(TOKEN_2022_PROGRAM_ID) => {
                let extensions = account.pointer("/data/parsed/info/extensions").cloned().unwrap_or(Value::Null);
                let has_transfer_fee_config = extension_named(&extensions, "transferFeeConfig");
                let transfer_fee_bps = if has_transfer_fee_config {
                    max_transfer_fee_bps(&extensions)
                } else {
                    Some(0)
                };
                Ok(Some(MintPolicy {
                    transfer_fee_bps,
                    permanent_delegate: extension_named(&extensions, "permanentDelegate"),
                    transfer_hook: extension_named(&extensions, "transferHook"),
                }))
            }
            _ => Ok(None),
        }
    }
}

pub struct RugCheckSafetyChecker {
    http: reqwest::Client,
    rpc_url: String,
}

impl RugCheckSafetyChecker {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            rpc_url: std::env::var("SOLANA_RPC_URL").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string()),
        }
    }
}

impl Default for RugCheckSafetyChecker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TokenSafetyChecker for RugCheckSafetyChecker {
    async fn assess(&self, symbol: &Symbol) -> Result<Option<SafetyReport>, PortError> {
        let url = format!("{REPORT_URL}/{}/report", symbol.as_str());

        let response = self
            .http
            .get(&url)
            .header("Content-Type", "application/json")
            .send()
            .await
            .map_err(|e| PortError::Network {
                venue: "rugcheck".to_string(),
                source: Box::new(e),
            })?;

        if !response.status().is_success() {
            // A brand-new token may not be indexed by RugCheck yet -
            // treat any non-success as "not enough information", not a
            // hard failure.
            return Ok(None);
        }

        let report: RugCheckReport = response.json().await.map_err(|e| PortError::MalformedResponse {
            venue: "rugcheck".to_string(),
            reason: e.to_string(),
        })?;

        if report.rugged {
            return Ok(Some(SafetyReport {
                sell_tax_bps: None,
                token_transfer_fee_bps: None,
                sellability: ben_snipes_domain::SellabilityEvidence::Unknown,
                has_permanent_delegate: false,
                ownership_renounced: false,
                liquidity_locked: false,
                is_mintable: true,
            }));
        }

        let Some(token) = report.token else {
            // The whole `token` sub-object is missing - a much stronger
            // signal something is wrong with the assumed response shape
            // than any individual field being absent. Treat as "not
            // enough information" rather than guessing.
            return Ok(None);
        };

        // serde maps both an absent field and an explicit JSON `null`
        // to `None` for an `Option<Value>` field, so `is_some()` alone
        // correctly distinguishes "authority present" (any non-null
        // value, typically a pubkey string) from "renounced/absent" -
        // assuming the field names themselves are right. See the
        // module doc comment for the residual risk if they're not.
        let is_mintable = token.mint_authority.is_some();
        let freeze_authority_present = token.freeze_authority.is_some();
        let ownership_renounced = !is_mintable && !freeze_authority_present;

        // Best-effort: non-empty lockers array/object is treated as
        // "some liquidity locking exists". See module docs - this is
        // the least-confident field mapping here.
        let liquidity_locked = match &report.lockers {
            Some(Value::Array(arr)) => !arr.is_empty(),
            Some(Value::Object(obj)) => !obj.is_empty(),
            _ => false,
        };

        let Some(mint_policy) = self.inspect_mint_policy(symbol.as_str()).await? else {
            return Ok(None);
        };

        Ok(Some(SafetyReport {
            // RugCheck does not establish a real sell path or a DEX sell tax.
            // Keep those independent and fail closed rather than treating a
            // token-level transfer fee as proof that a sell will work.
            sell_tax_bps: None,
            token_transfer_fee_bps: mint_policy.transfer_fee_bps,
            sellability: if mint_policy.permanent_delegate || mint_policy.transfer_hook {
                ben_snipes_domain::SellabilityEvidence::Failed
            } else if !freeze_authority_present {
                ben_snipes_domain::SellabilityEvidence::Structural
            } else {
                ben_snipes_domain::SellabilityEvidence::Failed
            },
            has_permanent_delegate: mint_policy.permanent_delegate,
            ownership_renounced,
            liquidity_locked,
            is_mintable,
        }))
    }
}
