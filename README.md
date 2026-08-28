# ben_snipes

A listing-sniper bot: watches CEX and DEX venues for newly tradable
symbols, buys them, and exits at a configurable take-profit target
(default +10%).

**Status: architectural scaffold, not yet safe to run against real
funds.** See "Not yet implemented" below before doing anything except
building and running the mock demo.

## Architecture

Hexagonal (ports and adapters), split across a Cargo workspace so each
concern can be built, tested, and scaled independently:

```
crates/domain/       pure business types and rules (Listing, Position,
                      ProfitTarget, StopLoss, AcquisitionCriteria,
                      SafetyCriteria). No I/O, no async runtime.
crates/ports/         traits the application depends on: ListingSource,
                      ListingStateStore, ExchangeClient, MetricsProvider,
                      TokenSafetyChecker, Clock.
crates/application/   use cases: NewListingDetector, AcquisitionEngine,
                      PositionManager. Depends only on domain + ports,
                      never on a concrete adapter.
crates/config/        typed config loading (TOML + env overrides).
crates/adapters/
  statefile/          ListingStateStore backed by one JSON file per
                      source, atomic writes via temp-file + rename.
  cex-mock/           fake CEX: ListingSource + ExchangeClient +
                      MetricsProvider, exercises the full-snapshot-diff
                      path.
  dex-mock/           fake DEX: adds TokenSafetyChecker on top, exercises
                      the cursor/incremental path.
bin/runner/           composition root - the only crate that wires
                      concrete adapters into the application. Builds
                      to the `ben_snipes` binary.
```

The dependency rule: `domain` depends on nothing in this workspace.
`ports` depends on `domain`. `application` depends on `domain` and
`ports`. Adapters depend on `domain` and `ports`. Only `runner`
depends on everything. This is what makes it possible to add a real
MEXC or Raydium adapter later without touching `application` at all.

### New-listing detection strategy

`NewListingDetector` (in `crates/application`) supports both approaches
discussed:

- **Cursor-based incremental fetch** - if a venue can answer "what's new
  since cursor X" (a `since` timestamp, a block number, a sequence ID),
  its adapter returns `ListingSnapshot::Incremental` and the detector
  just tracks the cursor. No diffing needed. `dex-mock` demonstrates
  this using a simulated block number as the cursor.
- **Full-snapshot diff** - if a venue only exposes "here's everything
  tradable right now," its adapter returns `ListingSnapshot::Full` and
  the detector diffs it against the set of dedupe keys persisted by a
  `ListingStateStore` (the `statefile` adapter, by default). `cex-mock`
  demonstrates this path.

Same output either way: a `Vec<Listing>` of things not seen before.
Which strategy a venue uses is invisible outside its own adapter.

#### Cold-start rule (important for any real adapter)

The very first poll of a source must never surface its entire existing
symbol universe as "new" - a bot that's just started up hasn't watched
anything long enough to know what's actually new versus what was simply
already listed before it started. `NewListingDetector` already handles
this for the `Full`-snapshot path (the first poll of any source
establishes a baseline and reports nothing). It does **not** handle it
for the `Incremental` path, because that's an adapter contract, not a
detector concern: an incremental adapter that receives `cursor: None`
must default to "now" (the current block/timestamp/sequence number),
never "the beginning of time" - `dex-mock` illustrates the mechanism but
deliberately starts its simulated block counter at zero for demo
visibility, which a real chain adapter must not do.

### Autonomous acquisition and exit

`AcquisitionEngine` (in `crates/application`) is what makes buying
autonomous rather than just alert-only. For each newly-detected listing:

1. Fetch its volume (and market cap, for context) via a
   `MetricsProvider` (returns `None` if data isn't available yet - the
   engine skips rather than guesses).
2. Check it against `AcquisitionCriteria` (`risk.min_volume_24h` in
   config) - active 24h volume is the sole gate. Market cap is *not*
   used to disqualify a listing either way: a high-cap listing with real
   volume is just as tradeable as a low-cap one, so `ListingMetrics`
   still carries `market_cap` for logging/sizing context, but nothing
   currently filters on it.
3. If it passes, size the position from `risk.max_position_size`, submit
   a market buy, and hand back an open `Position`.

`PositionManager` then watches each open position every poll tick and
submits the exit order once price crosses either threshold
(`risk.take_profit_percent` or `risk.stop_loss_percent`). No human is in
either loop.

### Stop-loss

Every position now carries both a `ProfitTarget` (ceiling) and a
`StopLoss` (floor) - `Position::exit_reason` checks the ceiling first,
then the floor, and returns which one fired so logs and (eventually) P&L
reporting can tell a win from a loss rather than just "position closed".
Configured via `risk.stop_loss_percent`; default is 5%.

### Honeypot / rug detection

DEX listings get an extra gate before `AcquisitionEngine` will buy:
`TokenSafetyChecker` supplies a `SafetyReport` (sell tax, whether
ownership is renounced, whether liquidity is locked, whether the
contract is still mintable), and `SafetyCriteria` rejects anything with
too high a sell tax, still-mintable supply, or neither renounced
ownership nor locked liquidity. This is wired in via an optional
`SafetyGate` on `AcquisitionEngine` - `dex-mock` has one, `cex-mock`
deliberately doesn't (a CEX listing can't be an unsellable honeypot
contract; the exchange's order book controls execution, not a token
contract). Tunable via `safety.max_sell_tax_bps` in config.

**This is still a real limitation to know about:** the mock's safety
reports are hand-set values (`set_safety_report` in `dex-mock`), not a
real assessment. A real adapter needs to actually simulate a sell
against the token contract and read its on-chain state - see "Not yet
implemented" below.

## Building and running

This environment could not install a Rust toolchain or reach
crates.io to compile this scaffold (no network access), so **none of
this has been built or tested yet** - it's been written to be correct
and idiomatic, but you need to verify it on a machine with network
access before trusting it:

```bash
# from the workspace root
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run --bin ben_snipes
```

`cargo run` starts the poll loop against the two mock adapters, logs
one simulated new DEX listing on the first tick, and polls every
`risk.poll_interval_seconds` (see `config/default.toml`) until you hit
Ctrl+C.

## Not yet implemented

This is a scaffold, not a trading system. Before this touches real
funds it needs, at minimum:

- **Real exchange/DEX adapters.** The mocks are reference
  implementations only. A real CEX adapter needs authenticated
  REST/WebSocket calls, HMAC request signing, and rate-limit handling.
  A real DEX adapter needs an RPC connection, transaction signing, and
  ideally a private relay (Flashbots on Ethereum, Jito on Solana) so
  your entry can't be sandwiched.
- **Real honeypot/rug data.** The logic exists (`SafetyCriteria`,
  `TokenSafetyChecker`) but `dex-mock`'s reports are hand-set, not
  derived from an actual sell simulation or contract read. A real
  `TokenSafetyChecker` needs to simulate a sell through the DEX router
  and inspect the token contract's ownership/mint functions on-chain -
  a dedicated token-scanner API is the usual shortcut.
- **Wallet/key security.** No key management is implemented. A real
  deployment needs encrypted secrets at minimum, ideally an HSM or
  equivalent, and a hot wallet capped to what you can afford to lose.
- **Circuit breaker / kill switch.** `max_position_size` is enforced
  per-position by `AcquisitionEngine`, but there's still no global cap on
  concurrent open positions or automatic pause on abnormal conditions
  (repeated failed exits, API errors spiking, unusually many listings in
  a short window - the last of which often signals a compromised or
  spoofed feed rather than a real listing wave).
- **Persisted open positions.** `open_positions` currently lives only in
  the runner's in-memory `Vec` - a crash or restart forgets any position
  that hasn't hit its take-profit or stop-loss yet, with no
  on-chain/on-exchange reconciliation to recover it.
- **Incremental-adapter cold start.** See the cold-start rule above -
  real `Incremental`-style adapters need to default an absent cursor to
  "now," which is on the adapter author, not enforced by the framework.

## License

MIT - see `LICENSE`.
