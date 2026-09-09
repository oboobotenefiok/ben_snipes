# ben_snipes

An autonomous new-listing sniper: watches for tokens becoming newly
tradable with active volume, buys them, and holds until a configurable
take-profit target is hit - typically +10%. **There is no stop-loss, by
design**: a position is held until it hits target, however long that
takes; it never exits at a loss. **DEX-only.** CEX listings were
deliberately dropped - they're too rare and too slow relative to
on-chain launches to be worth the surface area for a bot whose whole
edge is being early. See "Automation & execution platforms" below for
how buys/sells are meant to actually get executed.

**Status: the Solana pipeline is fully wired end to end.** Real
detection, real volume filtering (DexScreener), a real cross-source
deduplication ledger, and - when `SOLANA_PRIVATE_KEY` is set - real
buy/sell execution. **This means it
can autonomously spend real funds.** See "Automation & execution
platforms" for exactly what's verified vs. best-effort in each piece,
and review the execution module documentation before funding a
wallet. EVM now has real Alloy-based execution, private-RPC submission,
requires an `EVM_PRIVATE_KEY` plus a configured private RPC.

## Architecture

Hexagonal (ports and adapters), split across a Cargo workspace:

```
crates/domain/       pure business types and rules: Listing, Chain,
                      CanonicalTokenId, Position, ProfitTarget,
crates/ports/         traits the application depends on: ListingSource,
                      ListingStateStore, AcquisitionLedger, PositionStore,
                      ExchangeClient, MetricsProvider,
crates/application/   use cases: NewListingDetector, AcquisitionEngine,
                      PositionManager. Depends only on domain + ports.
crates/config/        typed config loading (TOML + env overrides).
crates/adapters/
  statefile/          ListingStateStore (per-source JSON snapshots),
                      AcquisitionLedger (cross-source dedup), and
                      PositionStore (open-position recovery across
                      restarts) - all file-backed with atomic
                      temp-file+rename writes.
  ws-support/          shared reconnect-with-backoff helper for the two
                      websocket-backed real adapters below.
  pumpfun/             REAL Solana pipeline, six modules:
                      listing detection (PumpPortal websocket),
                      execution.rs (signing/broadcast) +
                      exchange_client.rs (live buy/sell with mandatory wallet),
                      metrics_provider.rs
                      price_feed.rs (Jupiter price, SOL-denominated),
                      retry.rs (shared retry-with-backoff for the
                      transient-failure-prone network calls above).
  evm-onchain/         REAL EVM ListingSource: subscribes directly to a
                      DEX factory's pair-creation logs over eth_subscribe.
                      Chain/factory/event-agnostic, configured per chain.
bin/runner/           composition root - the only crate that wires
                      concrete adapters into the application. Builds
                      to the `ben_snipes` binary.
```

## Why not DexScreener/GeckoTerminal for detection

Both are indexer/display APIs: rate-limited, and their new-pairs
endpoints cap out around 20-30 results per call with no real
pagination for a live firehose. That's fine for browsing, not for
catching every new token as it happens. `pumpfun` and `evm-onchain`
instead **subscribe directly to the event source** - a live websocket
feed (PumpPortal) or a raw `eth_subscribe` log subscription to a DEX
factory contract - so detection is push-based with no pagination
ceiling at all. Aggregator APIs are still useful, just for a different
job: enriching a listing with metrics once it exists (this is exactly
what `MetricsProvider` is for), not for discovering it in the first
place.

### Pending listings and delayed indexers

A listing can be detected on-chain before DexScreener has indexed it, and a
listing can also be indexed while its 24h volume is still below the acquisition
threshold. Those states are **pending**, not rejected.

Each listing source persists its pending candidates alongside its normal cursor
and seen-key state. The runner retries pending candidates every
`risk.pending_listing_retry_seconds` seconds. A pending candidate gets a full
24-hour retry window starting when it enters the pending queue. During that
window, a token that later becomes indexed or later reaches the required volume
can still be bought, subject to the volume threshold and all entry
guardrails.

Only a definitive rejection, a successful acquisition, or expiry of the
24-hour window removes a candidate from pending. The `seen_keys` set is kept
separate, so expiring a candidate prevents it from being rediscovered by a
full-snapshot source while still allowing the source's cursor to advance.

The state format also accepts the previous pending-listing representation on
load, using the listing's original `first_seen` timestamp as the migration
start time.

### Cross-source deduplication: `CanonicalTokenId` + `AcquisitionLedger`

If more than one source ever watches the same chain (e.g. `pumpfun`
plus a future Birdeye poller, both watching Solana), they could each
report the same underlying token through a different `Venue`. Buying it
twice would be a real bug, not a cosmetic one. Two pieces fix this:

- **`CanonicalTokenId`** (`crates/domain/src/canonical.rs`) - a token's
  true identity is its chain plus its lowercased contract/mint address,
  *not* which venue/source reported it. Two listings from different
  sources for the same token always produce the same canonical ID.
- **`AcquisitionLedger`** (port in `crates/ports`, file-backed
  implementation in `crates/adapters/statefile`) - `AcquisitionEngine`
  reserves a token's canonical ID immediately before buying. Whichever
  source gets there first wins the reservation; every other source's
  report of the same token becomes a silent no-op. If the buy itself
  then fails, the reservation is released so a later attempt can retry.

**Known limitation:** the file-backed ledger is atomic *within one
running process* (an in-process async mutex), not across multiple bot
instances sharing the same state directory. Running more than one
instance against the same ledger file needs a real concurrent store
(e.g. a database with a unique constraint) instead.

### Surviving a restart: `PositionStore`

Without this, a crash after a real buy doesn't just lose bookkeeping -
it orphans the position entirely. The `AcquisitionLedger` reservation
persists (correctly - the token really was bought, it shouldn't be
bought again), but the open-position list used to live only in the
runner's memory, so nothing would be left watching that position for
its take-profit target. Money spent, no exit mechanism, forgotten by
the bot that spent it.

`PositionStore` (port in `crates/ports`, file-backed implementation in
`crates/adapters/statefile`) persists the complete open-position list -
saved immediately after every buy, and again after every exit-check
pass - and `main.rs` loads it at startup instead of always starting
from an empty list. Same atomic temp-file+rename pattern. The runner's instance lock prevents
multiple local processes from concurrently mutating the same state directory.

### New-listing detection strategy

`NewListingDetector` supports two source shapes:

- **Cursor-based incremental** (`ListingSnapshot::Incremental`) - a
  push-based feed like `pumpfun`/`evm-onchain` just forwards whatever
  arrived since the last poll; no diffing needed.
- **Full-snapshot diff** (`ListingSnapshot::Full`) - a venue that only
  exposes "here's everything right now" gets diffed against a persisted
  set of dedupe keys (real adapters use the incremental path).

The very first poll of a `Full` source establishes a baseline and
reports nothing as new - without this, a bot's first poll of any
"list everything" endpoint would flag every pre-existing symbol as a
brand-new listing. This does **not** apply automatically to
`Incremental` sources - see the cold-start note below.

**Cold-start rule for real incremental adapters:** an adapter that
receives `cursor: None` must default to "now" (the current
block/timestamp), never "the beginning of time" - `pumpfun` and
`evm-onchain` are naturally safe here since they only forward events
that arrive *after* the websocket connects, with no historical replay.
A future adapter with true cursor persistence (resuming from a stored
block number after a restart) would need to apply this rule explicitly.

### Autonomous acquisition and exit

`AcquisitionEngine` per detected listing: check volume via
`MetricsProvider` -> check `AcquisitionCriteria` (`risk.min_volume_24h`,
the sole acquisition gate - market cap does not disqualify a listing) ->
in the ledger -> size from `risk.max_position_size` and buy.
`PositionManager` then watches every open position and exits once
`risk.take_profit_percent` is reached - and only then. There is no
stop-loss: a position that drops after entry is simply held, however
long it takes to recover to target, rather than sold at a loss. This is
a deliberate strategy choice ("10% or nothing"), not an oversight.

The bot is live-only. A Solana wallet is mandatory at startup, and configured EVM chains likewise require `EVM_PRIVATE_KEY` and a private execution RPC. The bot has no simulated or detection-only execution mode.

## Automation & execution platforms

**Solana - wired in and real.** `ben_snipes-adapter-pumpfun::execution`
implements real, non-custodial signing and broadcast against
PumpPortal's **Local Transaction API** (`/api/trade-local`): they build
an unsigned transaction, we sign it locally with a wallet loaded from
the `SOLANA_PRIVATE_KEY` environment variable (never a file, never
logged, no default), and broadcast it ourselves via raw JSON-RPC.
Chosen over their "Lightning" API specifically because Lightning is
custodial (they hold your key), which doesn't fit the self-custody
stance taken everywhere else in this project. `PumpPortalExchangeClient`
wraps this into a real `ExchangeClient`: buy confirms on-chain then
reads the resulting balance via `getTokenAccountsByOwner` to report
back actual quantity/entry price; sell offloads a known quantity the
same way. If `SOLANA_PRIVATE_KEY` is not set, startup fails clearly; the bot never enters a degraded execution mode.

**This is the highest-risk code in the whole project, and it says so in
its own doc comment.** `solana-sdk` went through a major breaking
restructuring recently (the Anza fork, v3 -> v4 - `Keypair::from_bytes`
became `Keypair::try_from`, `Pubkey` became a type alias for a new
`Address` type). The signing step is built on the lowest-level,
most-likely-to-remain-stable primitives available (raw bincode
deserialize/sign/reserialize, raw JSON-RPC instead of the
`solana-client` crate) specifically to minimize exposure to that churn,
but **read `execution.rs`'s module doc comment and verify the signing
block against docs.rs for the exact `solana-sdk` version pinned in
`Cargo.toml` before running this against real funds.** The
confirmation-polling and balance-reading RPC calls in
`exchange_client.rs` carry meaningfully less of that specific risk (JSON-RPC
method names are wire-protocol-stable, not a Rust crate's internal API
surface) but are equally unverified by an actual network call in this
environment - sanity-check the response shape on first real run.

**The port-shape mismatch flagged last round is fixed.**
`ExchangeClient` used to only offer a quantity-based `submit_order`,
which assumed you already know a price - PumpPortal's actual buy
interface is "spend this much SOL," with no price to pre-compute
without bonding-curve math. `submit_buy_by_amount` is now a first-class
port method: `AcquisitionEngine` spends `position_size` directly and
gets back a `FilledBuy { quantity, entry_price }` reporting what
actually happened, instead of pre-computing a quantity that never
matched what the venue needed. Selling stays quantity-based
(`submit_order`) since by the time you're exiting, the quantity is
already known - it's the position you're holding.

**Volume filtering and price monitoring are now real.**

**The port-shape mismatch flagged two rounds ago is fixed.**
`ExchangeClient` used to only offer a quantity-based `submit_order`,
which assumed you already know a price - PumpPortal's actual buy
interface is "spend this much SOL," with no price to pre-compute
without bonding-curve math. `submit_buy_by_amount` is now a first-class
port method: `AcquisitionEngine` spends `position_size` directly and
gets back a `FilledBuy { quantity, entry_price }` reporting what
actually happened. Selling stays quantity-based (`submit_order`) since
by the time you're exiting, the quantity is already known.

**Reliability hardening added since:** `PumpPortalExchangeClient` now
checks the wallet's SOL balance (`getBalance`, a foundational, stable
RPC method) before attempting a buy, so an obviously-insufficient
balance fails fast with a clear message instead of wasting a signed,
broadcast transaction attempt. The network calls throughout
`pumpfun` most worth retrying - the trade-local request, balance reads,
confirmation polling - go through `retry.rs`'s backoff helper or an
inline equivalent; every call site was checked for idempotency first
(see that module's doc comment for the reasoning, including why
resubmitting an identical signed transaction is safe on Solana
specifically, unlike most payment-style APIs).

**Entry-side operational guardrails are now also enforced in the runner.**
New buys pause when the operator creates `state/STOP_ENTRIES`, when the
configured concurrent-position cap is full, when one poll cycle produces
anomalously many new listings, or when consecutive operational failures
trip the circuit breaker. Existing positions continue through their normal
exit checks while new entries are paused.

**Consolidated risk summary, because this is the round where the bot
became capable of spending real funds:** (1) the signing code in
`execution.rs` is pinned to the current `solana-sdk` 4.1.0 shape and now
fails closed on an unknown sell-tax value; (3) `simulateTransaction`
preflight now runs before signing on both entry transactions and the exact
sell transaction immediately before an exit; (4) this environment still
has no Rust toolchain because Debian package index access is unavailable. Start
with the smallest `max_position_size` you're willing to lose entirely,
watch the logs (`RUST_LOG=debug`), and watch the wallet address on a
block explorer during the first several trades.

**EVM execution is now wired.** Alloy builds and locally signs
Uniswap-V2-compatible native-coin buys and token sells. Every write is
simulated before signing, and the signed EIP-2718 transaction is submitted
through the configured private RPC. The adapter verifies the connected chain
ID before execution, serializes wallet writes to prevent in-process nonce
collisions, and derives EVM sell proceeds from the wallet balance delta plus
verified root source, and a measured sell tax below the configured limit.

## Continuous integration

`.github/workflows/build.yml` runs on every push and pull request (plus
manual dispatch): installs Rust via apt (this project's standing
convention, rather than rustup or a toolchain action), then `cargo
build --workspace`, `cargo test --workspace`, and `cargo clippy
--workspace --all-targets -- -D warnings`. Caches the cargo registry
and build artifacts via `Swatinem/rust-cache` for faster runs.

## Building and running

The local environment used for implementation does not have a Rust toolchain,
so compile/test verification is delegated to the repository's GitHub Actions
workflow. Run the same checks locally before trusting a deployment:

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`cargo run --bin ben_snipes` starts the live poll loop. The Solana wallet is mandatory; the process exits before creating venues if `SOLANA_PRIVATE_KEY` is missing. Configured EVM chains also require their execution wallet and private RPC. The bot performs real execution only.


**For a first real Solana run:** set `risk.max_position_size` in
`config/default.toml` to the smallest amount you're willing to lose
entirely (not a "small but meaningful" amount - genuinely willing to
lose, given the unverified-code caveats above), run with
`RUST_LOG=debug` to see every decision the acquisition pipeline makes,
and watch the wallet address (logged at startup) on a block explorer
during the first several trades rather than trusting the bot's own logs
alone. PumpPortal doesn't support devnet, so there's no zero-risk way to
test the live path short of this - which is exactly why starting small
and watching closely matters here more than in most projects.

To enable Solana execution: `export SOLANA_PRIVATE_KEY="<base58-encoded
secret key>"` before running - the base58 format `solana-keygen` and
most wallet exports use. It is mandatory for startup. **Read
the warnings in "Automation & execution platforms" before setting this
to a real, funded wallet's key.**

## Current implementation

The acquisition path is intentionally minimal: source-level deduplication, 24h volume filtering, cross-source acquisition-ledger reservation, exchange execution prechecks, buy execution, position tracking, and take-profit exits.

## License

MIT - see `LICENSE`.

## Historical replay / backtesting

The workspace now includes a deterministic replay engine and `ben_snipes-backtest` binary. It consumes a JSON dataset containing timestamped listing observations, reference prices, 24h volume and market cap, then applies the same volume acquisition criteria, position sizing, maximum-position cap, and take-profit rule used by the application.

Example:

```text
cargo run -p ben_snipes-backtest -- data/backtest-example.json
```

This is a **strategy-rule replay**, not a market simulator. It does not claim to reproduce order-book depth, MEV, gas, latency, spread, slippage, partial fills, or venue-specific execution. The report therefore treats the replay price as the reference fill price. Live persistent P&L remains explicitly separate from this historical result.
