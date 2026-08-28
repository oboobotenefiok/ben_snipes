use crate::DomainError;
use serde::{Deserialize, Serialize};
use std::fmt;

/// The blockchain a DEX-listed token actually lives on - "solana",
/// "ethereum", "base", etc. This is deliberately a separate concept
/// from `Venue`: `Venue` identifies *which source/DEX* reported a
/// listing (e.g. "pumpfun", "uniswap-v2-ethereum"), while `Chain`
/// identifies *where the token itself exists on-chain*.
///
/// That split is what makes cross-source deduplication possible: two
/// different sources watching the same chain (say, our own on-chain
/// watcher and a third-party indexer, both watching Ethereum) can
/// report the same token through two different `Venue`s, but they'll
/// always agree on `Chain` - so `CanonicalTokenId` keys on chain +
/// address, not on venue.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Chain(String);

impl Chain {
    pub fn new(raw: impl Into<String>) -> Result<Self, DomainError> {
        let raw = raw.into();
        if raw.trim().is_empty() {
            return Err(DomainError::EmptyChain);
        }
        Ok(Self(raw.to_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Chain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_chain() {
        assert_eq!(Chain::new("  "), Err(DomainError::EmptyChain));
    }

    #[test]
    fn normalises_to_lowercase() {
        let chain = Chain::new("Solana").expect("literal chain is valid");
        assert_eq!(chain.as_str(), "solana");
    }
}
