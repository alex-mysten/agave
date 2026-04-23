//! Build a ledger whose slots are maximally packed with single-destination
//! 1-lamport system transfers. Output can be replayed via
//! `agave-ledger-tool --ledger <path> verify --no-snapshot` to measure the
//! pure execute-path throughput ceiling under full block packing.
//!
//! By default produces 1-instruction transactions (just SystemProgram::Transfer).
//! Pass `--use-nonce` to emit 3-instruction transactions matching the
//! bench-tps production shape:
//!   1. ComputeBudget::SetComputeUnitPrice
//!   2. SystemProgram::AdvanceNonceAccount (injected by Message::new_with_nonce)
//!   3. SystemProgram::Transfer
//! This is heavier per-tx and closer to what a mainnet-shaped deposit workload
//! would see.

use {
    clap::{App, Arg},
    rayon::prelude::*,
    solana_account::Account,
    solana_compute_budget_interface::ComputeBudgetInstruction,
    solana_entry::entry::{next_entry_mut, Entry},
    solana_fee_calculator::FeeCalculator,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::{
        blockstore::{create_new_ledger, Blockstore},
        blockstore_options::LedgerColumnOptions,
        shred::{ProcessShredsStats, ReedSolomonCache, Shred, Shredder},
    },
    solana_message::Message,
    solana_nonce::{
        state::{Data as NonceData, DurableNonce, State as NonceState},
        versions::Versions as NonceVersions,
    },
    solana_runtime::genesis_utils::{create_genesis_config_with_leader, GenesisConfigInfo},
    solana_shred_version::version_from_hash,
    solana_signer::Signer,
    solana_system_interface::{instruction as system_instruction, program as system_program},
    solana_system_transaction as system_transaction,
    solana_transaction::Transaction,
    std::{path::PathBuf, sync::Arc, time::Instant},
};

const NONCE_ACCOUNT_SIZE: usize = 80;
const NONCE_RENT_EXEMPT_LAMPORTS: u64 = 2_000_000; // comfortably above 80-byte rent-exempt min

fn main() {
    agave_logger::setup_with_default("info");

    let matches = App::new("packed-ledger")
        .about("Generate a ledger with densely-packed single-destination deposit slots")
        .arg(
            Arg::with_name("ledger")
                .long("ledger")
                .takes_value(true)
                .required(true)
                .help("Output ledger directory (will be wiped)"),
        )
        .arg(
            Arg::with_name("txs_per_slot")
                .long("txs-per-slot")
                .takes_value(true)
                .default_value("10000"),
        )
        .arg(
            Arg::with_name("num_slots")
                .long("num-slots")
                .takes_value(true)
                .default_value("1"),
        )
        .arg(
            Arg::with_name("use_nonce")
                .long("use-nonce")
                .takes_value(false)
                .help(
                    "Produce bench-tps-style 3-instruction transactions \
                     (ComputeBudget + AdvanceNonceAccount + Transfer) instead of \
                     the default 1-instruction minimal transfers. Requires per-sender \
                     nonce accounts in genesis.",
                ),
        )
        .get_matches();

    let ledger_path: PathBuf = matches.value_of("ledger").unwrap().into();
    let txs_per_slot: usize = matches.value_of("txs_per_slot").unwrap().parse().unwrap();
    let num_slots: u64 = matches.value_of("num_slots").unwrap().parse().unwrap();
    let use_nonce = matches.is_present("use_nonce");

    println!(
        "Building packed ledger at {} — {} txs × {} slot(s), tx shape: {}",
        ledger_path.display(),
        txs_per_slot,
        num_slots,
        if use_nonce {
            "3-instruction (ComputeBudget + AdvanceNonce + Transfer)"
        } else {
            "1-instruction (Transfer only)"
        }
    );

    if use_nonce && num_slots > 1 {
        println!(
            "WARNING: --use-nonce with --num-slots > 1 will not work correctly: \
             each nonce account's durable_nonce advances on first use, so subsequent \
             slots' txs would need fresh nonce hashes. Only 1 slot is supported in \
             nonce mode for now. Proceeding anyway; later slots likely fail replay."
        );
    }

    // Generate sender keypairs, destination, and (if nonce mode) one nonce
    // keypair per sender.
    let t = Instant::now();
    let senders: Vec<Keypair> = (0..txs_per_slot)
        .into_par_iter()
        .map(|_| Keypair::new())
        .collect();
    let dest = Keypair::new();
    let nonce_keypairs: Vec<Keypair> = if use_nonce {
        (0..txs_per_slot)
            .into_par_iter()
            .map(|_| Keypair::new())
            .collect()
    } else {
        Vec::new()
    };
    println!(
        "  generated {} senders + 1 destination ({}){} in {:?}",
        senders.len(),
        dest.pubkey(),
        if use_nonce {
            format!(" + {} nonce keypairs", nonce_keypairs.len())
        } else {
            String::new()
        },
        t.elapsed()
    );

    // Nonce-mode setup: every nonce account is pre-initialized with the same
    // durable_nonce value (derived once from an arbitrary hash). Each nonce's
    // authority is the matching sender. Each tx uses this shared durable nonce
    // as its recent_blockhash; the AdvanceNonceAccount instruction then writes
    // the bank's current last_blockhash into the nonce account's storage.
    let durable_nonce = if use_nonce {
        DurableNonce::from_blockhash(&Hash::new_unique())
    } else {
        DurableNonce::default()
    };

    // Build genesis with primordial accounts.
    let t = Instant::now();
    let leader_pubkey = solana_pubkey::Pubkey::new_unique();
    let GenesisConfigInfo {
        mut genesis_config, ..
    } = create_genesis_config_with_leader(
        1_000_000_000_000,  // mint lamports
        &leader_pubkey,
        1_000_000_000,      // validator stake lamports
    );
    // Minimal PoH work: 64 ticks × 2 hashes = 128 hashes per slot.
    // Override via env var for realistic-PoH comparison: PACKED_HASHES_PER_TICK=12000.
    let hashes_per_tick: u64 = std::env::var("PACKED_HASHES_PER_TICK")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    genesis_config.ticks_per_slot = 64;
    genesis_config.poh_config.hashes_per_tick = Some(hashes_per_tick);
    println!("  hashes_per_tick set to {}", hashes_per_tick);

    // Pre-fund senders. Nonce txs pay fee from sender, so give generous balance.
    let sender_lamports: u64 = 1_000_000_000;
    for sender in &senders {
        genesis_config.accounts.insert(
            sender.pubkey(),
            Account::new(sender_lamports, 0, &system_program::ID),
        );
    }
    // Destination: just needs to exist.
    genesis_config.accounts.insert(
        dest.pubkey(),
        Account::new(1, 0, &system_program::ID),
    );

    if use_nonce {
        // Serialize the nonce state once; per-sender accounts only vary by
        // the authority field, so we build the struct per-sender.
        let fee_calc = FeeCalculator::new(5000);
        for (sender, nonce_kp) in senders.iter().zip(nonce_keypairs.iter()) {
            let state = NonceState::Initialized(NonceData {
                authority: sender.pubkey(),
                durable_nonce,
                fee_calculator: fee_calc.clone(),
            });
            let versions = NonceVersions::new(state);
            let mut data = vec![0u8; NONCE_ACCOUNT_SIZE];
            bincode::serialize_into(&mut data[..], &versions)
                .expect("serialize NonceVersions");
            genesis_config.accounts.insert(
                nonce_kp.pubkey(),
                Account {
                    lamports: NONCE_RENT_EXEMPT_LAMPORTS,
                    data,
                    owner: system_program::ID,
                    executable: false,
                    rent_epoch: 0,
                },
            );
        }
    }
    println!(
        "  populated genesis with {} primordial accounts in {:?}",
        genesis_config.accounts.len(),
        t.elapsed()
    );

    // Materialize the ledger + slot 0.
    let t = Instant::now();
    let slot0_last_hash = create_new_ledger(
        &ledger_path,
        &genesis_config,
        256 * 1024 * 1024,
        LedgerColumnOptions::default(),
    )
    .expect("create_new_ledger failed");
    println!(
        "  wrote genesis + slot 0 in {:?} (slot 0 last hash: {})",
        t.elapsed(),
        slot0_last_hash
    );

    let blockstore = Blockstore::open(&ledger_path).unwrap();
    let ticks_per_slot = genesis_config.ticks_per_slot;
    let hashes_per_tick = genesis_config.poh_config.hashes_per_tick.unwrap_or(0);
    assert!(
        hashes_per_tick >= 2,
        "generator requires hashes_per_tick >= 2"
    );
    println!(
        "  slot shape: ticks_per_slot={}, hashes_per_tick={}",
        ticks_per_slot, hashes_per_tick
    );

    let leader_keypair = Arc::new(Keypair::new());
    let version = version_from_hash(&slot0_last_hash);
    let reed_solomon_cache = ReedSolomonCache::default();

    let mut current_hash = slot0_last_hash;
    let mut parent_slot: u64 = 0;
    let mut tx_blockhash = if use_nonce {
        *durable_nonce.as_hash()
    } else {
        slot0_last_hash
    };

    for slot in 1..=num_slots {
        let t_slot = Instant::now();

        let t = Instant::now();
        let txs: Vec<Transaction> = if use_nonce {
            senders
                .par_iter()
                .zip(nonce_keypairs.par_iter())
                .map(|(sender, nonce_kp)| build_nonced_tx(sender, nonce_kp, &dest.pubkey(), tx_blockhash))
                .collect()
        } else {
            senders
                .par_iter()
                .map(|sender| {
                    system_transaction::transfer(sender, &dest.pubkey(), 1, tx_blockhash)
                })
                .collect()
        };
        let build_us = t.elapsed().as_micros();

        let t = Instant::now();
        let mut entries: Vec<Entry> = Vec::with_capacity(ticks_per_slot as usize + 1);
        entries.push(next_entry_mut(&mut current_hash, 1, txs));
        entries.push(next_entry_mut(
            &mut current_hash,
            hashes_per_tick - 1,
            vec![],
        ));
        for _ in 1..ticks_per_slot {
            entries.push(next_entry_mut(&mut current_hash, hashes_per_tick, vec![]));
        }
        let entries_us = t.elapsed().as_micros();
        let slot_last_hash = entries.last().unwrap().hash;

        let t = Instant::now();
        let chained_merkle_root = blockstore
            .get_last_shred_merkle_root(parent_slot)
            .expect("get_last_shred_merkle_root failed")
            .unwrap_or_else(Hash::new_unique);
        let shredder = Shredder::new(slot, parent_slot, 0, version).unwrap();
        let (data_shreds, _coding_shreds): (Vec<Shred>, Vec<Shred>) = shredder
            .entries_to_merkle_shreds_for_tests(
                &leader_keypair,
                &entries,
                true,
                chained_merkle_root,
                0,
                0,
                &reed_solomon_cache,
                &mut ProcessShredsStats::default(),
            );
        let shreds = data_shreds;
        let shred_count = shreds.len();
        let shred_us = t.elapsed().as_micros();

        let t = Instant::now();
        blockstore
            .insert_shreds(shreds, None, false)
            .expect("insert_shreds failed");
        blockstore
            .set_roots(std::iter::once(&slot))
            .expect("set_roots failed");
        let insert_us = t.elapsed().as_micros();

        println!(
            "  slot {}: {} entries, {} shreds — build_txs {}us, build_entries {}us, \
             shred {}us, insert {}us; total {:?}",
            slot,
            entries.len(),
            shred_count,
            build_us,
            entries_us,
            shred_us,
            insert_us,
            t_slot.elapsed()
        );

        parent_slot = slot;
        if !use_nonce {
            tx_blockhash = slot_last_hash;
        }
        // For nonce mode: after first slot, all nonces are advanced to the bank's
        // last_blockhash, which we don't compute here. Don't use this for slot 2+.
    }

    drop(blockstore);
    println!("Done. Ledger at {}.", ledger_path.display());
    println!(
        "Run: agave-ledger-tool --ignore-ulimit-nofile-error --ledger {} verify --no-snapshot",
        ledger_path.display()
    );
}

fn build_nonced_tx(
    sender: &Keypair,
    nonce_kp: &Keypair,
    dest: &solana_pubkey::Pubkey,
    nonce_hash: Hash,
) -> Transaction {
    // Instructions in the order `Message::new_with_nonce` expects:
    //   the first position will be filled by AdvanceNonceAccount (it injects it),
    //   then our extras follow.
    let cu_price_ix = ComputeBudgetInstruction::set_compute_unit_price(1);
    let transfer_ix = system_instruction::transfer(&sender.pubkey(), dest, 1);
    let msg = Message::new_with_nonce(
        vec![cu_price_ix, transfer_ix],
        Some(&sender.pubkey()),
        &nonce_kp.pubkey(),
        &sender.pubkey(), // sender IS the nonce authority
    );
    Transaction::new(&[sender], msg, nonce_hash)
}
