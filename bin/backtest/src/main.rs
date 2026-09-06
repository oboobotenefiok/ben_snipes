use ben_snipes_application::{BacktestConfig, BacktestEngine, BacktestEvent};
use ben_snipes_domain::PerformanceSummary;
use serde::Deserialize;
use std::{env, fs};

#[derive(Debug, Deserialize)]
struct BacktestFile {
    config: BacktestConfig,
    events: Vec<BacktestEvent>,
}

fn print_summary(summary: &PerformanceSummary) {
    println!("trades: {}", summary.trade_count);
    println!("wins: {}", summary.winning_trades);
    println!("losses: {}", summary.losing_trades);
    println!("flats: {}", summary.flat_trades);
    println!("quote cost: {}", summary.total_quote_cost);
    println!("quote proceeds: {}", summary.total_quote_proceeds);
    println!("realized pnl: {}", summary.realized_pnl);
}

fn main() {
    let path = match env::args().nth(1) {
        Some(path) => path,
        None => {
            eprintln!("usage: ben_snipes-backtest <dataset.json>");
            std::process::exit(2);
        }
    };

    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) => {
            eprintln!("failed to read '{path}': {e}");
            std::process::exit(1);
        }
    };

    let dataset: BacktestFile = match serde_json::from_str(&raw) {
        Ok(dataset) => dataset,
        Err(e) => {
            eprintln!("invalid backtest dataset '{path}': {e}");
            std::process::exit(1);
        }
    };

    let engine = match BacktestEngine::new(dataset.config) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("invalid backtest config: {e}");
            std::process::exit(1);
        }
    };

    let report = engine.run(dataset.events);
    println!("events processed: {}", report.events_processed);
    println!("listings seen: {}", report.listings_seen);
    println!("entries opened: {}", report.entries_opened);
    println!("pending events: {}", report.pending_events);
    println!("rejected events: {}", report.rejected_events);
    println!("still open: {}", report.still_open.len());
    print_summary(&report.performance);
}
