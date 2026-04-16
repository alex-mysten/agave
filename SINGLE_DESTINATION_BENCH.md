# Single-Destination TPS Benchmark — Configurations and Results

This document records a benchmarking experiment to measure the throughput of an
Agave validator when every transaction deposits to the **same destination
account**. The core question: how many such transactions per second can a
single-node validator commit successfully?

## Headline result

On a single-node dev cluster (macOS, Apple Silicon, default banking-stage
scheduler, durable-nonce sender), the validator commits roughly **850–1,500
successful single-destination 1-lamport deposits per second**, bounded by
single-account write-lock serialization in the banking stage. Without durable
nonces the apparent cluster TPS is much higher (up to ~40k peak), but the
overwhelming majority of those transactions are committed-but-failed (stale
blockhash by the time the scheduler reaches them) — they do not actually
transfer funds.

## Environment

- **Repo:** `mystenmark/agave`, branch `solana-bench` (HEAD `2d279e16ba`,
  agave-validator `4.1.0-alpha.0`)
- **Host:** `Manageds-Virtual-Machine.local`, Darwin 25.2.0, aarch64
- **Toolchain:** rustc 1.94.1, release build (`cargo build --release`)
- **Cluster:** single-node dev cluster via `multinode-demo/`
- **Date:** 2026-04-16

### Validator startup args (via `multinode-demo/bootstrap-validator.sh`)
```
--no-snapshot-fetch
--no-poh-speed-test
--no-os-network-limits-test
--no-enforce-ulimit-nofile           (added in this branch — see below)
--rpc-port 8899
--snapshot-interval-slots 200
--no-incremental-snapshots
--rpc-faucet-address 127.0.0.1:9900
--no-wait-for-vote-to-start-leader
--full-rpc-api
--allow-private-addr
--gossip-port 8001
```

### Genesis config (defaults from `multinode-demo/setup.sh`)
- Cluster type: development
- Slots per epoch: 8192
- Slot duration: 6.25 ms
- Hashes per tick: 13452
- FeeRateGovernor: target 10,000 lamports/sig, max 100,000

## Code changes added for this experiment

All changes scoped to this experiment, no behavioral change to existing flags.

| File | Change |
|---|---|
| `bench-tps/src/cli.rs` | New `--single-destination` flag and `Config.single_destination` field. Mutually exclusive with `--num-conflict-groups`. |
| `bench-tps/src/bench.rs` | New `KeypairChunks::new_with_single_destination` constructor (every chunk's destination VecDeque points at the same single keypair); `TransactionChunkGenerator.single_destination` field; `advance()` suppresses the periodic source/dest swap when single-destination is set; before/after destination-balance reporting and "successful deposits/sec" log line; new unit test. |
| `validator/src/commands/run/args.rs` | New `--no-enforce-ulimit-nofile` validator flag (hidden). Required on macOS where the per-process file-descriptor cap is below the validator's hard-coded 1M target. |
| `validator/src/commands/run/execute.rs` | Wire `--no-enforce-ulimit-nofile` into `enforce_ulimit_nofile` in the validator config. |
| `multinode-demo/bootstrap-validator.sh` | Pass `--no-enforce-ulimit-nofile` so the demo script works on macOS. |

## How "successful deposits/sec" is measured

Built-in bench-tps metrics report cluster `getTransactionCount` deltas, which
include vote transactions and failed system transfers (a system transfer that
errors out still counts as committed and still pays fees). To measure
**ground-truth successful deposits**, the bench now snapshots the
single-destination account's `lamports` balance at run-start and run-end and
reports the delta. Each successful 1-lamport transfer increments that balance
by exactly 1, so `delta_lamports` equals `successful_deposits`.

Look for this line at end of run:
```
Single-destination deposit summary: <pubkey> balance <start> -> <end>
  (delta N lamports = N successful 1-lamport deposits over Ts =
   X successful deposits/sec)
```

## Results

All runs used `--keypair-multiplier 2` (Phase A & B) or default keypair-multiplier
of 8 (nonce runs). All bench commands run via `multinode-demo/bench-tps.sh`.

### Notation
- **Cluster avg TPS** — what bench-tps prints as `Average TPS`. This is
  `cluster_tx_count_delta / duration` and includes votes and failed-but-
  committed transfers.
- **Successful deposits/sec** — destination-balance delta divided by run
  duration. Only counts transfers that actually moved a lamport.
- **Drop rate** — `(sender_attempted - cluster_committed) / sender_attempted`,
  i.e. fraction of txs the sender pushed that never landed in any block. **Note:
  this is a fraction (0.75 = 75%), not a percent.**

### Run 1 — Phase A: existing `--num-conflict-groups 1`, alternating dest

```
multinode-demo/bench-tps.sh \
  --num-conflict-groups 1 --keypair-multiplier 2 --duration 60
```

| metric | value |
|---|---|
| Highest 1s sample | 17,721 TPS |
| Cluster avg TPS | 989 |
| Cluster-committed txs | 67,707 over ~67s |
| Drop rate | 0.90 (90%) |
| Successful deposits/sec | not directly measured pre-instrumentation; estimated < 100 |

Behavior was bursty: 5–17k TPS for 1–3 seconds, then ~0–3 TPS for 10+ seconds.
Caveat at the time: with `keypair_multiplier=2` only one chunk is generated, so
`TransactionChunkGenerator::advance()` flips `reclaim_lamports_back_to_source`
on every cycle, swapping source and destination. So the destination *alternated*
between two keypairs rather than being truly fixed. This motivated Phase B.

### Run 2 — Phase B: new `--single-destination`, no nonces

```
multinode-demo/bench-tps.sh \
  --single-destination --keypair-multiplier 2 --duration 60
```

| metric | value |
|---|---|
| Highest 1s sample | 39,941 TPS |
| Cluster avg TPS | 11,787 |
| Cluster-committed txs | 714,016 over ~60s |
| Drop rate | 0.68 (68%) |
| Successful deposits/sec | **~0** (verified via dest balance: unchanged across this and a back-to-back rerun) |

The patch worked structurally — log confirmed `Single-destination mode: every
transaction targets 6yY5XMhBRQ61uFb4DYbbzYWxuJ7NKoX1XnSGju39cEw5` — and apparent
cluster throughput jumped 12× over Phase A. But the destination's on-chain
balance did not move at all between two back-to-back 60s and 30s runs. The
12k cluster TPS were almost entirely committed-but-failed system transfers —
their blockhash had expired by the time the banking-stage scheduler picked them
up under single-account write-lock contention.

This was the trigger to add **durable nonces** and **on-chain balance
verification**.

### Run 3 — `--single-destination --use-durable-nonce`, 30s

```
multinode-demo/bench-tps.sh \
  --single-destination --use-durable-nonce \
  --tx-count 10000 --duration 30
```

| metric | value |
|---|---|
| Destination | `5ZNg45Yj3mqVhTNLjDYBQA9RP4Nhsoy78N3ou7L7couF` |
| Starting balance | 999,433,200 lamports |
| Ending balance | 999,482,747 lamports |
| Successful deposits | 49,547 over 33.27 s |
| **Successful deposits/sec** | **1,489.38** |
| Cluster avg TPS | 1,493 |
| Highest 1s sample | 6,450 |
| Drop rate | 0.65 (65%) |

Cluster avg TPS and successful deposits/sec now match within ~0.3% — durable
nonces eliminate blockhash expiry, so virtually every committed tx is also a
successful deposit. The drop rate is the sender pushing more than the
banking stage can commit.

### Run 4 — `--single-destination --use-durable-nonce`, 60s

```
multinode-demo/bench-tps.sh \
  --single-destination --use-durable-nonce \
  --tx-count 10000 --duration 60
```

| metric | value |
|---|---|
| Destination | `77QnVyJ4xhUQM3oqLgXpuoLTZYypAxF2ddJwvgReLzPD` |
| Starting balance | 999,433,200 lamports |
| Ending balance | 999,486,400 lamports |
| Successful deposits | 53,200 over 62.43 s |
| **Successful deposits/sec** | **852.13** |
| Cluster avg TPS | 854.93 |
| Highest 1s sample | 4,591 |
| Drop rate | 0.79 (79%) |

The 60s number is materially lower than the 30s number. Per-second sampler
data shows the bench is bursty (sign-chunk → send → wait for queue to drain →
advance → repeat) and burst peaks decay over the course of the run:

```
Burst peaks (TPS) in chronological order:
  4592, 4129, 4003, 4419, 2423, 2973, 1136, 1508, 3207, 1575,
   145, 1929, 1541, 2394, 1307, 1553, 1350, 1281,  533,  570,
   502, 1347, 1192, 1417,  932,  286,  830, 1012,  412,  815,
   807,  836
```

Early bursts hit 4k TPS, late bursts settle to ~500–1,300. The 30s run is
dominated by the high-burst early window; the 60s run averages over the slower
tail.

### Run 5 — `--single-destination --use-durable-nonce --sustained`, 60s

```
multinode-demo/bench-tps.sh \
  --single-destination --use-durable-nonce --sustained \
  --tx-count 10000 --duration 60
```

| metric | value |
|---|---|
| Destination | `BshU4n5CYd6GEWLDd1sEDFn5CZTJanABNs4Znr3aP4aL` |
| Starting balance | 999,433,200 lamports |
| Ending balance | 999,490,741 lamports |
| Successful deposits | 57,541 over 60.57 s |
| **Successful deposits/sec** | **949.92** |
| Cluster avg TPS | 954.52 |
| Highest 1s sample | 6,721 |
| Drop rate | 0.75 (75%) |

`--sustained` overlaps tx generation with sending so the queue is always
non-empty. This nudged throughput up only ~10–15% over the non-sustained 60s
run, and the same decay shape persisted:

```
Burst peaks (TPS), chronological:
  early: 3410, 4843, 6721, 4743, 2547, 3520, 4925, 3539
  mid:   1650, 2047,  520, 2124, 2233, 1382,  685, 1238, 1599, 1418, 294
  late:    63, 1127, 1391, 1230,  186, 1165, 1036,   55, 1136
```

That sustained mode did not flatten the curve indicates the slowdown is
**validator-side**, not sender-starvation. Likely candidates: RocksDB
compaction, accounts-db growth, slot-time variability under sustained
single-account write load.

## Summary table — successful deposits/sec across configs

| config | duration | successful deposits/sec |
|---|---|---|
| `--num-conflict-groups 1` (no nonces) | 60s | < 100 (estimated) |
| `--single-destination` (no nonces) | 60s | ~0 |
| `--single-destination --use-durable-nonce`, tx-count 10000 | 30s | **1,489** |
| `--single-destination --use-durable-nonce`, tx-count 10000 | 60s | **852** |
| `--single-destination --use-durable-nonce --sustained`, tx-count 10000 | 60s | **950** |

Defensible single number for this single-node macOS dev cluster:
**~900 successful deposits/sec**, with steady-state around 500–1,000 depending
on burst phase.

## Real-world parallel

For context: on Solana mainnet, on-chain orderbook market-makers like those on
**Phoenix** exhibit the same write-lock contention pattern. Every MM transaction
on a Phoenix market (e.g. SOL/USDC, market state PDA
`6cZ6AMRNFcLkSjBEHxmDYgrc6N9EuCNWUARNiBP484FL`) write-locks the same market
state account, so the entire MM population on that pair serializes through one
account — directly analogous to this benchmark's single-destination pattern,
but with much higher CU usage per tx. A typical Phoenix MM batches
cancel + cancel + requote into a single atomic transaction with a sequence-
enforcer guard against stale bundles. Drift v2 perps share the same hot-account
shape (perp market account is the lock); OpenBook v2 splits hot state across
bids/asks/event-heap which somewhat reduces single-account pressure.

## Caveats and follow-ups

- **Single-node, macOS, dev cluster.** Not a production-shaped number. Linux
  with proper `kern.maxfilesperproc` tuning would let us drop the
  `--no-enforce-ulimit-nofile` workaround and exercise the full configured
  path.
- **Default block-production-method (banking stage).** Worth retrying with
  `--block-production-method central-scheduler`.
- **Single bench client.** Multiple parallel `bench-tps` instances would test
  whether the cap is in the bench client or the validator.
- **Per-tx CU is minimal** (1-lamport system transfer). Real workloads (e.g.
  Phoenix MM) use orders of magnitude more CU per tx; throughput would be
  proportionally lower.
- **No priority fees.** With priority fees the scheduler ordering would change.
- **Steady-state characterization.** The decay over 60s is unexplained;
  validator metrics output during the run would identify whether RocksDB
  compaction, accounts-db, or something else is responsible.

## Reproducing

From repo root:

```bash
# 1. Build
export LIBCLANG_PATH=$(xcrun --show-sdk-path)/../../Toolchains/XcodeDefault.xctoolchain/usr/lib
cargo build --release \
  --bin agave-validator --bin solana-genesis \
  --bin solana-faucet --bin solana-keygen
cargo build --release --manifest-path dev-bins/Cargo.toml --bin solana-bench-tps

# 2. Bring up a fresh local validator
export PATH="$PWD/target/release:$PWD/dev-bins/target/release:$PATH"
export USE_INSTALL=1
rm -rf config
./multinode-demo/setup.sh
./multinode-demo/faucet.sh &
./multinode-demo/bootstrap-validator.sh &

# wait for RPC health
until curl -s -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' \
  http://127.0.0.1:8899 | grep -q '"ok"'; do sleep 1; done

# 3. Run the bench (sustained + nonces is the cleanest setup)
./multinode-demo/bench-tps.sh \
  --single-destination --use-durable-nonce --sustained \
  --tx-count 10000 --duration 60
```

The "Single-destination deposit summary" line in the output is the
ground-truth measurement.
