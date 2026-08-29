# ben_snipes

An autonomous new-listing sniper: watches for tokens becoming newly
tradable, buys the ones that pass a volume/safety filter, and exits at a
configurable take-profit or stop-loss. **DEX-only.** CEX listings were
deliberately dropped - they're too rare and too slow relative to
on-chain launches to be worth the surface area for a bot whose whole
edge is being early. See "Automation & execution platforms" below for
how buys/sells are meant to actually get executed.

**Status: real detection for Solana + EVM; real buy/sell execution for
Solana when a wallet is configured.** See "Automation & execution
platforms" for what "real" means here and its caveats, and "Not yet
implemented" for what's still missing (most importantly: a live price
feed, without which nothing actually buys yet regardless of execution
being wired up).

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
  pumpfun/             REAL Solana ListingSource via PumpPortal's
                      subscribeNewToken feed, plus REAL buy/sell
                      execution via their Local Transaction API
                      (execution.rs + exchange_client.rs) - falls back
                      to detection-only if no wallet is configured.
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

**What's still genuinely missing, and why it matters:** even with real
execution wired in, nothing buys yet - `MetricsProvider` for Solana
still always returns `None` (no live volume source exists), which
makes `AcquisitionEngine` correctly refuse to buy anything. And even if
that's fixed, `PositionManager` still needs `current_price()` to decide
*when* to exit, which `PumpPortalExchangeClient` still can't provide -
that needs either bonding-curve account reads or a live trade-stream
subscription, neither built yet. These two gaps turn out to be the same
underlying need (real-time per-token price/volume data), so they're
naturally one future piece of work, not two.

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
source (detects real listings; execution is live if `SOLANA_PRIVATE_KEY`
is set, but nothing will actually buy yet regardless - see "Not yet
implemented" for why), and any EVM chains listed in
`config/default.toml`'s `evm_chains` (empty by default - see the
commented example in that file for what's required to enable one: your
own RPC websocket URL with an API key, and a verified `topic0` for the
target factory's creation event).

To enable Solana execution: `export SOLANA_PRIVATE_KEY="<base58-encoded
secret key>"` before running - the base58 format `solana-keygen` and
most wallet exports use. Leave it unset to run detection-only. **Read
the warnings in "Automation & execution platforms" before setting this
to a real, funded wallet's key.**

## Not yet implemented

- **Live price/volume feed for Solana.** This is now the single biggest
  blocker to anything actually buying: `MetricsProvider` (needed to pass
  acquisition criteria) and `current_price()` (needed for exit timing)
  both need real-time per-token data that doesn't exist yet -
  bonding-curve account reads or a `subscribeTokenTrade` aggregation are
  the two candidate approaches (see "Automation & execution platforms").
- **EVM trade execution.** Alloy + a router/aggregator + Flashbots
  Protect, per "Automation & execution platforms" - hasn't been started.
- **Real Solana/EVM safety data.** `TokenSafetyChecker` is a
  `None`-always placeholder for both real adapters. Solana needs
  pump.fun-specific checks (mint/freeze authority state); EVM needs a
  real sell-simulation or contract-read check.
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

## License

MIT - see `LICENSE`.
