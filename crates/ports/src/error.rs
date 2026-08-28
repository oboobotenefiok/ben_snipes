use thiserror::Error;

/// Errors that cross the hexagon boundary: network failures, malformed
/// responses, disk errors. Kept separate from `ben_snipes_domain::DomainError`
/// on purpose, since "the exchange API timed out" and "you asked for a
/// negative take-profit" are different categories of problem and callers
/// often want to handle them differently (retry one, reject the other).
#[derive(Debug, Error)]
pub enum PortError {
    #[error("network request to {venue} failed: {source}")]
    Network {
        venue: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("failed to parse response from {venue}: {reason}")]
    MalformedResponse { venue: String, reason: String },

    #[error("state store I/O failed: {0}")]
    Storage(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("domain rule violated while translating adapter data: {0}")]
    Domain(#[from] ben_snipes_domain::DomainError),

    #[error("venue rejected the request: {0}")]
    Rejected(String),
}
