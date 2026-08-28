//! `ben_snipes-ports` defines the boundary of the hexagon: traits that
//! describe what the application needs from the outside world (a
//! listings feed, a place to persist state, an exchange to trade on, a
//! clock) without saying anything about how those needs get met.
//!
//! Concrete implementations live in `ben_snipes-adapter-*` crates and get
//! wired in at the composition root (the `runner` binary). This is what
//! makes it possible to swap a mock CEX adapter for a real MEXC adapter
//! without touching a single line of application logic.

mod acquisition_ledger;
mod clock;
mod error;
mod exchange_client;
mod listing_source;
mod metrics_provider;
mod state_store;
mod token_safety_checker;

pub use acquisition_ledger::AcquisitionLedger;
pub use clock::Clock;
pub use error::PortError;
pub use exchange_client::ExchangeClient;
pub use listing_source::{ListingSnapshot, ListingSource};
pub use metrics_provider::MetricsProvider;
pub use state_store::{KnownListings, ListingStateStore};
pub use token_safety_checker::TokenSafetyChecker;
