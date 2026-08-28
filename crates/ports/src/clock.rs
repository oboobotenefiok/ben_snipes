use time::OffsetDateTime;

/// Abstracts "what time is it" so application logic that stamps a
/// `Listing::first_seen` can be unit tested with a fixed clock instead of
/// depending on wall-clock time.
pub trait Clock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}

/// The real clock, used everywhere except tests.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}
