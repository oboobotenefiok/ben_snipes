use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Low-overhead process metrics shared by the runner and its local
/// Prometheus-compatible HTTP exporter. Counters are monotonic for the
/// lifetime of the process; `open_positions` is a gauge.
#[derive(Debug, Default)]
pub struct RuntimeMetrics {
    polls: AtomicU64,
    poll_errors: AtomicU64,
    listings_detected: AtomicU64,
    pending_decisions: AtomicU64,
    rejected_decisions: AtomicU64,
    positions_opened: AtomicU64,
    buy_errors: AtomicU64,
    exit_checks: AtomicU64,
    exits_filled: AtomicU64,
    exit_errors: AtomicU64,
    journal_errors: AtomicU64,
    journal_recoveries: AtomicU64,
    open_positions: AtomicU64,
}

impl RuntimeMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn inc_polls(&self) { self.polls.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_poll_errors(&self) { self.poll_errors.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_listings_detected(&self, count: usize) { self.listings_detected.fetch_add(count as u64, Ordering::Relaxed); }
    pub fn inc_pending_decisions(&self) { self.pending_decisions.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_rejected_decisions(&self) { self.rejected_decisions.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_positions_opened(&self) { self.positions_opened.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_buy_errors(&self) { self.buy_errors.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_exit_checks(&self) { self.exit_checks.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_exits_filled(&self) { self.exits_filled.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_exit_errors(&self) { self.exit_errors.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_journal_errors(&self) { self.journal_errors.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_journal_recoveries(&self) { self.journal_recoveries.fetch_add(1, Ordering::Relaxed); }
    pub fn set_open_positions(&self, count: usize) { self.open_positions.store(count as u64, Ordering::Relaxed); }

    pub fn render_prometheus(&self) -> String {
        format!(
            "# TYPE ben_snipes_polls_total counter\nben_snipes_polls_total {}\n\
# TYPE ben_snipes_poll_errors_total counter\nben_snipes_poll_errors_total {}\n\
# TYPE ben_snipes_listings_detected_total counter\nben_snipes_listings_detected_total {}\n\
# TYPE ben_snipes_pending_decisions_total counter\nben_snipes_pending_decisions_total {}\n\
# TYPE ben_snipes_rejected_decisions_total counter\nben_snipes_rejected_decisions_total {}\n\
# TYPE ben_snipes_positions_opened_total counter\nben_snipes_positions_opened_total {}\n\
# TYPE ben_snipes_buy_errors_total counter\nben_snipes_buy_errors_total {}\n\
# TYPE ben_snipes_exit_checks_total counter\nben_snipes_exit_checks_total {}\n\
# TYPE ben_snipes_exits_filled_total counter\nben_snipes_exits_filled_total {}\n\
# TYPE ben_snipes_exit_errors_total counter\nben_snipes_exit_errors_total {}\n\
# TYPE ben_snipes_journal_errors_total counter\nben_snipes_journal_errors_total {}\n# TYPE ben_snipes_journal_recoveries_total counter\nben_snipes_journal_recoveries_total {}\n\
# TYPE ben_snipes_open_positions gauge\nben_snipes_open_positions {}\n",
            self.polls.load(Ordering::Relaxed),
            self.poll_errors.load(Ordering::Relaxed),
            self.listings_detected.load(Ordering::Relaxed),
            self.pending_decisions.load(Ordering::Relaxed),
            self.rejected_decisions.load(Ordering::Relaxed),
            self.positions_opened.load(Ordering::Relaxed),
            self.buy_errors.load(Ordering::Relaxed),
            self.exit_checks.load(Ordering::Relaxed),
            self.exits_filled.load(Ordering::Relaxed),
            self.exit_errors.load(Ordering::Relaxed),
            self.journal_errors.load(Ordering::Relaxed),
            self.journal_recoveries.load(Ordering::Relaxed),
            self.open_positions.load(Ordering::Relaxed),
        )
    }
}
