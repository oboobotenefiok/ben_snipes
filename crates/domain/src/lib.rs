//! `ben_snipes-domain` holds the core business types for the bot: what a
//! listing is, what a venue is, what a position is, and the rules that
//! govern them (like "a take-profit percentage must be positive").
//!
//! Nothing in this crate talks to a network, a filesystem, or a clock.
//! That's on purpose - it's the "hexagon" in hexagonal architecture, and
//! keeping it pure means we can unit test all our business rules without
//! spinning up mock servers or touching disk.

mod acquisition;
mod canonical;
mod chain;
mod error;
mod listing;
mod order;
mod position;
mod safety;
mod venue;

pub use acquisition::{AcquisitionCriteria, ListingMetrics};
pub use canonical::CanonicalTokenId;
pub use chain::Chain;
pub use error::DomainError;
pub use listing::{Listing, Symbol};
pub use order::{FilledBuy, Order, OrderSide, OrderStatus};
pub use position::{Position, ProfitTarget};
pub use safety::{SafetyCriteria, SafetyReport};
pub use venue::{Venue, VenueKind};
