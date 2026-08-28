use thiserror::Error;

/// Errors that come from violating a business rule, as opposed to errors
/// from I/O (those live in `ben_snipes-ports`, next to the traits that can
/// fail in I/O-flavoured ways).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DomainError {
    #[error("take-profit percentage must be positive, got {0}")]
    InvalidProfitTarget(String),

    #[error("stop-loss percentage must be positive, got {0}")]
    InvalidStopLoss(String),

    #[error("symbol cannot be empty")]
    EmptySymbol,

    #[error("venue name cannot be empty")]
    EmptyVenueName,

    #[error("chain identifier cannot be empty")]
    EmptyChain,

    #[error("order quantity must be positive, got {0}")]
    InvalidQuantity(String),

    #[error("min volume must not be negative, got {0}")]
    InvalidMinVolume(String),
}
