use crate::DomainError;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Whether a venue is a centralised exchange (an API you authenticate
/// against) or a decentralised one (a chain you read/write on-chain state
/// against). Kept as a simple two-way split at the domain level; the
/// specifics of "which chain" or "which exchange" live in the venue name
/// and get resolved to a concrete adapter at the composition root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VenueKind {
    Cex,
    Dex,
}

impl fmt::Display for VenueKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VenueKind::Cex => write!(f, "cex"),
            VenueKind::Dex => write!(f, "dex"),
        }
    }
}

/// A trading venue, e.g. `Venue::new(VenueKind::Cex, "mexc")` or
/// `Venue::new(VenueKind::Dex, "raydium")`.
///
/// This is deliberately just a tag, not a live connection. The domain
/// layer doesn't know how to talk to MEXC or Raydium; it only needs to
/// know that a `Listing` came from one of them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Venue {
    kind: VenueKind,
    name: String,
}

impl Venue {
    pub fn new(kind: VenueKind, name: impl Into<String>) -> Result<Self, DomainError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(DomainError::EmptyVenueName);
        }
        Ok(Self {
            kind,
            name: name.to_lowercase(),
        })
    }

    pub fn kind(&self) -> VenueKind {
        self.kind
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for Venue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_name() {
        assert_eq!(
            Venue::new(VenueKind::Cex, "  "),
            Err(DomainError::EmptyVenueName)
        );
    }

    #[test]
    fn normalises_name_to_lowercase() {
        let venue = Venue::new(VenueKind::Dex, "Raydium").expect("valid name is fine here");
        assert_eq!(venue.name(), "raydium");
    }
}
