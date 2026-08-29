# ben_snipes

An autonomous new-listing sniper: watches for tokens becoming newly
tradable, buys the ones that pass a volume/safety filter, and exits at a
configurable take-profit or stop-loss. **DEX-only.** CEX listings were
deliberately dropped - they're too rare and too slow relative to
on-chain launches to be worth the surface area for a bot whose whole
edge is being early. See "Automation & execution platforms" below for
how buys/sells are meant to actually get executed.

**Status: the Solana pipeline is fully wired end to end.** Real
detection, real volume filtering (DexScreener), a real safety gate
(RugCheck), a real cross-source dedup ledger, and - when
`SOLANA_PRIVATE_KEY` is set - real buy/sell execution. **This means it
can autonomously spend real funds.** See "Automation & execution
platforms" for exactly what's verified vs. best-effort in each piece,
and read every module doc comment it points to before funding a
wallet. EVM execution is detection-only still - see "Not yet
implemented."

## Architecture

Hexagonal (ports and adapters), split across a Cargo workspace:

```
crates/domain/       pure business types and rules: Listing, Chain,
                      CanonicalTokenId, Position, ProfitTarget, StopLoss,
                      AcquisitionCriteria, SafetyCriteria. No I/O.
crates/ports/         traits the application depends on: ListingSource,
                      ListingStateStore, AcquisitionLedger,
                      ExchangeClient, MetricsProvider,
                      TokenSafetyChecker, Clock.
crates/application/   use cases: NewListingDetector, AcquisitionEngine,
                      PositionManager. Depends only on domain + ports.
crates/config/        typed config loading (TOML + env overrides).
crates/adapters/
  statefile/          ListingStateStore (per-source JSON snapshots) and
                      AcquisitionLedger (cross-source dedup), both
                      file-backed with atomic temp-file+rename writes.
  ws-support/          shared reconnect-with-backoff helper for the two
                      websocket-backed real adapters below.
  pumpfun/             REAL Solana pipeline, five modules:
                      listing detection (PumpPortal websocket),
                      execution.rs (signing/broadcast) +
                      exchange_client.rs (buy/sell, falls back to
                      detection-only with no wallet), metrics_provider.rs
                      (DexScreener volume), safety_checker.rs (RugCheck),
                      price_feed.rs (Jupiter price, SOL-denominated).
  evm-onchain/         REAL EVM ListingSource: subscribes directly to a
                      DEX factory's pair-creation logs over eth_subscribe.
                      Chain/factory/event-agnostic, configured per chain.
  dex-mock/            synthetic demo venue (buy+exit works end to end
                      with fake data) - kept so `cargo run` demonstrates
                      the full pipeline without needing real funds.
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

### New-listing detection strategy

`NewListingDetector` supports two source shapes:

- **Cursor-based incremental** (`ListingSnapshot::Incremental`) - a
  push-based feed like `pumpfun`/`evm-onchain` just forwards whatever
  arrived since the last poll; no diffing needed.
- **Full-snapshot diff** (`ListingSnapshot::Full`) - a venue that only
  exposes "here's everything right now" gets diffed against a persisted
  set of dedupe keys (`dex-mock` demonstrates this path).

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

### Autonomous acquisition, safety gate, and exit

`AcquisitionEngine` per detected listing: check volume via
`MetricsProvider` -> check `AcquisitionCriteria` (`risk.min_volume_24h`,
the sole gate - market cap doesn't disqualify a listing either way) ->
check the optional `SafetyGate` (honeypot/rug signals: sell tax,
ownership renounced, liquidity locked, mintable supply -
`safety.max_sell_tax_bps` in config) -> reserve the `CanonicalTokenId`
in the ledger -> size from `risk.max_position_size` and buy.
`PositionManager` then watches every open position and exits on
whichever of `risk.take_profit_percent` / `risk.stop_loss_percent`
fires first.

**`dex-mock`** has a real, working `SafetyGate` with hand-set
demo data, purely to prove the full pipeline. **`pumpfun` and
`evm-onchain` have no `SafetyGate` configured** - their
`MetricsProvider` always returns `None` (no real volume/market-cap
source exists yet for either), which makes `AcquisitionEngine` correctly
refuse to buy anything through them. That's the current, deliberate,
safe state - not a bug. Their `ExchangeClient` is likewise a
loud-on-call placeholder that should be structurally unreachable, since
the metrics check always bails out first.

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
same way. If `SOLANA_PRIVATE_KEY` isn't set, `main.rs` falls back to
`NoWalletExchange` automatically - detection keeps running, buying/
selling just stays inert, rather than the whole bot refusing to start.

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

**Volume filtering, safety gate, and price monitoring are now real too -
at three different confidence levels, and it matters which is which:**

- **`current_price` (Jupiter Price API v3, `price_feed.rs`) - high
  confidence.** Verified against a literal example response in Jupiter's
  own docs. One easy-to-miss detail already handled: Jupiter's prices
  are USD-denominated, but `entry_price` throughout this codebase is
  SOL-denominated (it comes from `quote_amount spent in SOL / quantity
  received`). Comparing a raw USD price against a SOL-denominated
  take-profit/stop-loss target would be wrong by roughly the SOL/USD
  exchange rate, not a rounding error - `fetch_price` converts by
  fetching SOL's own price in the same batched call.
- **`MetricsProvider` (DexScreener single-token lookup,
  `metrics_provider.rs`) - high confidence.** Free, keyless, well
  corroborated. Note this is a *different* DexScreener endpoint than the
  one this project deliberately avoided for detection - that was the
  paginated new-pairs firehose; this is a single lookup by an address
  you already have, which was never the endpoint with the pagination
  problem.
- **`TokenSafetyChecker` (RugCheck, `safety_checker.rs`) - mixed
  confidence, and the module doc comment is explicit about which parts.**
  `mintAuthority`/`freezeAuthority` field names are independently
  corroborated by two sources. Liquidity-lock detection is best-effort.
  **Sell-tax detection is not meaningfully implemented** - RugCheck
  doesn't appear to expose it, so `sell_tax_bps` is always `0`, which
  means *unverified*, not confirmed-safe. There's also a residual risk
  worth naming directly: if the two authority field names turn out to be
  wrong, they'd silently read as "renounced" (safe) rather than erroring
  - fail-*open*, the opposite of this codebase's usual default. One
  fail-closed guard is in place (a missing `token` sub-object entirely
  aborts the assessment), but it can't catch a merely-wrong field name
  within an otherwise-present object. Verify against a live response
  before trusting this with real funds.

**The port-shape mismatch flagged two rounds ago is fixed.**
`ExchangeClient` used to only offer a quantity-based `submit_order`,
which assumed you already know a price - PumpPortal's actual buy
interface is "spend this much SOL," with no price to pre-compute
without bonding-curve math. `submit_buy_by_amount` is now a first-class
port method: `AcquisitionEngine` spends `position_size` directly and
gets back a `FilledBuy { quantity, entry_price }` reporting what
actually happened. Selling stays quantity-based (`submit_order`) since
by the time you're exiting, the quantity is already known.

**Consolidated risk summary, because this is the round where the bot
became capable of spending real funds:** (1) the signing code in
`execution.rs` is built on solana-sdk primitives unverified against the
current pinned version - read that module's doc comment; (2) RugCheck's
safety gate has a real fail-open risk if its field-name assumptions are
wrong, and doesn't check sell-tax at all; (3) nothing here has been
compiled or run, this environment has no network or Rust toolchain.
Start with the smallest `max_position_size` you're willing to lose
entirely, watch the logs (`RUST_LOG=debug`), and watch the wallet
address on a block explorer during the first several trades.

**EVM - not started.** [Alloy](https://alloy.rs) for building/signing
transactions (ethers-rs, mentioned earlier in this project, is now
officially deprecated in favor of Alloy) - either calling a DEX router
directly or going through a price aggregator (0x/1inch) for better
routing, submitted via Flashbots Protect (Ethereum) or the equivalent
private RPC per chain for sandwich protection.

## Continuous integration

`.github/workflows/build.yml` runs on every push and pull request (plus
manual dispatch): installs Rust via apt (this project's standing
convention, rather than rustup or a toolchain action), then `cargo
build --workspace`, `cargo test --workspace`, and `cargo clippy
--workspace --all-targets -- -D warnings`. Caches the cargo registry
and build artifacts via `Swatinem/rust-cache` for faster runs.

## Building and running

No network or Rust toolchain was available in the environment this was
built in, so **none of this has been compiled or tested** - written to
be correct, but unverified. Run before trusting it:

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`cargo run --bin ben_snipes` starts the poll loop against the demo
venue (buys/exits with synthetic data), the real PumpPortal Solana
source (real detection, real volume/safety filtering, and real buy/sell
execution if `SOLANA_PRIVATE_KEY` is set - **this can spend real
funds**, see "Automation & execution platforms" before setting it), and
any EVM chains listed in `config/default.toml`'s `evm_chains` (empty by
default - see the commented example in that file for what's required to
enable one: your own RPC websocket URL with an API key, and a verified
`topic0` for the target factory's creation event).

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
most wallet exports use. Leave it unset to run detection-only. **Read
the warnings in "Automation & execution platforms" before setting this
to a real, funded wallet's key.**

## Not yet implemented

- **Real sell-tax detection.** `RugCheckSafetyChecker` always reports
  `sell_tax_bps = 0` - not because it's confirmed zero, but because
  RugCheck doesn't appear to expose this and no sell-simulation-based
  check has been built. If this matters for your risk tolerance, don't
  rely on `SafetyCriteria`'s tax check via this checker alone.
- **EVM trade execution and safety/metrics data.** Alloy + a
  router/aggregator + Flashbots Protect for execution (per "Automation &
  execution platforms"); `MetricsProvider`/`TokenSafetyChecker` for EVM
  are still `None`-always placeholders. None of this has been started.
- **Pre-trade balance checks and retry logic.** No check that the
  wallet has enough SOL for a buy plus fees before attempting one, and
  no retry-with-backoff on transient RPC failures (a single failed RPC
  call currently just fails that attempt outright, relying on the next
  poll tick to retry from scratch).
- **Wallet secrets management.** `SOLANA_PRIVATE_KEY` is read directly
  from the environment - fine for a single trusted deployment, not for
  production secrets hygiene. A real deployment wants this from a
  secrets manager (Vault, AWS Secrets Manager, etc.), ideally with an
  HSM, and a hot wallet capped to what you can afford to lose regardless.
- **Circuit breaker / kill switch.** No global cap on concurrent open
  positions, no auto-pause on repeated failures or an unusually large
  burst of listings (often signals a spoofed feed).
- **Persisted open positions.** `open_positions` lives only in the
  runner's in-memory `Vec` - a crash forgets anything not yet closed,
  with no reconciliation against the exchange/chain on restart.
- **Multi-instance ledger coordination.** See the `AcquisitionLedger`
  limitation noted above.
- **EVM `topic0` values.** Not hardcoded anywhere on purpose - see
  `ben_snipes-adapter-evm-onchain`'s crate docs for why, and what to do
  instead before enabling a chain.
- **Transaction simulation before signing.** A pre-flight
  `simulateTransaction` check would catch some failures before spending
  a real fee attempting them - not implemented.

## License

MIT - see `LICENSE`.
