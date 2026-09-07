use serde::{Deserialize, Serialize};

/// Evidence that a token can be transferred out of the holder account.
/// `Structural` is intentionally weaker than a live DEX sell simulation: it
/// means the token program state does not expose the known transfer traps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SellabilityEvidence {
    /// A live buy/sell or equivalent execution simulation succeeded.
    Simulated,
    /// On-chain token-program state was inspected and no known transfer trap
    /// was found. This does not guarantee DEX liquidity or route execution.
    Structural,
    /// The checker did not have enough evidence.
    Unknown,
    /// The checker found a definitive transfer/sell failure.
    Failed,
}

/// On-chain safety signals for a token, gathered before buying a DEX
/// listing. This is scoped to DEX-style acquisitions on purpose: a CEX
/// listing has already been through the exchange's own vetting (it
/// can't be an unsellable honeypot contract, because the exchange
/// controls the order book, not a smart contract the token author
/// wrote), so `AcquisitionEngine` only applies this gate when a
/// `SafetyGate` is actually configured for a venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyReport {
    /// Sell tax in basis points (100 = 1%). `None` means the checker could
    /// not verify a sell tax. Unknown must remain distinct from a measured
    /// zero so the safety gate cannot accidentally fail open.
    pub sell_tax_bps: Option<u32>,
    /// Token-level transfer fee, distinct from a DEX/router sell tax.
    /// `None` means the mint policy could not be inspected.
    #[serde(default)]
    pub token_transfer_fee_bps: Option<u32>,
    /// Evidence that the token can be transferred/sold. This is deliberately
    /// independent from `sell_tax_bps`, because a tax quote is not proof that
    /// a sell path works.
    #[serde(default = "default_sellability_evidence")]
    pub sellability: SellabilityEvidence,
    /// Whether a Token-2022 permanent delegate can seize or otherwise
    /// control token accounts. This is a hard safety failure.
    #[serde(default)]
    pub has_permanent_delegate: bool,
    /// Whether contract ownership has been renounced (no admin function
    /// left that could rug the token after purchase).
    pub ownership_renounced: bool,
    /// Whether the liquidity pool backing this token is time-locked
    /// (the classic "dev pulls liquidity" rug becomes much harder).
    pub liquidity_locked: bool,
    /// Whether the contract retains a mint function that could inflate
    /// supply, and therefore dump price, after purchase.
    pub is_mintable: bool,
}

/// The rule that decides whether a `SafetyReport` clears the bar to buy.
///
/// Deliberately conservative by default: a listing needs an acceptable
/// sell tax, must not be freely mintable, and must show at least one of
/// "ownership renounced" or "liquidity locked" - neither one alone is a
/// guarantee, but the complete absence of both is one of the most
/// reliable rug signals there is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyCriteria {
    max_sell_tax_bps: u32,
    max_token_transfer_fee_bps: u32,
}

fn default_sellability_evidence() -> SellabilityEvidence {
    SellabilityEvidence::Unknown
}

impl SafetyCriteria {
    pub fn new(max_sell_tax_bps: u32, max_token_transfer_fee_bps: u32) -> Self {
        Self {
            max_sell_tax_bps,
            max_token_transfer_fee_bps,
        }
    }

    pub fn passes(&self, report: &SafetyReport) -> bool {
        let Some(sell_tax_bps) = report.sell_tax_bps else {
            return false;
        };
        if sell_tax_bps > self.max_sell_tax_bps {
            return false;
        }
        let Some(token_transfer_fee_bps) = report.token_transfer_fee_bps else {
            return false;
        };
        if token_transfer_fee_bps > self.max_token_transfer_fee_bps {
            return false;
        }
        if !matches!(report.sellability, SellabilityEvidence::Simulated | SellabilityEvidence::Structural)
            || report.has_permanent_delegate
        {
            return false;
        }
        if report.is_mintable {
            return false;
        }
        if !(report.ownership_renounced || report.liquidity_locked) {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn safe_report() -> SafetyReport {
        SafetyReport {
            sell_tax_bps: Some(200),
            token_transfer_fee_bps: Some(0),
            sellability: SellabilityEvidence::Simulated,
            has_permanent_delegate: false,
            ownership_renounced: true,
            liquidity_locked: true,
            is_mintable: false,
        }
    }

    #[test]
    fn accepts_a_clean_report() {
        let criteria = SafetyCriteria::new(1_000, 0);
        assert!(criteria.passes(&safe_report()));
    }

    #[test]
    fn rejects_sell_tax_above_threshold() {
        let criteria = SafetyCriteria::new(500, 0);
        let report = SafetyReport {
            sell_tax_bps: Some(900),
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn rejects_unknown_sell_tax() {
        let criteria = SafetyCriteria::new(1_000, 0);
        let report = SafetyReport {
            sell_tax_bps: None,
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn rejects_unknown_token_transfer_fee() {
        let criteria = SafetyCriteria::new(1_000, 0);
        let report = SafetyReport {
            token_transfer_fee_bps: None,
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn rejects_unverified_sellability() {
        let criteria = SafetyCriteria::new(1_000, 0);
        let report = SafetyReport {
            sellability: SellabilityEvidence::Unknown,
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn rejects_permanent_delegate() {
        let criteria = SafetyCriteria::new(1_000, 0);
        let report = SafetyReport {
            has_permanent_delegate: true,
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn rejects_mintable_supply_regardless_of_other_signals() {
        let criteria = SafetyCriteria::new(1_000, 0);
        let report = SafetyReport {
            is_mintable: true,
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn rejects_when_neither_renounced_nor_locked() {
        let criteria = SafetyCriteria::new(1_000, 0);
        let report = SafetyReport {
            ownership_renounced: false,
            liquidity_locked: false,
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn accepts_when_only_liquidity_is_locked() {
        let criteria = SafetyCriteria::new(1_000, 0);
        let report = SafetyReport {
            ownership_renounced: false,
            liquidity_locked: true,
            ..safe_report()
        };
        assert!(criteria.passes(&report));
    }
}
