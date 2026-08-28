use serde::{Deserialize, Serialize};

/// On-chain safety signals for a token, gathered before buying a DEX
/// listing. This is scoped to DEX-style acquisitions on purpose: a CEX
/// listing has already been through the exchange's own vetting (it
/// can't be an unsellable honeypot contract, because the exchange
/// controls the order book, not a smart contract the token author
/// wrote), so `AcquisitionEngine` only applies this gate when a
/// `SafetyGate` is actually configured for a venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyReport {
    /// Sell tax in basis points (100 = 1%). A high, unverifiable, or
    /// "can't even simulate a sell" tax is the single strongest honeypot
    /// signal - it's usually the actual mechanism a honeypot contract
    /// uses to trap buyers.
    pub sell_tax_bps: u32,
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
}

impl SafetyCriteria {
    pub fn new(max_sell_tax_bps: u32) -> Self {
        Self { max_sell_tax_bps }
    }

    pub fn passes(&self, report: &SafetyReport) -> bool {
        if report.sell_tax_bps > self.max_sell_tax_bps {
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
            sell_tax_bps: 200,
            ownership_renounced: true,
            liquidity_locked: true,
            is_mintable: false,
        }
    }

    #[test]
    fn accepts_a_clean_report() {
        let criteria = SafetyCriteria::new(1_000);
        assert!(criteria.passes(&safe_report()));
    }

    #[test]
    fn rejects_sell_tax_above_threshold() {
        let criteria = SafetyCriteria::new(500);
        let report = SafetyReport {
            sell_tax_bps: 900,
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn rejects_mintable_supply_regardless_of_other_signals() {
        let criteria = SafetyCriteria::new(1_000);
        let report = SafetyReport {
            is_mintable: true,
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn rejects_when_neither_renounced_nor_locked() {
        let criteria = SafetyCriteria::new(1_000);
        let report = SafetyReport {
            ownership_renounced: false,
            liquidity_locked: false,
            ..safe_report()
        };
        assert!(!criteria.passes(&report));
    }

    #[test]
    fn accepts_when_only_liquidity_is_locked() {
        let criteria = SafetyCriteria::new(1_000);
        let report = SafetyReport {
            ownership_renounced: false,
            liquidity_locked: true,
            ..safe_report()
        };
        assert!(criteria.passes(&report));
    }
}
