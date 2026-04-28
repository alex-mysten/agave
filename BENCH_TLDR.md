# Solana Single-Destination Throughput — TL;DR

The full chronicle is split across three logs, in chronological order:

1. **`SINGLE_DESTINATION_BENCH.md`** — established the single-destination
   live-bench workflow on Agave: added `--single-destination` to bench-tps,
   verified deposits via on-chain dest-balance delta, found that without
   durable nonces the apparent cluster TPS is ~12k but real successful
   deposits are ~0 (committed-but-failed transfers).
2. **`SOLANA_BENCH_LOG.md`** — extended with replay measurements, a
   custom packed-ledger generator, CPU profiling, and a cross-hardware
   Linux Zen 3 comparison. Identified that the live bench is bottlenecked
   far upstream of execute. Re-measured today's SOL packed-replay
   numbers on a non-thermally-throttled machine.
3. **`TOKEN_BENCH_LOG.md`** — extended both replay and live to SPL Token
   transfers (stablecoin-shape workload), re-using the same
   single-destination methodology with a custom mint and per-sender
   token accounts.

This file consolidates the **best, methodologically-clean numbers**
from all three, and a one-paragraph summary suitable for citation. All
numbers are from the same Apple Silicon Mac VM, agave-validator
4.1.0-alpha.0 at HEAD `2c046e2ab4` (on top of `970d6332cb` and master
`2d279e16ba`). Cross-hardware Zen 3 numbers, where included, are
flagged inline as captured in a prior session and not re-measured
today.

## Best numbers

Single-destination 1-unit deposits, single-node dev cluster, Apple
Silicon Mac VM (16 cores, 16 GB), unless noted.

| measurement | tx shape | SOL | SPL Token | Token vs SOL |
|---|---|---:|---:|---:|
| **Replay ceiling, plain transfer** | 1-ix Transfer (token: 2-ix +CB-limit) | **50,420 TPS** at 27,000 txs/slot | **~37,400 TPS** at 21,260 txs/slot | -25% |
| **Replay ceiling, bench-tps shape (nonced)** | 3-ix (CB-price + AdvanceNonce + Transfer); token: 4-ix | **41,576 TPS** at 19,000 txs/slot | **~27,600 TPS** at 16,100 txs/slot | -34% |
| **Live, 600s sustained, fresh validator** | nonced (3-ix SOL / 4-ix token), `--keypair-multiplier 2` | **104 dps** | **94 dps** | -10% |
| Live, 60s, fresh validator (per-run, n=2 each) | nonced | 509–643 dps | 533–573 dps | within noise |
| Cross-hardware: replay, 25k pure transfer (Zen 3 EPYC 7443P, prior session) | 1-ix Transfer | 41,000 TPS | not measured | — |

Per-tx CU under cost-model: SOL plain 1481 / nonced 2084; token plain
1881 / nonced 2484. Per-writable-account budget post-SIMD-0286 = 40 M
(= 100 M block × 40%). All four ceilings above bind exactly when the
dest writable account hits ~40 M cumulative cost.

## One-paragraph summary

Agave validator on a single Apple Silicon Mac VM commits roughly
**50,000 native SOL transfers per second** and **~37,000 SPL Token
(stablecoin-shape) transfers per second** to a single hot destination
account when the slot is maximally packed at the per-writable-account
cost limit (40 M CU = 40% of the post-SIMD-0286 100 M block-units
cap), bounded not by unique-account count but by per-account cost.
Adding the bench-tps-shape `ComputeBudget::SetComputeUnitPrice` +
`AdvanceNonceAccount` instructions drops these to ~42k SOL / ~28k
token. **Token transfer is 25–35% slower than SOL at the replay
ceiling** — heavier per-tx execute (SBF program + 165-byte SPL
`Account` state load/save vs lamports-only system updates), an extra
writable account per tx. **Live throughput is dramatically lower:
~100 deposits/sec sustained over 600 s for either mode**, around
0.25% of the replay ceiling, with the bottleneck far upstream of
execute (banking-stage scheduler, single-fee-payer write-lock churn,
sender pacing, durable-nonce-account contention, QUIC stream limits)
— factors that don't materially differ between SOL and token modes,
which is why the token-vs-SOL execute-cost gap shrinks from 25–35% at
replay to ~10% at live scale. The live ceiling is sender-bound and
single-client; multi-client / improved scheduler is the lever.

## Key findings, distilled

### From `SINGLE_DESTINATION_BENCH.md` (replay infrastructure & first measurements)

- **Without durable nonces**, single-destination live throws away
  apparent throughput: cluster commits ~12k TPS but virtually all are
  committed-but-failed system transfers (stale blockhash by the time
  the scheduler reaches them under single-account write-lock
  contention). Dest balance does not move.
- **With durable nonces** (`--use-durable-nonce`), the cluster commit
  rate matches successful-deposit rate within ~0.3%. This is the
  required setup for any meaningful live single-destination bench.
- 30 s burst gives ~1,489 dps; 60 s window ~852 dps; 600 s sustained
  ~292 dps (multiplier=8). Decay is severe — early bursts hit 4 k+
  TPS, late settles to 500–1,300 TPS. Validator-side decay (not
  sender-starvation: `--sustained` doesn't flatten the curve).

### From `SOLANA_BENCH_LOG.md` (replay ceiling + cross-hardware)

- **Live bench is far below replay ceiling** — ~100 dps live vs
  ~25,000 TPS on the densest live-bench slot when replayed. Live is
  bottlenecked upstream of execute.
- **CPU profile of replay** (under full PoH verify): ~70% of cycles
  in SHA256 (PoH chain verification). PoH runs on parallel threads
  alongside execute, so wall-time is unchanged when PoH is reduced;
  CPU drops 42×, wall drops 6.3× when `--skip-verification` is
  enabled.
- **Custom packed-ledger generator** (`ledger-tool/src/bin/packed-ledger.rs`)
  measures the execute-path ceiling directly by synthesizing a
  maximally-packed slot bypassing banking stage. SOL plain peak
  50,420 TPS at 27,000 txs/slot; SOL nonced 41,576 TPS at 19,000.
  Earlier in the same chronicle, thermally-throttled measurements on
  the same commit gave ~35k peak — re-measured today on a cooler
  machine yields the higher numbers above.
- **Per-writable-account-cost limit is the binding constraint** at
  density. With SIMD-0286 active (`raise_block_limits_to_100m`), the
  limit is `block_units × 40% = 40 M`. Pre-SIMD-0286 it was 24 M, so
  ceilings on a current-mainnet validator would be roughly halved.
- **Cross-hardware** (Zen 3 EPYC 7443P, Linux bare metal, prior
  session): SOL plain ports closely (~41k peak); SOL nonced drops
  ~30% (~25k), plausibly cache / memory-subsystem effects on the
  per-tx nonce-account load. Re-running on Zen 3 today is open.

### From `TOKEN_BENCH_LOG.md` (SPL Token — stablecoin-shape)

- **Token replay ceilings are 25–35% below SOL** at the same density,
  for the same reason as the cross-hardware Zen 3 nonced gap: heavier
  per-tx state-load work and an extra writable account per tx.
- **Token-mode genesis must use `Rent::default()`** (not the dev
  cluster's `Rent::free()`) when sourcing SPL program accounts —
  otherwise `bpf_loader_upgradeable_program_accounts` produces
  zero-lamport accounts that are filtered out as not-loadable, and
  every token transfer fails at execution.
- **`mint_to` is non-idempotent.** Live setup must track per-tx
  signatures and only consider an account funded once that exact
  signature is confirmed; state-based retry (`is amount > 0`)
  double-mints under network jitter, inflating the dest's apparent
  starting balance during the bench.
- **Live SOL vs live token: -10%**, dramatically smaller than the
  25–35% replay-ceiling gap. Live bottlenecks ~300–400× below the
  execute ceiling, and the bottleneck is upstream of execute, so most
  of the per-tx-cost gap is invisible. Only ~10% leaks through.

## Methodology essentials

- **Replay measurements** use a custom-generated single-slot ledger
  (`packed-ledger`). Wall time of the slot replay (between the slot
  0 and slot 1 `bank frozen` log lines) is the throughput measurement.
  PoH-rate sensitivity verified: `hashes_per_tick={2, 12500, 23809}`
  produce identical wall-times because PoH verify runs parallel to
  execute on separate threads.
- **Live measurements** must use durable nonces (`--use-durable-nonce`)
  and verify deposits via the dest account's on-chain balance delta,
  not bench-tps's printed cluster-TPS (which counts committed-but-
  failed transfers).
- **Each live measurement must run on its own freshly-bootstrapped
  validator.** Cumulative state on a long-lived dev validator
  (rocksdb compaction, accounts-db growth) penalizes whichever bench
  ran second; running multiple benches sequentially gives nonsense
  cross-mode comparisons.
- **60 s windows are too short** at this hardware's drop rate
  (95–99%): run-to-run variance is ~25%. Use 600 s sustained.
- The packed-replay ceilings are best-case-by-construction. Mainnet
  shape (loaded accounts-db, competing workloads, fee-market pressure,
  steady-state multi-slot drift, snapshot service) would land lower —
  measuring that gap is open follow-up work.

## Reproducing in one block

```bash
export LIBCLANG_PATH=/Applications/Xcode_26.3.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib
export DYLD_FALLBACK_LIBRARY_PATH=$LIBCLANG_PATH

# Build everything
CARGO_TARGET_DIR=$PWD/target cargo build --release \
  --bin agave-validator --bin solana-genesis \
  --bin solana-faucet --bin solana-keygen
CARGO_TARGET_DIR=$PWD/dev-bins/target cargo build --release \
  --manifest-path dev-bins/Cargo.toml \
  --bin solana-bench-tps --bin agave-ledger-tool --bin packed-ledger

# Replay ceiling — SOL nonced, peak density
./dev-bins/target/release/packed-ledger \
  --ledger /tmp/sol-nonced --txs-per-slot 19000 --num-slots 1 --use-nonce
./dev-bins/target/release/agave-ledger-tool --ignore-ulimit-nofile-error \
  --ledger /tmp/sol-nonced verify --no-snapshot

# Replay ceiling — Token nonced, peak density
./dev-bins/target/release/packed-ledger \
  --ledger /tmp/token-nonced --txs-per-slot 16100 --num-slots 1 \
  --use-token --use-nonce
./dev-bins/target/release/agave-ledger-tool --ignore-ulimit-nofile-error \
  --ledger /tmp/token-nonced verify --no-snapshot

# Live bench — bring up fresh cluster, run, tear down (run for both modes)
export PATH="$PWD/target/release:$PWD/dev-bins/target/release:$PATH"
export USE_INSTALL=1
rm -rf config && ./multinode-demo/setup.sh
./multinode-demo/faucet.sh > /tmp/faucet.log 2>&1 &
./multinode-demo/bootstrap-validator.sh > /tmp/validator.log 2>&1 &
until curl -s -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' \
  http://127.0.0.1:8899 | grep -q '"ok"'; do sleep 1; done

# SOL live
./multinode-demo/bench-tps.sh \
  --single-destination --use-durable-nonce --sustained \
  --tx-count 10000 --keypair-multiplier 2 --duration 600

# Token live (restart cluster between modes for clean numbers)
./multinode-demo/bench-tps.sh \
  --single-destination --use-token --use-durable-nonce --sustained \
  --tx-count 10000 --keypair-multiplier 2 --duration 600
```

The headline log lines:
- Replay: `ledger processed in N ms` after the slot-1 `bank frozen` line.
  TPS = `txs_per_slot / wall_seconds`.
- Live: `Single-destination deposit summary: ... X successful deposits/sec`
  for SOL, `Single-destination token deposit summary: ... X successful
  deposits/sec` for token.

## Open questions (cross-cutting)

In rough priority order:

1. **Loaded accounts-db.** Pre-populate the ledger with a large dummy
   account set before the packed slot to model cache-miss pressure.
   The packed-replay ceilings here assume a hot, empty accounts-db
   (every load is a cache hit); mainnet-scale (100M+ accounts) would
   slow the per-tx execute path by an unmeasured amount.
2. **Re-run cross-hardware on this commit.** Zen 3 numbers in the log
   are from a thermally-throttled prior session; the actual Mac-vs-Zen-3
   ratio at today's measurements is unknown.
3. **Multi-client live bench.** Single bench-tps client appears to
   bottleneck live commit at ~0.25% of replay ceiling. Multiple
   parallel clients targeting the same dest would test whether the
   live ceiling lifts.
4. **Live at `--keypair-multiplier 8`** to land closer to the original
   SOLANA_BENCH_LOG numbers (292 dps SOL sustained); SOL-vs-token
   ratio at this multiplier is unmeasured.
5. **`TransferChecked` shape.** Token bench uses the deprecated
   `Transfer` ix; current SDKs default to `TransferChecked` (extra
   read-only mint reference, slightly higher per-tx CU).
6. **Token-2022 with extensions.** Confidential transfers, transfer
   hooks, etc. have very different per-tx CU profiles.
7. **`central-scheduler` vs `central-scheduler-greedy`** —
   experiment whether denser banking-stage scheduling lifts the
   live ceiling.
8. **`simulate-block-production` on captured banking trace** to test
   whether a different scheduler would pack denser blocks than what
   live banking actually produced.
9. **Multi-slot packed runs** to surface accounts-db growth /
   bank-fork bookkeeping / snapshot-service overhead.
10. **Newer server-class CPU** (Zen 4, Sapphire Rapids) for a more
    representative \"modern validator\" number, especially for the
    nonced workload which is memory-subsystem-sensitive on Zen 3.
11. **Corresponding Sui throughput on the same hardware** — the
    headline missing datapoint for the cross-chain comparison story.
