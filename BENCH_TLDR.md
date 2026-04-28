# Solana Single-Destination Throughput — TL;DR

Best numbers from `SINGLE_DESTINATION_BENCH.md`, `SOLANA_BENCH_LOG.md`,
and `TOKEN_BENCH_LOG.md`. All on Apple Silicon Mac VM, agave-validator
4.1.0-alpha.0, single-node dev cluster, single-destination 1-unit
deposits.

## Headline numbers

| measurement | SOL | SPL Token | Token vs SOL |
|---|---:|---:|---:|
| **Replay ceiling, plain transfer** (1-ix) | **50,420 TPS** at 27,000 txs/slot | **~37,400 TPS** at 21,260 txs/slot | -25% |
| **Replay ceiling, bench-tps shape** (3-ix nonced; token: 4-ix) | **41,576 TPS** at 19,000 txs/slot | **~27,600 TPS** at 16,100 txs/slot | -34% |
| **Live, 600s sustained, fresh validator** | **104 dps** | **94 dps** | -10% |

## Summary

Agave on a single Apple Silicon Mac VM commits roughly **50,000
native SOL transfers per second** and **~37,000 SPL Token
(stablecoin-shape) transfers per second** to a single hot destination
account when the slot is maximally packed at the per-writable-account
cost limit (40 M CU = 40% of the post-SIMD-0286 100 M block-units
cap), bounded by per-account cost rather than unique-account count.
Adding the bench-tps-shape `ComputeBudget::SetComputeUnitPrice` +
`AdvanceNonceAccount` instructions drops these to ~42 k SOL and
~28 k token. Token transfer is **25–35 % slower than SOL at the
replay ceiling** — heavier per-tx execute (SBF program + 165-byte
SPL `Account` state load/save vs lamports-only system updates), an
extra writable account per tx. **Live throughput is dramatically
lower: ~100 deposits/sec sustained over 600 s for either mode**,
around 0.25 % of the replay ceiling, with the bottleneck far
upstream of execute (banking-stage scheduler, single-fee-payer
write-lock churn, sender pacing, durable-nonce-account contention,
QUIC stream limits). Those factors don't materially differ between
SOL and token modes, which is why the 25–35 % execute-cost gap
shrinks to ~10 % at live scale. The live ceiling is sender-bound
and single-client; multi-client / improved scheduler is the lever
to lift it.
