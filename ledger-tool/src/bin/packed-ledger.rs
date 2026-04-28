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
//!
//! Pass `--use-token` to switch from native SOL transfers to single-destination
//! SPL Token transfers (simulates stablecoin transfer performance). Genesis
//! pre-creates a mint, one initialized token account per sender (funded with
//! per-tx supply), and one dest token account; each tx is then a 2-instruction
//! transaction:
//!   1. ComputeBudget::SetComputeUnitLimit (so per-tx CU doesn't default to 200k
//!      and starve the writable-account-units budget)
//!   2. spl_token::Transfer (1 unit, source -> dest, sender as authority)
//! Combinable with `--use-nonce` to get the bench-tps-shape 3-instruction
//! token tx (CB + AdvanceNonce + TokenTransfer).

use {
    clap::{App, Arg},
    rayon::prelude::*,
    solana_account::Account,
    solana_compute_budget_interface::ComputeBudgetInstruction,
    solana_entry::entry::{next_entry_mut, Entry},
    solana_fee_calculator::FeeCalculator,
    solana_hash::Hash,
    solana_instruction::Instruction,
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
    solana_program_pack::Pack,
    solana_pubkey::Pubkey,
    solana_runtime::genesis_utils::{create_genesis_config_with_leader, GenesisConfigInfo},
    solana_shred_version::version_from_hash,
    solana_signer::Signer,
    solana_system_interface::{instruction as system_instruction, program as system_program},
    solana_system_transaction as system_transaction,
    solana_transaction::Transaction,
    spl_generic_token::token as spl_token_program,
    spl_token_interface::state::{Account as TokenAccount, AccountState, Mint},
    std::{path::PathBuf, sync::Arc, time::Instant},
};

const NONCE_ACCOUNT_SIZE: usize = 80;
const NONCE_RENT_EXEMPT_LAMPORTS: u64 = 2_000_000; // comfortably above 80-byte rent-exempt min

// SPL Token Transfer real CU ~4500. Set the limit a bit above to leave headroom
// for sig + write-locks accounting in the per-tx programs_execution_cost field
// (which the cost model uses to enforce writable-account-units / block-units).
// Setting this explicitly is essential — without it the default per-ix
// 200_000-CU budget would let only ~120 single-dest token transfers fit under
// the per-block writable-account budget (24M).
const TOKEN_TX_COMPUTE_UNIT_LIMIT: u32 = 6_000;
// 6-decimal mint, e.g. USDC-shape. Funded supply per sender comfortably
// exceeds num_slots * 1-unit/slot.
const TOKEN_DECIMALS: u8 = 6;
const PER_SENDER_TOKEN_SUPPLY: u64 = 1_000_000;
// Dev cluster genesis is created with `Rent::default()` which sets
// `lamports_per_byte_year=0`, so `minimum_balance(82)` returns 0. Zero-lamport
// accounts are skipped by `is_loadable()` (and runtime filters them in many
// places), so we explicitly fund mint + token accounts with a small fixed
// balance to keep them visible/loadable. 2_000_000 lamports is the same
// conservative floor we use for nonce accounts above.
const TOKEN_ACCOUNT_LAMPORTS_FLOOR: u64 = 2_000_000;

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
        .arg(
            Arg::with_name("use_token")
                .long("use-token")
                .takes_value(false)
                .help(
                    "Generate SPL Token (Tokenkeg... program) Transfer txs \
                     instead of native SOL transfers. Stablecoin-shape workload: \
                     hot writable account is the dest token account, not the \
                     dest system account. Adds a ComputeBudget::SetComputeUnitLimit \
                     instruction so per-tx CU doesn't default to 200k. \
                     Stackable with --use-nonce.",
                ),
        )
        .get_matches();

    let ledger_path: PathBuf = matches.value_of("ledger").unwrap().into();
    let txs_per_slot: usize = matches.value_of("txs_per_slot").unwrap().parse().unwrap();
    let num_slots: u64 = matches.value_of("num_slots").unwrap().parse().unwrap();
    let use_nonce = matches.is_present("use_nonce");
    let use_token = matches.is_present("use_token");

    let tx_shape = match (use_token, use_nonce) {
        (false, false) => "1-instruction (SOL Transfer only)",
        (false, true) => "3-instruction (ComputeBudget + AdvanceNonce + SOL Transfer)",
        (true, false) => "2-instruction (ComputeBudget + Token Transfer)",
        (true, true) => "4-instruction (ComputeBudget + AdvanceNonce + ComputeBudgetLimit + Token Transfer)",
    };
    println!(
        "Building packed ledger at {} — {} txs × {} slot(s), tx shape: {}",
        ledger_path.display(),
        txs_per_slot,
        num_slots,
        tx_shape,
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
    // keypair per sender. In token mode also: one mint keypair, one token-account
    // keypair per sender, and one dest token-account keypair.
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
    let (mint_kp, sender_token_kps, dest_token_kp) = if use_token {
        let mint_kp = Keypair::new();
        let sender_token_kps: Vec<Keypair> = (0..txs_per_slot)
            .into_par_iter()
            .map(|_| Keypair::new())
            .collect();
        let dest_token_kp = Keypair::new();
        (Some(mint_kp), sender_token_kps, Some(dest_token_kp))
    } else {
        (None, Vec::new(), None)
    };
    println!(
        "  generated {} senders + 1 destination ({}){}{} in {:?}",
        senders.len(),
        dest.pubkey(),
        if use_nonce {
            format!(" + {} nonce keypairs", nonce_keypairs.len())
        } else {
            String::new()
        },
        if use_token {
            format!(
                " + mint {} + {} sender token accounts + dest token account {}",
                mint_kp.as_ref().unwrap().pubkey(),
                sender_token_kps.len(),
                dest_token_kp.as_ref().unwrap().pubkey(),
            )
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

    if use_token {
        // Inject the SPL Token program (and friends) into genesis. The default
        // create_genesis_config_with_leader_ex pulls in the native SOL mint
        // account but not the Tokenkeg... program ELF, so without this our
        // token transfer txs would fail at execution time even though the
        // token state accounts deserialize fine.
        //
        // create_genesis_config_with_leader uses `Rent::free()` (zero rate),
        // which makes `bpf_loader_upgradeable_program_accounts` produce
        // program accounts with lamports=0. Zero-lamport accounts are not
        // loadable, so the bank's tx executor sees `ProgramAccountNotFound`
        // and every token transfer fails (committed-but-failed). Pass a
        // realistic Rent to compute proper rent-exempt minimums for the
        // program/programdata accounts.
        let program_rent = solana_rent::Rent::default();
        for (program_id, account) in solana_program_binaries::spl_programs(&program_rent) {
            genesis_config
                .accounts
                .insert(program_id, Account::from(account));
        }
        let mint_kp = mint_kp.as_ref().unwrap();
        let dest_token_kp = dest_token_kp.as_ref().unwrap();
        let token_program_id = spl_token_program::id();
        let mint_rent = genesis_config
            .rent
            .minimum_balance(Mint::LEN)
            .max(TOKEN_ACCOUNT_LAMPORTS_FLOOR);
        let token_acc_rent = genesis_config
            .rent
            .minimum_balance(TokenAccount::LEN)
            .max(TOKEN_ACCOUNT_LAMPORTS_FLOOR);

        // Mint: pre-initialized, supply = total funded across all sender accounts,
        // no freeze authority, mint authority set to a throwaway pubkey since we
        // never mint at runtime in this generator.
        let total_supply: u64 = PER_SENDER_TOKEN_SUPPLY
            .checked_mul(txs_per_slot as u64)
            .expect("mint supply overflow");
        let mint_state = Mint {
            mint_authority: solana_program_option::COption::Some(Pubkey::new_unique()),
            supply: total_supply,
            decimals: TOKEN_DECIMALS,
            is_initialized: true,
            freeze_authority: solana_program_option::COption::None,
        };
        let mut mint_data = vec![0u8; Mint::LEN];
        mint_state.pack_into_slice(&mut mint_data);
        genesis_config.accounts.insert(
            mint_kp.pubkey(),
            Account {
                lamports: mint_rent,
                data: mint_data,
                owner: token_program_id,
                executable: false,
                rent_epoch: 0,
            },
        );

        // Each sender's token account: owned (in SPL sense) by the sender keypair,
        // funded with PER_SENDER_TOKEN_SUPPLY units. Account.owner (Solana account
        // owner) is the token program; SPL Account.owner is the sender keypair.
        let mint_pubkey = mint_kp.pubkey();
        let sender_token_accounts: Vec<(Pubkey, Account)> = senders
            .par_iter()
            .zip(sender_token_kps.par_iter())
            .map(|(sender, tok_kp)| {
                let state = TokenAccount {
                    mint: mint_pubkey,
                    owner: sender.pubkey(),
                    amount: PER_SENDER_TOKEN_SUPPLY,
                    delegate: solana_program_option::COption::None,
                    state: AccountState::Initialized,
                    is_native: solana_program_option::COption::None,
                    delegated_amount: 0,
                    close_authority: solana_program_option::COption::None,
                };
                let mut data = vec![0u8; TokenAccount::LEN];
                state.pack_into_slice(&mut data);
                (
                    tok_kp.pubkey(),
                    Account {
                        lamports: token_acc_rent,
                        data,
                        owner: token_program_id,
                        executable: false,
                        rent_epoch: 0,
                    },
                )
            })
            .collect();
        for (key, acc) in sender_token_accounts {
            genesis_config.accounts.insert(key, acc);
        }

        // Dest token account: owned by `dest`, balance 0.
        let dest_state = TokenAccount {
            mint: mint_pubkey,
            owner: dest.pubkey(),
            amount: 0,
            delegate: solana_program_option::COption::None,
            state: AccountState::Initialized,
            is_native: solana_program_option::COption::None,
            delegated_amount: 0,
            close_authority: solana_program_option::COption::None,
        };
        let mut dest_data = vec![0u8; TokenAccount::LEN];
        dest_state.pack_into_slice(&mut dest_data);
        genesis_config.accounts.insert(
            dest_token_kp.pubkey(),
            Account {
                lamports: token_acc_rent,
                data: dest_data,
                owner: token_program_id,
                executable: false,
                rent_epoch: 0,
            },
        );
        println!(
            "  token mode: mint={}, supply={}, decimals={}, dest_token_account={}",
            mint_pubkey, total_supply, TOKEN_DECIMALS, dest_token_kp.pubkey(),
        );
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
        let txs: Vec<Transaction> = match (use_token, use_nonce) {
            (false, false) => senders
                .par_iter()
                .map(|sender| {
                    system_transaction::transfer(sender, &dest.pubkey(), 1, tx_blockhash)
                })
                .collect(),
            (false, true) => senders
                .par_iter()
                .zip(nonce_keypairs.par_iter())
                .map(|(sender, nonce_kp)| build_nonced_tx(sender, nonce_kp, &dest.pubkey(), tx_blockhash))
                .collect(),
            (true, false) => {
                let dest_token = dest_token_kp.as_ref().unwrap().pubkey();
                senders
                    .par_iter()
                    .zip(sender_token_kps.par_iter())
                    .map(|(sender, sender_tok_kp)| {
                        build_token_transfer_tx(
                            sender,
                            &sender_tok_kp.pubkey(),
                            &dest_token,
                            tx_blockhash,
                        )
                    })
                    .collect()
            }
            (true, true) => {
                let dest_token = dest_token_kp.as_ref().unwrap().pubkey();
                senders
                    .par_iter()
                    .zip(sender_token_kps.par_iter())
                    .zip(nonce_keypairs.par_iter())
                    .map(|((sender, sender_tok_kp), nonce_kp)| {
                        build_nonced_token_transfer_tx(
                            sender,
                            &sender_tok_kp.pubkey(),
                            &dest_token,
                            nonce_kp,
                            tx_blockhash,
                        )
                    })
                    .collect()
            }
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
    dest: &Pubkey,
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

fn token_transfer_ix(source: &Pubkey, dest: &Pubkey, authority: &Pubkey) -> Instruction {
    spl_token_interface::instruction::transfer(
        &spl_token_program::id(),
        source,
        dest,
        authority,
        &[],
        1,
    )
    .expect("build spl token transfer")
}

fn build_token_transfer_tx(
    sender: &Keypair,
    sender_token_acc: &Pubkey,
    dest_token_acc: &Pubkey,
    blockhash: Hash,
) -> Transaction {
    let cu_limit_ix = ComputeBudgetInstruction::set_compute_unit_limit(TOKEN_TX_COMPUTE_UNIT_LIMIT);
    let transfer_ix = token_transfer_ix(sender_token_acc, dest_token_acc, &sender.pubkey());
    let msg = Message::new(&[cu_limit_ix, transfer_ix], Some(&sender.pubkey()));
    Transaction::new(&[sender], msg, blockhash)
}

fn build_nonced_token_transfer_tx(
    sender: &Keypair,
    sender_token_acc: &Pubkey,
    dest_token_acc: &Pubkey,
    nonce_kp: &Keypair,
    nonce_hash: Hash,
) -> Transaction {
    let cu_price_ix = ComputeBudgetInstruction::set_compute_unit_price(1);
    let cu_limit_ix = ComputeBudgetInstruction::set_compute_unit_limit(TOKEN_TX_COMPUTE_UNIT_LIMIT);
    let transfer_ix = token_transfer_ix(sender_token_acc, dest_token_acc, &sender.pubkey());
    let msg = Message::new_with_nonce(
        vec![cu_price_ix, cu_limit_ix, transfer_ix],
        Some(&sender.pubkey()),
        &nonce_kp.pubkey(),
        &sender.pubkey(),
    );
    Transaction::new(&[sender], msg, nonce_hash)
}
