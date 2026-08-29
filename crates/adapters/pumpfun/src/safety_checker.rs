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
//! 0, which means unverified, not confirmed-safe.** If accurate sell-tax
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

pub struct RugCheckSafetyChecker {
    http: reqwest::Client,
}

impl RugCheckSafetyChecker {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
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
                sell_tax_bps: 0,
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

        Ok(Some(SafetyReport {
            // Always 0 - see module docs. This is "unverified", not
            // "confirmed zero tax".
            sell_tax_bps: 0,
            ownership_renounced,
            liquidity_locked,
            is_mintable,
        }))
    }
}
