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
    price_cache_hits: AtomicU64,
    price_cache_misses: AtomicU64,
    price_batches: AtomicU64,
    price_batch_items: AtomicU64,
    price_latency_ms_total: AtomicU64,
    price_latency_samples: AtomicU64,
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
    pub fn inc_exit_checks_by(&self, count: usize) { self.exit_checks.fetch_add(count as u64, Ordering::Relaxed); }
    pub fn inc_exit_errors_by(&self, count: usize) { self.exit_errors.fetch_add(count as u64, Ordering::Relaxed); }
    pub fn set_price_cache_stats(&self, hits: u64, misses: u64, batches: u64, batch_items: u64, latency_ms_total: u64, latency_samples: u64) {
        self.price_cache_hits.store(hits, Ordering::Relaxed);
        self.price_cache_misses.store(misses, Ordering::Relaxed);
        self.price_batches.store(batches, Ordering::Relaxed);
        self.price_batch_items.store(batch_items, Ordering::Relaxed);
        self.price_latency_ms_total.store(latency_ms_total, Ordering::Relaxed);
        self.price_latency_samples.store(latency_samples, Ordering::Relaxed);
    }

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
# TYPE ben_snipes_open_positions gauge\nben_snipes_open_positions {}\n# TYPE ben_snipes_price_cache_hits_total counter\nben_snipes_price_cache_hits_total {}\n# TYPE ben_snipes_price_cache_misses_total counter\nben_snipes_price_cache_misses_total {}\n# TYPE ben_snipes_price_batches_total counter\nben_snipes_price_batches_total {}\n# TYPE ben_snipes_price_batch_items_total counter\nben_snipes_price_batch_items_total {}\n# TYPE ben_snipes_price_cache_hit_ratio gauge\nben_snipes_price_cache_hit_ratio {}\n# TYPE ben_snipes_price_average_batch_size gauge\nben_snipes_price_average_batch_size {}\n# TYPE ben_snipes_jupiter_average_latency_ms gauge\nben_snipes_jupiter_average_latency_ms {}\n",
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
            self.price_cache_hits.load(Ordering::Relaxed),
            self.price_cache_misses.load(Ordering::Relaxed),
            self.price_batches.load(Ordering::Relaxed),
            self.price_batch_items.load(Ordering::Relaxed),
            {
                let hits = self.price_cache_hits.load(Ordering::Relaxed);
                let misses = self.price_cache_misses.load(Ordering::Relaxed);
                if hits + misses == 0 { 0.0 } else { hits as f64 / (hits + misses) as f64 }
            },
            {
                let batches = self.price_batches.load(Ordering::Relaxed);
                if batches == 0 { 0.0 } else { self.price_batch_items.load(Ordering::Relaxed) as f64 / batches as f64 }
            },
            {
                let samples = self.price_latency_samples.load(Ordering::Relaxed);
                if samples == 0 { 0.0 } else { self.price_latency_ms_total.load(Ordering::Relaxed) as f64 / samples as f64 }
            },
        )
    }
}
