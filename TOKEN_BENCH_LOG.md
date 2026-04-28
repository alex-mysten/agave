# Single-Destination SPL Token (Stablecoin) Bench

Extends `SOLANA_BENCH_LOG.md`. Same shape of question — how fast can the
validator commit single-destination 1-unit deposits — but with **SPL Token
transfers** (Tokenkeg... program, custom mint) instead of native SOL
transfers. This simulates stablecoin-like deposit flow: a hot dest token
account is the contended writable, the per-tx work includes SPL Token
program BPF execution and an SPL `Account` (165-byte) state update on both
source and destination.

Two complementary measurements:
1. **Replay ceiling** (custom packed-ledger generator) — the validator's
   execute-path upper bound under perfect conditions. This was the
   primary investigation; sections below up to "Live bench" cover it.
2. **Live bench** (`bench-tps --use-token`) — what the live single-node
   cluster actually commits when fed by a normal sender. Final section.

## Summary — SOL vs SPL Token, live and replay

Single-destination 1-unit deposits on the same Mac dev cluster, same
commit, same session. "Replay" = ceiling from a maximally-packed slot
in a synthetic ledger. "Live" = sustained rate of deposits actually
committed when a single bench-tps client feeds the validator.

| measurement | tx shape | SOL | Token | Token vs SOL |
|---|---|---:|---:|---:|
| **Replay ceiling, plain** | 1-ix Transfer (token: 2-ix, +CB-limit) | **~50,000 TPS** (peak 50,420 at 27k txs/slot) | **~37,400 TPS** (peak at 21,260 txs/slot) | **-25%** |
| **Replay ceiling, nonced** | 3-ix bench-tps shape (token: 4-ix) | **~42,000 TPS** (peak 41,576 at 19k txs/slot) | **~27,600 TPS** (peak at 16,100 txs/slot) | **-34%** |
| Live bench, 60s, fresh val | nonced (3-ix SOL / 4-ix token) | 509–643 dps (n=2) | 533–573 dps (n=2) | within noise |
| **Live bench, 600s sustained, fresh val** | nonced | **104 dps** | **94 dps** | **-10%** |
| Live as % of replay ceiling | nonced, 600s | ~0.25% | ~0.34% | — |

Density-vs-mode dynamics:
- **Replay ceilings** are bound by the post-SIMD-0286 per-writable-account
  cost limit (40 M = block-units 100 M × 40%). Token modes hit it at
  fewer txs/slot than SOL (per-tx CU is higher: 1881 plain / 2484 nonced
  for token vs 1481 / 2084 for SOL).
- **Live rates: token is ~10% slower than SOL**, matching the per-tx
  execute cost direction. The gap is much smaller than the 25–35% gap
  visible at the replay ceiling because the live bench bottlenecks
  ~300–400× below its replay ceiling — far upstream of execute. So most
  of the SOL-vs-token execute-cost gap is invisible at live scale; only
  ~10% leaks through.
- **Short-window (60s) noise is large.** Run-to-run variance on a fresh
  validator is ~25% for the 60s window at this drop rate (95–99%). The
  600s sustained number is the one to quote; 60s should be averaged
  over multiple runs if used.
- **More instructions per tx ≠ proportionally lower TPS.** Going from
  1-ix to 3-ix SOL costs ~16% TPS at peak; from 2-ix to 4-ix token
  costs ~26%. Sigverify, account loads, write-lock acquisition, and
  state commit dominate per-tx cost; extra ComputeBudget +
  AdvanceNonceAccount ix add only a few μs each.

> **Methodology note.** Each live measurement above ran on its own
> freshly-bootstrapped single-node cluster (fresh genesis, fresh
> ledger). An earlier set of measurements ran multiple benches on the
> same long-lived validator, which gave noisy results: a 600s SOL run
> after a 600s token run came out 18% *lower* than the token run,
> apparently because the validator's accumulated state (rocksdb
> compaction, log entries, accounts-db growth) penalized whichever bench
> ran second. Always restart the validator between measurements.

## Headline result

**Replay ceiling (execute-path upper bound):**
- **Plain SPL Token Transfer** (1 ix per tx, no ComputeBudget price /
  AdvanceNonce): **~40,000 deposits/sec** sustained from 5k–20k txs/slot,
  with the per-account-cost ceiling at **~21,260 txs/slot** giving
  **~37,500 deposits/sec** at the densest viable pack.
- **Bench-tps-shape SPL Token Transfer** (3-ix: ComputeBudget price +
  AdvanceNonceAccount + Token Transfer + the implicit
  SetComputeUnitLimit): **~31,000 deposits/sec** sustained from 5k–14k
  txs/slot, ceiling at **~16,100 txs/slot** giving **~27,500 deposits/sec**.
- For comparison on the **same hardware and commit** (re-run together,
  not just quoting prior log): **SOL plain ~48k TPS** (peak 25k-tx slot),
  **SOL nonced ~43k TPS** (peak ~19k-tx slot). Token transfer is **15–30%
  slower per tx** at the same density and packs **~15–25% fewer txs per
  slot** than native SOL.

**Live bench (single-node cluster, sustained, each run on a fresh validator):**
- **Live SPL Token Transfer** (single-destination + durable nonce +
  sustained, 600s window, fresh validator): **94 deposits/sec**.
- **Same-config SOL baseline** on its own fresh validator: 104 dps.
  So **live token is ~10% slower than live SOL**, matching the per-tx
  execute-cost direction (token replay ceiling is 25–35% below SOL,
  but most of that gap is invisible at the live rate which is
  ~300–400× below either ceiling).
- 60s windows are too noisy on this hardware to compare meaningfully:
  fresh-validator SOL 60s ranges 509–643 dps run-to-run; token 60s
  ranges 533–573. Use the 600s sustained number as the headline.
- **Live-vs-replay gap is ~300× for tokens** (94 dps vs ~28k TPS
  nonced replay ceiling). Same magnitude for SOL (104 dps vs ~42k).

The bottleneck on density is the post-SIMD-0286 per-account-cost limit
(40 % of the 100 M block-units cap = 40 M CU per writable account). For
the dest token account this binds at:
- ~21k plain token txs (per-tx cost ~1881 CU)
- ~16k nonced token txs (per-tx cost ~2484 CU)

These ceilings are **not** the unique-account-count cap that bound the
SOL pure-transfer case at ~25k.

## Environment

Same host/toolchain/commit as the SOL packed-ledger work:
- **Host:** `Manageds-Virtual-Machine.local`, Darwin 25.2.0, aarch64
  (Apple Silicon VM).
- **Toolchain:** rustc 1.94.1, release build.
- **Repo:** `mystenmark/agave`, branch `steka-packed-ledger-replay`,
  HEAD `250591ae4a` on top of `970d6332cb`. The HEAD commit adds
  `--use-token` to the packed-ledger generator (~120 LOC).
- **Date:** 2026-04-28.

## Code changes for this experiment

| File | Change |
|---|---|
| `ledger-tool/src/bin/packed-ledger.rs` | New `--use-token` flag. In token mode, genesis injects: SPL Token program (Tokenkeg...) via `solana_program_binaries::spl_programs(&Rent::default())`; one custom mint pre-initialized with `decimals=6`; per-sender SPL `Account` token account (initialized, mint-bound, `amount=1_000_000`); one dest SPL `Account` token account (initialized, `amount=0`). Tx generation switches to `spl_token_interface::instruction::transfer(...)` of 1 unit per tx, prefixed with `ComputeBudgetInstruction::set_compute_unit_limit(6_000)` so per-tx programs-execution-cost doesn't default to 200k and starve the writable-account-cost budget. Stackable with `--use-nonce` (4-ix txs). |
| `ledger-tool/Cargo.toml`, `dev-bins/Cargo.toml` | New deps: `solana-program-binaries`, `solana-program-pack`, `solana-program-option`, `spl-generic-token`, `spl-token-interface`. |

Two non-obvious gotchas surfaced and are fixed in the patch:
1. **Empty SPL Token program account.** `create_genesis_config_with_leader`
   uses `Rent::free()` (zero rate), so
   `solana_program_binaries::spl_programs(&genesis_config.rent)` returned
   program/programdata accounts with `lamports=0`. Zero-lamport accounts
   are filtered as not-loadable, so the bank's tx executor reported
   `ProgramAccountNotFound` for every token tx and they all
   committed-but-failed (signatures verified, fees collected,
   `dest.amount` never moved). Fix: pass an explicit `Rent::default()` to
   `spl_programs()` so program lamports are properly rent-funded
   (~1.14M lamports for the 36-byte Program account).
2. **Empty mint / token-account funding.** Same `Rent::free()` issue —
   `genesis_config.rent.minimum_balance(82)` and `…(165)` both return 0,
   yielding zero-lamport state accounts that don't load. Fix: floor at
   2_000_000 lamports (same conservative floor we use for nonce accounts).

## How "successful deposits/sec" is measured

Same approach as the SOL packed bench: for each density, build a single
slot of N transfers, replay via `agave-ledger-tool verify --no-snapshot`,
and check that the dest token account's `amount` field went from 0 to N
post-replay. Wall-clock TPS is `N / slot_replay_wall_time`, where
slot_replay_wall_time is the gap between the slot-0 `bank frozen` and
slot-1 `bank frozen` lines. PoH chain verification + sigverify run on
parallel threads alongside the (single-threaded) execute path, so they
don't sit on the slot's critical path under tight PoH; the prior
sensitivity check in `SOLANA_BENCH_LOG.md` confirmed wall-time is
near-flat across `hashes_per_tick={2, 12500, 23809}`. The same logic
applies to token mode; not separately re-verified here.

## Tx shape under `--use-token`

Plain (`--use-token`):
1. `ComputeBudget::SetComputeUnitLimit(6000)`
2. `TokenInstruction::Transfer { amount: 1 }`
   - source: sender's pre-initialized token account
   - dest: shared dest token account
   - authority: sender keypair (signs as fee payer + transfer authority)

Bench-tps-shape (`--use-token --use-nonce`):
1. `ComputeBudget::SetComputeUnitPrice(1)`
2. `SystemProgram::AdvanceNonceAccount`  (injected by `Message::new_with_nonce`)
3. `ComputeBudget::SetComputeUnitLimit(6000)`
4. `TokenInstruction::Transfer { amount: 1 }`

Writable accounts per tx (token, plain): sender (fee payer), source
token account, dest token account = **3 writable**. SOL plain has 2.
Read-only: ComputeBudget program, Token program (and its programdata).

## Per-tx cost in the cost-model

Empirical from `cost_tracker_stats`:
- Plain token: **block_cost / tx_count = 1881 CU/tx**
- Nonced token: **2484 CU/tx**
- (Plain SOL: 1481 CU/tx; nonced SOL: 2084 CU/tx — same setup.)

The 600 CU difference between SOL and token is the
`ComputeBudget::SetComputeUnitLimit` requested limit subtracted off the
default-200k ceiling, plus extra write-lock cost for the additional
writable account (300 CU) and ComputeBudget instruction.

## Densities measured (plain `--use-token`)

| txs/slot | block_cost | unique accounts | slot Δt | **TPS** | dest.amount post-replay | notes |
|---:|---:|---:|---:|---:|---:|---|
| 5,000 | 9.4 M | 10,001 | 119 ms | **42,016** | 5,000 | |
| 10,000 | 18.8 M | 20,001 | 249 ms | **40,160** | 10,000 | |
| 12,000 | 22.6 M | 24,001 | 293 ms | **40,955** | 12,000 | |
| 14,000 | 26.3 M | 28,001 | 343 ms | **40,816** | 14,000 | |
| 20,000 | 37.6 M | 40,001 | 489 ms | **40,898** | 20,000 | |
| 21,000 | 39.5 M | 42,001 | 570 ms | **36,842** | 21,000 | last comfortable density |
| 21,260 | 39.99 M | 42,521 | 568 ms | **37,430** | 21,260 | **ceiling** — 39.99 M of 40 M limit |
| 22,000 | — | — | — | rejected | — | `WouldExceedMaxAccountCostLimit` |
| 25,000 | — | — | — | rejected | — | same |

## Densities measured (`--use-token --use-nonce`)

| txs/slot | block_cost | unique accounts | slot Δt | **TPS** | dest.amount | notes |
|---:|---:|---:|---:|---:|---:|---|
| 5,000 | 12.4 M | 15,001 | 160 ms | **31,250** | 5,000 | |
| 10,000 | 24.8 M | 30,001 | 326 ms | **30,675** | 10,000 | |
| 12,000 | 29.8 M | 36,001 | 380 ms | **31,579** | 12,000 | |
| 14,000 | 34.8 M | 42,001 | 450 ms | **31,111** | 14,000 | |
| 16,000 | 39.7 M | 48,001 | 534 ms | **29,963** | 16,000 | |
| 16,100 | 39.99 M | 48,301 | 584 ms | **27,568** | 16,100 | **ceiling** |
| 20,000+ | — | — | — | rejected | — | `WouldExceedMaxAccountCostLimit` |

## Same-hardware SOL baselines (re-run on this commit)

Old SOLANA_BENCH_LOG.md numbers were captured in a prior session with
different thermal / cache state; this is the apples-to-apples
side-by-side from this run.

| workload | density | slot Δt | TPS |
|---|---:|---:|---:|
| SOL plain | 10,000 | 207 ms | 48,310 |
| SOL plain | 14,000 | 292 ms | 47,945 |
| SOL plain | 25,000 (max) | 512 ms | **48,852** |
| SOL nonced | 10,000 | 235 ms | 42,553 |
| SOL nonced | 14,000 | 324 ms | 43,210 |
| SOL nonced | 25,000 | rejected | — `WouldExceedMaxAccountCostLimit` |

## Token vs SOL summary

| config | SOL TPS | Token TPS | Δ |
|---|---:|---:|---:|
| plain, 10k slot | 48,310 | 40,160 | -17% |
| plain, 14k slot | 47,945 | 40,816 | -15% |
| plain, peak / max-density | 48,852 (25k) | 37,430 (21,260) | -23% |
| nonced, 10k slot | 42,553 | 30,675 | -28% |
| nonced, 14k slot | 43,210 | 31,111 | -28% |
| nonced, peak / max-density | 43,210 (14k) | 27,568 (16,100) | -36% |

Stablecoin-shape (token) workload is consistently 15–35 % slower than
the SOL transfer ceiling on the same hardware and commit, with the
nonced-bench-tps shape being the most penalized. The gap is in two
places:

1. **Per-tx execute is heavier.** Token transfer runs SBF (BPF) code
   plus two SPL `Account` (165-byte) state loads/saves; SOL transfer is
   a builtin doing two lamport-only updates. ~3–7 μs more per tx on
   this hardware.
2. **Density ceiling is lower.** Token plain bottoms out at 21,260 (vs
   25,000 for SOL plain) because per-tx cost is higher and the dest
   account budget binds first.

## Why the per-account-cost limit is 40 M (not 24 M)

`MAX_WRITABLE_ACCOUNT_UNITS = 24_000_000` is the pre-SIMD-0286 default,
but `Bank::apply_cost_tracker_limits_for_active_features` recomputes it
when the `raise_block_limits_to_100m` feature is active:

```rust
let block_cost_limit = if raise_block_limits_to_100m {
    simd_0286_block_limit() // = 100M
} else {
    cost_tracker.get_block_limit() // = 60M
};
let account_cost_limit = block_cost_limit * 40 / 100; // 40M or 24M
```

Our packed ledger uses `FeatureSet::all_enabled()`, so SIMD-0286 is on
and the per-account budget is 40 M. The empirical ceilings match this
exactly: plain at 39.99 M with 21,260 txs, nonced at 39.99 M with 16,100
txs.

A pre-SIMD-0286 mainnet-shape ledger would cap at:
- ~12,750 plain token txs/slot at the same ~40k TPS rate (24 M / 1881)
- ~9,660 nonced token txs/slot at the same ~31k TPS rate (24 M / 2484)

i.e. the throughput per-slot would be roughly halved by the older
account-cost cap. The post-SIMD-0286 ceilings here are the right ones to
quote for a current/next-epoch mainnet.

## Live bench

Same single-node dev cluster as `SOLANA_BENCH_LOG.md` Run 1, with a new
`--use-token` flag to bench-tps. At setup, bench-tps creates a fresh
mint, one SPL token account per gen_keypair (each owned by its
gen_keypair so the owner pays for and signs its own create+init,
avoiding fee-payer-write-lock contention at funder), and mint_to-funds
each. Tx generation switches to
`ComputeBudget::SetComputeUnitLimit(6000)` + `spl_token::Transfer(1)`
(plus `AdvanceNonceAccount` if `--use-durable-nonce`). End-of-run
summary reports the dest token account's amount delta over the run
duration.

### Bench-tps changes

| File | Change |
|---|---|
| `bench-tps/src/cli.rs` | New `--use-token` flag; `Config.use_token`. Requires `--single-destination`. |
| `bench-tps/src/token_setup.rs` (new, ~280 LOC) | `setup_token_accounts()` builds the mint + one token account per gen_keypair + mint_to fund. Uses `send_batch` for create+init (each owner pays its own; idempotent retry — second tx fails with "account already exists"), and `send_batch + signature tracking via get_signature_status` for mint_to (non-idempotent; can't retry without confirming each individual signature, else accounts get double-minted). |
| `bench-tps/src/bench.rs` | `TransactionChunkGenerator` gets `token_context: Option<Arc<TokenContext>>` + `sender_to_token_account: HashMap<Pubkey, Pubkey>`. New `generate_token_txs` and `generate_nonced_token_txs` paths. New end-of-run "Single-destination token deposit summary" line that reads dest amount delta. |
| `bench-tps/src/main.rs` | Calls `setup_token_accounts` after `generate_durable_nonce_accounts`, passes resulting `TokenContext` into `do_bench_tps`. |
| `bench-tps/Cargo.toml` | Adds `solana-program-pack`, `spl-generic-token`, `spl-token-interface`. |

### Setup gotchas surfaced

1. **Single-fee-payer write-lock at scale.** Initial implementation had
   the bench-tps funder pay for every token-account create. With
   N=20,000 gen_keypairs (typical `--tx-count 10000 --keypair-multiplier 2`
   single-destination setup), that meant 20k create_account txs all
   holding the funder's write-lock — banking stage processes those
   serially within a slot, and many timed out before being included.
   First-round confirmation rate dropped to ~0/round after the initial
   burst, and the loop never converged. Fix: each owner pays for and
   signs its own token-account create+init. The funder is no longer the
   write-lock contention point. Setup time for 20k accounts dropped
   from "doesn't converge" to ~6 seconds.
2. **`mint_to` is not idempotent.** First-attempt code retried mint_to
   like nonce-create — send batch, sleep, check `state.amount > 0`,
   retry pending. But mint_to with a re-signed tx on a different
   blockhash gets a new signature and lands as a separate tx, so if
   both attempts land the account is double-minted. Observed in a
   smoke test: dest had `2,000,006,163` units at end-of-run when
   only ~6,163 deposits had occurred during the bench (1B over-minted
   during setup). Fix: track each mint_to's signature, only consider
   that account funded once `get_signature_status` returns
   `Some(Ok(()))` for *that exact signature*. Retry only if the
   signature ends up dropped past the blockhash window (75s).

### Densities measured

All runs use single-destination + durable-nonce + sustained, with
`--keypair-multiplier 2` (one chunk = `tx_count` sources rotating
through the same dest). The `--keypair-multiplier 8` default (4
chunks) was not exercised here — its setup would be 4× heavier
(80k token accounts to create + mint_to-fund). **Each measurement
ran on its own freshly-bootstrapped validator** to avoid
cumulative-state contamination between runs.

| config | duration | deposits | dps | peak 1s | drop |
|---|---:|---:|---:|---:|---:|
| Token, sustained, multi=2, fresh val (run 1) | 60s | ~32,000 | 533 | 4,337 | 0.95 |
| Token, sustained, multi=2, fresh val (run 2) | 60s | 34,575 | 573 | 5,578 | 0.96 |
| **Token, sustained, multi=2, fresh val** | **600s** | 56,456 | **94.04** | 4,657 | 0.99 |

### Same-config SOL baselines (fresh validator, this session)

Same flags, same hardware, same multiplier=2, same session — re-measured
today rather than quoted from the (multiplier=8, prior-session)
SOLANA_BENCH_LOG numbers — to give a same-config side-by-side.

| config | duration | deposits | dps | peak 1s | drop |
|---|---:|---:|---:|---:|---:|
| SOL, multi=2, fresh val (run 1) | 60s | 39,457 | 643 | 6,796 | 0.94 |
| SOL, multi=2, fresh val (run 2) | 60s | 31,260 | 509 | 6,459 | 0.95 |
| **SOL, sustained, multi=2, fresh val** | **600s** | 62,659 | **104.36** | 7,008 | 0.99 |

### Token vs SOL, live

| window | SOL dps | Token dps | Δ | notes |
|---|---:|---:|---:|---|
| 60s, fresh val | 509–643 (n=2) | 533–573 (n=2) | within noise | ~25% run-to-run variance dominates the comparison |
| **600s sustained, fresh val** | **104** | **94** | **-10%** | matches per-tx execute-cost direction |

At 60s, the run-to-run variance (95–99% drop, stochastic
banking-stage / sender pacing) is large enough that single runs
can't distinguish SOL from token. The 600s sustained number averages
over enough slots that the underlying per-tx cost gap shows through:
**token is ~10% slower than SOL live**, in the same direction as
(but much smaller than) the 25–35% replay-ceiling gap.

### Live-vs-replay gap

| workload | live (600s, fresh val) | replay ceiling (this hardware) | gap |
|---|---:|---:|---:|
| SOL nonced | 104 dps | ~42,000 TPS | ~400× |
| Token nonced | 94 dps | ~28,000 TPS | ~300× |

Both modes hit ~0.25–0.35% of their respective replay ceilings under
a single live bench-tps client. The bottleneck is upstream of
execute (banking-stage scheduler, sender pacing, durable-nonce-account
churn, QUIC stream limits) — none of which is materially different
between SOL and token mode. So **live token transfer rate is roughly
~10% slower than live SOL**, dramatically smaller than the 25–35%
gap at the replay ceiling.

The corollary: improvements that lift the live ceiling (better
scheduler, multi-client, longer-lived nonces) help both modes; the
full SOL-vs-token throughput gap only manifests once the live system
can sustain rates within striking distance of the per-account-cost
ceiling.

### Note on `--keypair-multiplier`

The original SOLANA_BENCH_LOG durable-nonce runs used the bench-tps
default (`--keypair-multiplier 8`, 4 chunks, 80k keypairs). Today's
multiplier=2 runs are at lower keypair-pool density and produce lower
absolute dps than the original log's multiplier=8 run on SOL (292 dps
sustained 600s, multiplier=8). The relative SOL-vs-token comparison
above is fair because both are at multiplier=2 today. Re-running both
at multiplier=8 today is a follow-up; expected directional finding
(token ≈ SOL at the live ceiling) should still hold but with both
landing higher in absolute terms.

## Caveats

Same set as SOLANA_BENCH_LOG.md, plus:

1. **Empty, hot accounts-db.** Token accounts and the mint live in the
   in-memory AccountsCache HashMap with all hits. Real cluster has 100M+
   accounts in rocksdb; cold-cache loads of an SPL `Account` (~165 B
   read + 165 B write per tx, 2× per tx in token mode) would amortize
   differently than the in-memory path.
2. **No competing workloads / mainnet noise.** Replay runs alone.
3. **Single mint, single dest.** The hot account is fixed; in mainnet,
   stablecoin flows split across multiple dest accounts (treasury,
   exchange hot wallets, AMM pools), which would unblock parallelism
   the per-account-cost limit blocks here.
4. **All token accounts pre-initialized.** No `InitializeAccount` /
   `Allocate` work in the slot — that would change accounts-data-delta
   accounting and add per-tx CU.
5. **`Transfer`, not `TransferChecked`.** `Transfer` is the deprecated
   one-instruction path; `TransferChecked` adds the mint as a read-only
   account and is what current SDKs default to. Per-tx CU would rise
   slightly.
6. **`spl-p-token-1.0.0-rc.1`.** The Pinocchio rewrite of the legacy
   spl-token program. ELF size and per-tx CU should be similar but not
   identical to the pre-Pinocchio token program some validators run
   today.

## Open questions / follow-ups

1. **Cross-hardware.** Re-run on the `amd-zen3` host. SOLANA_BENCH_LOG
   showed nonced SOL workload was ~30% slower on Zen 3 than the Mac VM,
   plausibly due to per-CCX-L3 cache pressure. Token mode adds even more
   per-tx state-load work; the cross-hardware gap may widen.
2. **`TransferChecked` shape.** Re-run with the modern transfer path.
3. **Mixed-dest.** Two dest token accounts split round-robin to expose
   how much of the ceiling is the per-account budget vs the per-block
   budget.
4. **Token-2022 with extensions.** Confidential transfers, transfer
   hooks, etc., have very different per-tx CU profiles and would
   warrant a separate measurement.
5. **Live at multiplier=8.** Today's same-session live comparison was
   at `--keypair-multiplier 2` for tractability. Re-run both SOL and
   token at the bench-tps default multiplier=8 to land closer to the
   original SOLANA_BENCH_LOG live numbers (292 dps SOL sustained); the
   SOL-vs-token ratio should still be ~1:1.
6. **Multi-client.** A single bench-tps client appears to bottleneck
   live commit at ~1–2% of the replay ceiling. Multiple parallel
   clients targeting the same dest would test whether the ceiling lifts.

## Reproducing

Build (same env tweaks as SOLANA_BENCH_LOG.md):

```bash
export LIBCLANG_PATH=/Applications/Xcode_26.3.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib
export DYLD_FALLBACK_LIBRARY_PATH=$LIBCLANG_PATH

CARGO_TARGET_DIR=$PWD/dev-bins/target cargo build --release \
  --manifest-path dev-bins/Cargo.toml \
  --bin packed-ledger --bin agave-ledger-tool
```

Generate + replay (plain token, ceiling-density):

```bash
rm -rf /tmp/packed-token
./dev-bins/target/release/packed-ledger \
  --ledger /tmp/packed-token --txs-per-slot 21260 \
  --num-slots 1 --use-token

./dev-bins/target/release/agave-ledger-tool \
  --ignore-ulimit-nofile-error --ledger /tmp/packed-token \
  verify --no-snapshot
```

Wall time is in the `ledger processed in …` log line; per-account cost
in the `cost_tracker_stats` line. Confirm dest balance moved:

```bash
./dev-bins/target/release/agave-ledger-tool \
  --ignore-ulimit-nofile-error --ledger /tmp/packed-token \
  accounts --account <dest_token_account_pubkey>
# bytes 0x40..0x48 (LE u64) of the 165-byte payload should equal txs_per_slot
```

Bench-tps-shape (3-ix nonced):

```bash
./dev-bins/target/release/packed-ledger \
  --ledger /tmp/packed-token-nonce --txs-per-slot 16100 \
  --num-slots 1 --use-token --use-nonce
```

Live bench (token, single-destination, durable nonce, sustained):

```bash
# 1. Build solana-bench-tps with --use-token support (and the rest of
#    the validator binaries used by the multinode-demo wrapper).
CARGO_TARGET_DIR=$PWD/target cargo build --release \
  --bin agave-validator --bin solana-genesis \
  --bin solana-faucet --bin solana-keygen
CARGO_TARGET_DIR=$PWD/dev-bins/target cargo build --release \
  --manifest-path dev-bins/Cargo.toml --bin solana-bench-tps

# 2. Bring up local cluster.
export PATH="$PWD/target/release:$PWD/dev-bins/target/release:$PATH"
export USE_INSTALL=1
rm -rf config
./multinode-demo/setup.sh
./multinode-demo/faucet.sh &
./multinode-demo/bootstrap-validator.sh &
until curl -s -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' \
  http://127.0.0.1:8899 | grep -q '"ok"'; do sleep 1; done

# 3. Run the live token bench.
./multinode-demo/bench-tps.sh \
  --single-destination --use-token --use-durable-nonce --sustained \
  --tx-count 10000 --keypair-multiplier 2 --duration 600
```

Look for the line `Single-destination token deposit summary: ...
delta N units = N successful 1-unit token transfers over Ts =
X successful deposits/sec` for the ground-truth metric. `--use-token`
requires `--single-destination`. Setup time scales linearly with
`tx_count * keypair_multiplier` (one token account per keypair); on
this host, `--tx-count 10000 --keypair-multiplier 2` (= 20k token
accounts) takes ~7 seconds to set up.
