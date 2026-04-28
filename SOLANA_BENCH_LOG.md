# Single-Destination Bench — Replay & Packed-Block Experiments

Extends `SINGLE_DESTINATION_BENCH.md`. Same question (how many
single-destination 1-lamport deposits per second can an Agave validator
commit), new angles: long-window live bench, replay-side throughput,
CPU profile breakdown, and a custom packed-ledger generator that
measures the execute-path ceiling directly. Also a cross-hardware
comparison on a Linux Zen 3 server.

## Headline result

- **Live bench, sustained 600s**, single-node dev cluster: ~290
  deposits/sec averaged. The familiar 60s ~1,500 dps number is
  burst-inflated and not sustained.
- **Replay** of the same ledger reaches ~25k TPS on the densest bench
  slots; aggregate is diluted by fixed per-slot overhead (PoH verify,
  bank freeze).
- **Custom packed-ledger replay** at full block packing tops out at
  **~50,000 deposits/sec** for 1-instruction transfers (ceiling at
  27,000 txs/slot, bound by the 40 M per-writable-account-cost limit),
  and **~42,000 deposits/sec** for 3-instruction nonced transfers
  (ceiling at 19,000 txs/slot, same limit). Write-lock-serialized
  execute is the bottleneck.
- **Cross-hardware on Linux bare metal (AMD Zen 3):** pure-transfer
  ceiling holds (~35–41k on prior measurement); nonced drops ~30%
  (~25k), likely cache / memory-subsystem effects on the per-tx
  nonce-account load. Zen 3 numbers below were captured in an earlier
  session and have not been re-measured today; treat the absolute
  values as illustrative, the ratios as still-current.

> **Note on update.** The packed-replay numbers were re-measured on
> this Mac host (same commit) after observing thermally-throttled
> behavior in an earlier session — old numbers were ~35k peak and
> ~37k peak respectively. Today's same-commit re-run on a cooler
> machine landed at the ~50k / ~42k peaks above and is reproducible
> across runs. The thermally-throttled prior numbers have been removed
> below; Zen 3 numbers (captured in that earlier session, not today)
> are flagged inline. See `TOKEN_BENCH_LOG.md` for the same-session
> SOL baseline used in the SPL Token comparison.

The large gap between live bench and packed-replay numbers means the
live bench is bottlenecked far upstream of execute (banking stage,
ingestion, client rate). The packed-replay number is a best-case
ceiling; realistic mainnet conditions (loaded accounts-db, competing
workloads, fee-market pressure) would land lower, and measuring that
gap is a follow-up.

## Environment

- **Host:** `Manageds-Virtual-Machine.local`, Darwin 25.2.0, aarch64
  (Apple Silicon VM). Cross-hardware runs on an `amd-zen3` host:
  Ubuntu 22.04, AMD EPYC 7443P (Zen 3, 24c/48t), 251 GB RAM, bare metal.
- **Toolchain:** rustc 1.94.1, release build.
- **Repo:** `mystenmark/agave`, branches `mlogan-single-destination-bench`
  (HEAD `970d6332cb`, original session) and `steka-packed-ledger-replay`
  (HEAD `250591ae4a`, today's re-measurement; only adds the
  `--use-token` flag to packed-ledger), both on top of master
  `2d279e16ba`, agave-validator `4.1.0-alpha.0`. The Run 5 packed-ledger
  numbers were captured today; live-bench (Runs 1–4) and Zen 3
  cross-hardware (Run 6) numbers are from the original session.

### Build-env tweaks (macOS / Xcode 26.3)

The `LIBCLANG_PATH` snippet from the baseline doc doesn't resolve under
Xcode 26.3, and this host has a global
`target-dir = ~/.cargo/sui-target` in `~/.cargo/config.toml` that
breaks the multinode-demo PATH setup. Use:

```bash
export LIBCLANG_PATH=/Applications/Xcode_26.3.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib
export DYLD_FALLBACK_LIBRARY_PATH=$LIBCLANG_PATH    # librocksdb-sys build script needs this at runtime
export CARGO_TARGET_DIR=$PWD/target                  # override the global sui-target location
```

Default genesis has `hashes_per_tick=23809` (vs 13,452 in earlier
snapshots); this affects live-bench slot wall-time but not the
replay-side numbers.

## Run 1 — Long-window live bench (sustained + nonce, 600s)

```
./multinode-demo/bench-tps.sh \
  --single-destination --use-durable-nonce --sustained \
  --tx-count 10000 --duration 600
```

| metric | 600s | 60s baseline |
|---|---:|---:|
| Successful deposits | 175,766 | 90,970 |
| **Successful deposits/sec** | **292** | 1,482 |
| Drop rate | 0.97 (97%) | 0.86 (86%) |

10× duration produced only ~2× deposits. The decay is severe: the
60s average is dominated by the opening ~10s burst. Per-second samples
show 4–10k TPS in the first 10 seconds decaying to ~500–800 TPS by
second 20 and lower thereafter. **292 dps is the defensible
sustained-load number** for this single-node setup.

## Run 2 — Replay of Run 1's ledger (`agave-ledger-tool verify`)

```
agave-ledger-tool --ignore-ulimit-nofile-error \
  --ledger config/bootstrap-validator/ verify --no-snapshot
```

`--ignore-ulimit-nofile-error` is the macOS analogue of the validator's
`--no-enforce-ulimit-nofile`.

Goal: disambiguate whether the ~292 dps was bottlenecked by banking
stage / ingestion / sender, or by execution itself. The single-account
write-lock in `execute_batch` is the same in both paths; if execution
were the cap, replay would match the live rate.

| metric | value |
|---|---:|
| Ledger slots | 1,912 (1,879 rooted) |
| Total txs in ledger | 360,730 |
| Wall time | 48.88 s |
| User + sys CPU | 588 s |
| **Average cores used** | **12 of 16 (75%)** |
| Aggregate dps over 175,766 deposits | ~4,400 |
| Densest bench slot replay TPS | ~25,000 |

Per-slot replay on representative bench slots:

| slot | tx count | replay Δt | TPS |
|---:|---:|---:|---:|
| 190 | 2,953 | 120 ms | ~24,600 |
| 192 | 4,813 | 215 ms | ~22,400 |
| 195 | 2,614 | 92 ms | ~28,400 |
| 200 | 2,191 | 74 ms | ~29,600 |

Replay is **~14× the live commit rate**. The live-bench bottleneck is
upstream of execute; per-tx execute is ~40 μs on this hardware.

12 cores used despite single-thread execute: sig-verify runs on
`replay_tx_thread_pool` in parallel, accounts-db / rocksdb threads
work concurrently, and parallel setup-phase slots (keypair funding,
nonce-account creation — no single-dest contention) saturate cores
during their replay window. Idle cores are blocked waiting on the
execute thread, not slack the workload could use.

## Run 3 — `--skip-verification` + `sample(1)` profile

Same ledger, two additional runs to isolate where replay CPU goes.

| mode | wall | user+sys CPU | avg cores |
|---|---:|---:|---:|
| `verify` (full, default) | 45.6 s | 571 s | 12.5 |
| `verify --skip-verification` | **7.25 s** | 13.6 s | **1.9** |

Skipping PoH chain + sig verification cuts wall time 6.3× and CPU 42×,
and aggregate dps converges to ~28k — same as the dense-slot ceiling
already showing in Run 2. Removing PoH/sig overhead means every slot
hits the execute ceiling instead of the per-slot PoH ceiling.

Companion `sample(1)` profile of the full-verify run (30 s window,
~243k thread-samples), grouped by top-of-stack category:

| category | samples | % of active |
|---|---:|---:|
| SHA256 (`sha2::sha256::compress256`, PoH chain verification) | 26,506 | ~70% |
| memory ops (memset / memmove / alloc) | 1,049 | ~3% |
| Ed25519 sig verify (`curve25519_dalek`) | 385 | ~1% |
| Execute path (accounts_db / loader / nonce / cost_tracker) | small + deeper | ~1–2% |

PoH SHA256 is the single biggest CPU consumer. `sample(1)`'s 10 ms
interval misses ~99% of `execute_batch` invocations (each ~40 μs), so
the profile under-represents execute; the skip-verify wall-time
measurement above is the better signal for execute-path cost.

## Run 4 — Aggressive bench-tps concurrency (denser bench slots)

```
solana-bench-tps \
  --url http://127.0.0.1:8899 --faucet 127.0.0.1:9900 \
  --bind-address 127.0.0.1 \
  --client-node-id config/bootstrap-validator/identity.json \
  --thread-batch-sleep-ms 0 --tx-count 20000 --duration 60 \
  -t 16 --tpu-connection-pool-size 8 \
  --single-destination --use-durable-nonce --sustained
```

Direct invocation (skipping the wrapper) so we can crank thread count
and queue-pool size. Goal: push banking stage to commit denser slots,
even if aggregate rate suffers.

| metric | aggressive 60s | baseline 60s |
|---|---:|---:|
| Successful deposits | 25,121 | 90,970 |
| **Successful deposits/sec** | **401** | 1,482 |
| Drop rate | 0.68 | 0.86 |

Aggregate dps went **down** — more sender threads thrashed the
scheduler rather than helping it. But individual slot density rose:

| slot | deposits | replay Δt | replay TPS |
|---:|---:|---:|---:|
| 315 | 6,204 | 211 ms | **29,400** |
| 384 | 5,988 | 194 ms | **30,900** |
| 416 | 5,211 | 199 ms | **26,200** |

Mean dense-bench replay TPS ~28,800 — measurably higher than the
~24,600 on smaller (~3k-tx) slots from Run 2. Per-slot fixed overhead
amortizes over more transactions as density grows. Still well short
of a maximally packed block (~10k–15k txs depending on tx shape).

## Run 5 — Custom packed-ledger generator

Live banking stage only reached ~6.2k single-dest deposits per slot —
its practical ceiling on this workload. To measure the packed-block
replay ceiling without that upstream cap, added a standalone generator:
**`ledger-tool/src/bin/packed-ledger.rs`**, ~250 lines.

What it does:

1. Builds a `GenesisConfig` via `create_genesis_config_with_leader`,
   then injects N pre-funded sender accounts (and 1 dest, and N
   pre-initialized durable nonce accounts in `--use-nonce` mode).
2. Sets `ticks_per_slot=64`, `hashes_per_tick=2` — minimum viable PoH
   to remove SHA chain cost from the wall-time measurement (a
   sensitivity sweep below confirms this doesn't inflate the headline).
3. `create_new_ledger` writes genesis + slot 0.
4. For each packed slot: builds one tx-entry (`num_hashes=1`,
   N-tx batch) followed by tick entries that close out the PoH count.
   Shreds via `Shredder::entries_to_merkle_shreds_for_tests`, chained
   to the parent's last shred merkle root (replay checks this).

Resulting ledger passes `verify --no-snapshot` cleanly. The 10k-tx run
was independently checked: dest balance went `1 → 10,001` post-replay
— every deposit succeeded, not committed-but-failed.

### Densities measured

Re-measured on this commit, same Mac host, in a fresh terminal session
not under thermal throttling. Per-tx cost from `cost_tracker_stats`:
1481 CU plain, 2084 CU nonced. Per-writable-account cost limit is 40 M
post-SIMD-0286 (= block-units 100 M × 40%); this is what binds density,
not unique-account count.

| txs/slot | mode | block_cost (CU) | per-acct cost | accounts | slot Δt | **TPS** |
|---:|---|---:|---:|---:|---:|---:|
| 5,000 | pure transfer | 7.4 M | 7.4 M | 5,001 | 108 ms | **46,729** |
| 10,000 | pure transfer | 14.8 M | 14.8 M | 10,001 | 207 ms | 48,310 |
| 14,000 | pure transfer | 20.7 M | 20.7 M | 14,001 | 292 ms | 47,945 |
| 20,000 | pure transfer | 29.6 M | 29.6 M | 20,001 | 395 ms | **50,633** |
| 25,000 | pure transfer | 37.0 M | 37.0 M | 25,001 | 512 ms | 48,852 |
| 27,000 | pure transfer | 39.99 M | 39.99 M | 27,001 | 535 ms | **50,420** |
| 28,000 | pure transfer | — | — | — | — | rejected: `WouldExceedMaxAccountCostLimit` |
| 5,000 | nonced (3-ix) | 10.4 M | 10.4 M | 10,001 | 117 ms | 42,735 |
| 10,000 | nonced | 20.8 M | 20.8 M | 20,001 | 235 ms | 42,553 |
| 14,000 | nonced | 29.2 M | 29.2 M | 28,001 | 324 ms | 43,210 |
| 18,000 | nonced | 37.5 M | 37.5 M | 36,001 | 432 ms | 41,667 |
| 19,000 | nonced | 39.6 M | 39.6 M | 38,001 | 457 ms | **41,576** |
| 19,200 | nonced | — | — | — | — | rejected: `WouldExceedMaxAccountCostLimit` |

Pure transfer ceiling: **27,000 txs/slot** at 39.99 M of 40 M
per-account budget. Nonced ceiling: **19,000 txs/slot** at 39.6 M.
Both bind on the per-writable-account-cost limit (40 M = block_units
100 M × 40%). The unique-account-count cap was misattributed in the
pre-update version of this doc; the actual binding constraint is the
per-account cost cap.

### Pure transfer vs nonced gap

At the same density, nonced runs ~13–18% slower than plain (e.g. 14k:
47.9k plain vs 43.2k nonced). The extra ComputeBudget +
AdvanceNonceAccount instructions add per-tx CU and write-lock work but
not 3× — most per-tx cost is in sig verify, account loads, and state
commit, which both modes pay equally.

### Headline number

Pure transfer: **~50,000 TPS** sustained across 5k–27k densities.
Nonced: **~42,000 TPS** sustained across 5k–19k. Both effectively
flat across densities, with peaks at the per-account-cost ceiling.

### PoH-rate sensitivity check (from prior session, still valid)

10k-nonced gen at three PoH levels (carried forward from the
thermally-throttled session — relative wall-times are still
representative):

| `hashes_per_tick` | wall | user CPU | total CPU |
|---:|---:|---:|---:|
| 2 | 0.54 s | 0.66 s | 0.91 s |
| 12,500 | 0.57 s | 0.88 s | 1.15 s |
| 23,809 (mainnet) | 0.55 s | 1.11 s | 1.31 s |

Wall time is essentially identical; only CPU goes up. PoH verification
runs on parallel threads alongside execute, so it doesn't extend the
critical path. The headline TPS holds under realistic PoH.

## Run 6 — Cross-hardware on `amd-zen3` (AMD EPYC 7443P, 48-thread, Linux)

Rebuilt the same branch + commit on the `amd-zen3` host: Ubuntu 22.04,
EPYC 7443P bare metal. Build took ~2 min for the main workspace, ~2 min
for dev-bins.

### Live bench (60s sustained + nonce)

| metric | amd-zen3 | Mac VM |
|---|---:|---:|
| Successful deposits | 93,214 | 90,970 |
| **Successful deposits/sec** | **1,528** | 1,482 |
| Peak 1s | 6,152 | 10,137 |
| Drop rate | 0.88 | 0.86 |

Essentially equal. The live-bench ceiling isn't hardware-bound —
banking stage + client coordination caps it at similar places on
both machines.

### Replay (same ledger)

Per-slot replay time is nearly identical across the two hosts
(~20–23 ms per slot). amd-zen3 has 30 idle cores of 48 but can't
get any individual slot to finish faster — the per-slot fixed
overhead (PoH verify, bank freeze, bank hash) is what's on the
critical path.

### Packed-ledger replay

Zen 3 numbers are from the original cross-hardware session (not
re-measured today). Mac VM numbers were captured in the same
thermally-throttled session, and on this commit are now ~1.4× higher
when re-measured (see Run 5). The Zen 3 column is therefore stale in
absolute terms; the **comparison ratio** between the two hardwares
should still be approximately right but warrants re-measurement.

| workload | amd-zen3 TPS (prior session) | Mac VM TPS (prior session, throttled) | Mac VM TPS (today, untrottled) |
|---:|---:|---:|---:|
| 10k pure transfer | 35,700 | 35,100 | **48,310** |
| 25k pure transfer | 41,000 | 36,800 | **48,852** |
| 27k pure transfer (max) | — | — | **50,420** |
| 10k nonced (3-ix) | 24,300 | 32,700 | **42,553** |
| 14k nonced | 25,400 | 36,700 | **43,210** |
| 19k nonced (max) | — | — | **41,576** |

The earlier conclusion still stands directionally: pure-transfer ceiling
ports closely to Linux bare metal, while nonced drops on Zen 3 due to
cache / memory-subsystem cost on the per-tx nonce-account load. But
quantitatively, the Mac peak is not 36k — it's ~50k for plain and ~42k
for nonced once the host isn't thermally throttled. Re-measure on
amd-zen3 to get an apples-to-apples 2026-04 comparison; the numbers
above shouldn't be used to argue specific Mac-vs-Linux gaps until
that's done.

Implications:

- The original Mac measurement was thermal-throttle-bound; the silicon
  goes higher than that prior session implied. Any single quoted Mac
  number should reference the today/untrottled column.
- For production-shape (nonced) workload, throughput is hardware-
  sensitive in ways the pure-transfer case isn't (per the prior-session
  Zen 3 measurement). Worth re-validating with a re-run.

## Flaws and caveats

Why the packed-replay number is a best-case-by-construction ceiling,
in priority order:

1. **Empty, hot accounts-db.** Our ledger has ~30k accounts total, all
   freshly written into the in-memory `AccountsCache` HashMap. Every
   account access is a cache hit. A real validator has 100M+ accounts
   in rocksdb, with cold-cache loads much slower than our in-memory
   path. The magnitude of the slowdown is workload- and
   hardware-specific and not measured here.
2. **No competing workloads.** Replay runs alone. A real validator
   simultaneously processes gossip, votes, shred reception, snapshots
   — all stealing CPU from execute.
3. **Single packed slot, no steady state.** Multi-slot runs would
   expose accounts-db growth, bank-fork bookkeeping, and snapshot
   service overhead.
4. **No fee-market / scheduler thrash.** Packed slots are pre-decided;
   real banking stage makes per-tx inclusion decisions. This is
   banking-stage-side, not replay-side, so it doesn't affect the
   ceiling directly — but no live validator could actually feed
   itself blocks at this rate.
5. **`hashes_per_tick=2` in the generator.** Verified above not to
   change wall-time, but the genesis is non-mainnet-shaped.

The packed-replay numbers are best-case-by-construction ceilings. Any
claim about loaded mainnet-shape validators requires measurement under
those conditions (follow-up #3).

## Summary

| config | Mac VM dps (today) | amd-zen3 dps (prior session) | notes |
|---|---:|---:|---|
| Live, 60s burst | 1,482 | 1,528 | burst-inflated |
| Live, 600s sustained (Run 1) | 292 | — | honest sustained rate |
| Live, aggressive client (Run 4) | 401 | — | denser slots, worse aggregate |
| Replay, live ledger peak slot (Run 2) | ~25,000 | ~25,000 | per-slot TPS |
| Replay, skip-verify (Run 3) | ~28,000 | — | removes PoH overhead |
| Packed, pure transfer ceiling (Run 5) | **~50,000** at 27k/slot | 41,000 at 25k/slot* | *Zen 3 not re-measured |
| Packed, 3-ix nonced ceiling (Run 5) | **~42,000** at 19k/slot | 25,400 at 14k/slot* | *Zen 3 not re-measured |
| Packed, plain SPL Token (TOKEN_BENCH_LOG) | ~37,400 at 21,260/slot | — | -23% vs SOL plain peak |
| Packed, nonced SPL Token (TOKEN_BENCH_LOG) | ~27,600 at 16,100/slot | — | -34% vs SOL nonced peak |

## Open questions / follow-ups

1. `central-scheduler` vs the default `central-scheduler-greedy` —
   does denser scheduling help banking stage commit more per slot?
2. Multiple parallel `bench-tps` clients targeting the same dest —
   would need a `--destination-pubkey` flag patch (small).
3. **Loaded accounts-db.** Pre-populate the ledger with a large
   dummy account set before the packed slot to model cache-miss
   pressure and compare against the empty-db numbers here.
4. `simulate-block-production` on the captured banking trace from
   Run 1 (~2.3 GB of events on disk) — does a different scheduler
   pack denser?
5. Multi-slot packed runs (currently single slot) to surface any
   steady-state drift.
6. **Corresponding Sui number on the same hardware** — the most
   important missing datapoint for the comparison story.
7. **Newer server-class CPU.** Zen 3 is 2021 silicon. A Zen 4 or
   Sapphire Rapids host would give a more representative "modern
   validator" number, particularly for the nonced workload which
   appears memory-subsystem-sensitive.
8. **Re-run Zen 3 on this commit.** The Run 6 cross-hardware Zen 3
   numbers were captured under the same thermal/cache state as the
   original (now-stale) Mac numbers. Re-run on amd-zen3 to settle the
   actual hardware ratio between Apple Silicon and EPYC for both
   plain and nonced. The directional finding (nonced is hardware-
   sensitive, plain is not) should still hold.
9. **Live SPL Token bench.** Companion to `TOKEN_BENCH_LOG.md` —
   add `--use-token` to bench-tps to measure the live-vs-replay gap
   for stablecoin-shape transfers, analogous to what was done for SOL.

## Reproducing

### Build

```bash
export LIBCLANG_PATH=/Applications/Xcode_26.3.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib
export DYLD_FALLBACK_LIBRARY_PATH=$LIBCLANG_PATH

CARGO_TARGET_DIR=$PWD/target cargo build --release \
  --bin agave-validator --bin solana-genesis \
  --bin solana-faucet --bin solana-keygen

CARGO_TARGET_DIR=$PWD/dev-bins/target cargo build --release \
  --manifest-path dev-bins/Cargo.toml \
  --bin solana-bench-tps --bin agave-ledger-tool --bin packed-ledger
```

### Live bench (Runs 1 and 4)

```bash
export PATH="$PWD/target/release:$PWD/dev-bins/target/release:$PATH"
export USE_INSTALL=1
rm -rf config
./multinode-demo/setup.sh
./multinode-demo/faucet.sh &
./multinode-demo/bootstrap-validator.sh &
until curl -s -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' \
  http://127.0.0.1:8899 | grep -q '"ok"'; do sleep 1; done

# Run 1: sustained 600s
./multinode-demo/bench-tps.sh \
  --single-destination --use-durable-nonce --sustained \
  --tx-count 10000 --duration 600

# Run 4: aggressive (direct invocation, no wrapper)
solana-bench-tps \
  --url http://127.0.0.1:8899 --faucet 127.0.0.1:9900 \
  --bind-address 127.0.0.1 \
  --client-node-id config/bootstrap-validator/identity.json \
  --thread-batch-sleep-ms 0 --tx-count 20000 --duration 60 \
  -t 16 --tpu-connection-pool-size 8 \
  --single-destination --use-durable-nonce --sustained
```

Grep `Single-destination deposit summary` for ground-truth dps.

### Replay an existing ledger (Runs 2 and 3)

```bash
# Stop the validator first.
agave-ledger-tool --ignore-ulimit-nofile-error \
  --ledger config/bootstrap-validator/ \
  verify --no-snapshot                  # add --skip-verification for Run 3
```

Top-line wall time is in the `ledger processed in …` log line. For
per-slot timing, grep `cost_tracker_stats` and pull `bank_slot=` +
`transaction_count=` plus the timestamp on each line.

### Packed-ledger generator (Run 5)

```bash
# Pure 1-instruction transfer
packed-ledger --ledger /tmp/packed --txs-per-slot 10000 --num-slots 1

# Bench-tps shape (3-instruction with nonce + ComputeBudget)
packed-ledger --ledger /tmp/packed --txs-per-slot 10000 --num-slots 1 --use-nonce

# Optional: realistic PoH for sensitivity check
PACKED_HASHES_PER_TICK=23809 packed-ledger --ledger /tmp/packed ...

# Verify
agave-ledger-tool --ignore-ulimit-nofile-error \
  --ledger /tmp/packed verify --no-snapshot

# Confirm deposits actually moved (not just committed-but-failed):
agave-ledger-tool --ignore-ulimit-nofile-error \
  --ledger /tmp/packed accounts --account <dest_pubkey> --no-account-data
# → Balance: 0.0000<N+1> SOL  for N successful deposits
```
