//! `ben_snipes-application` is where the actual use cases live: services
//! that pull together one or more ports and apply domain rules to them.
//!
//! Nothing here knows what a "MEXC" or a "Raydium" is - it only knows
//! about `dyn ListingSource`, `dyn ListingStateStore`, and so on. That's
//! what lets the same `NewListingDetector` work identically whether it's
//! wired to a real exchange or a mock one in a test.

mod acquisition_engine;
mod backtest;
mod new_listing_detector;
mod position_manager;
mod paper_exchange;
mod runtime_metrics;

pub use acquisition_engine::{AcquisitionDecision, AcquisitionEngine, SafetyGate};
pub use backtest::{BacktestConfig, BacktestEngine, BacktestEvent, BacktestReport, BacktestTrade};
pub use new_listing_detector::NewListingDetector;
pub use position_manager::{ExitResult, PositionManager};
pub use paper_exchange::PaperExchange;
pub use runtime_metrics::RuntimeMetrics;
