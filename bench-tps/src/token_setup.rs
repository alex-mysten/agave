//! Token-mode setup for bench-tps `--use-token`.
//!
//! Creates a fresh SPL Mint, one SPL Token Account per sender (funded with
//! tokens), and one dest SPL Token Account on the live cluster, using the same
//! retry-until-confirmed batch infrastructure that nonce setup uses. Output is
//! a `TokenContext` consumed by the bench's tx-generation path.

use {
    log::*,
    rayon::prelude::*,
    solana_compute_budget_interface::ComputeBudgetInstruction,
    solana_keypair::Keypair,
    solana_message::Message,
    solana_program_pack::Pack,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_signer::Signer,
    solana_system_interface::instruction as system_instruction,
    solana_tps_client::*,
    solana_transaction::Transaction,
    spl_generic_token::token as spl_token_program,
    spl_token_interface::state::{Account as TokenAccountState, Mint as MintState},
    std::{
        sync::Arc,
        thread::sleep,
        time::{Duration, Instant},
    },
};

/// Context handed to the bench's tx-generation path. One per run.
pub struct TokenContext {
    pub mint: Pubkey,
    pub token_program: Pubkey,
    /// One token account per gen_keypair, indexed identically. Whether a
    /// gen_keypair ends up acting as a source in any chunk or as the fixed
    /// dest, its token account is at the same index. Sources read from this
    /// to build Transfer.source; dest reads from this to track delta.
    pub per_keypair_token_accounts: Vec<Keypair>,
    /// Index of the single fixed destination within `per_keypair_token_accounts`.
    pub dest_index: usize,
    pub decimals: u8,
}

impl TokenContext {
    pub fn dest_token_account(&self) -> &Keypair {
        &self.per_keypair_token_accounts[self.dest_index]
    }
}

/// Per-sender funded amount. Each tx transfers 1 unit; this comfortably
/// covers a many-hour run at any reasonable banking-stage commit rate.
const PER_SENDER_TOKEN_AMOUNT: u64 = 1_000_000_000;
const TOKEN_DECIMALS: u8 = 6;
const SETUP_COMPUTE_UNIT_LIMIT: u32 = 60_000;

/// Build the mint and one token account per gen_keypair on the cluster.
///
/// Creates `gen_keypairs.len()` token accounts, each owned (SPL-side) by the
/// matching gen_keypair. `dest_index` is the position of the single-destination
/// keypair; its token account is treated as the dest for the bench's deposit
/// flow. All token accounts are mint_to-funded, including the dest's; we
/// snapshot the dest amount at run-start to compute the delta accurately.
///
/// Funder pays for everything (mint creation, every token account's
/// rent-exempt minimum, mint_to txs). Senders' SOL balance is not touched
/// here — they keep it for paying tx fees during the bench.
pub fn setup_token_accounts<T: 'static + TpsClient + Send + Sync + ?Sized>(
    client: Arc<T>,
    funder: &Keypair,
    gen_keypairs: &[Keypair],
    dest_index: usize,
) -> TokenContext {
    let token_program = spl_token_program::id();
    let mint_keypair = Keypair::new();
    info!(
        "Token setup: mint={} ({} keypairs incl. dest at index {})",
        mint_keypair.pubkey(),
        gen_keypairs.len(),
        dest_index,
    );

    let mint_rent = client
        .get_minimum_balance_for_rent_exemption(MintState::LEN)
        .expect("get_minimum_balance_for_rent_exemption(Mint)");
    let token_acc_rent = client
        .get_minimum_balance_for_rent_exemption(TokenAccountState::LEN)
        .expect("get_minimum_balance_for_rent_exemption(TokenAccount)");
    info!(
        "Token setup: mint rent={} lamports, token-account rent={} lamports",
        mint_rent, token_acc_rent
    );

    create_mint(
        client.as_ref(),
        funder,
        &mint_keypair,
        mint_rent,
        &token_program,
        TOKEN_DECIMALS,
    );

    let per_keypair_token_accounts: Vec<Keypair> =
        (0..gen_keypairs.len()).map(|_| Keypair::new()).collect();

    create_and_init_token_accounts(
        client.clone(),
        funder,
        &mint_keypair.pubkey(),
        gen_keypairs,
        &per_keypair_token_accounts,
        token_acc_rent,
        &token_program,
    );

    mint_to_accounts(
        client.clone(),
        funder,
        &mint_keypair,
        &per_keypair_token_accounts,
        &token_program,
    );

    info!(
        "Token setup: complete. mint={}, dest_token_account={}",
        mint_keypair.pubkey(),
        per_keypair_token_accounts[dest_index].pubkey()
    );

    TokenContext {
        mint: mint_keypair.pubkey(),
        token_program,
        per_keypair_token_accounts,
        dest_index,
        decimals: TOKEN_DECIMALS,
    }
}

fn latest_blockhash<T: TpsClient + ?Sized>(client: &T) -> solana_hash::Hash {
    loop {
        match client.get_latest_blockhash() {
            Ok(h) => return h,
            Err(err) => {
                warn!("get_latest_blockhash failed during token setup: {err:?}");
                sleep(Duration::from_secs(1));
            }
        }
    }
}

fn create_mint<T: TpsClient + ?Sized>(
    client: &T,
    funder: &Keypair,
    mint: &Keypair,
    mint_rent: u64,
    token_program: &Pubkey,
    decimals: u8,
) {
    let create_ix = system_instruction::create_account(
        &funder.pubkey(),
        &mint.pubkey(),
        mint_rent,
        MintState::LEN as u64,
        token_program,
    );
    let init_ix = spl_token_interface::instruction::initialize_mint(
        token_program,
        &mint.pubkey(),
        &funder.pubkey(),
        None,
        decimals,
    )
    .expect("build initialize_mint");

    let cu_limit_ix = ComputeBudgetInstruction::set_compute_unit_limit(SETUP_COMPUTE_UNIT_LIMIT);
    let msg = Message::new(
        &[cu_limit_ix, create_ix, init_ix],
        Some(&funder.pubkey()),
    );

    for attempt in 0..30 {
        let blockhash = latest_blockhash(client);
        let tx = Transaction::new(&[funder, mint], msg.clone(), blockhash);
        match client.send_transaction(tx.into()) {
            Ok(sig) => {
                info!(
                    "Mint create+init tx submitted: signature={sig} (attempt {})",
                    attempt + 1
                );
            }
            Err(err) => {
                warn!("Mint create tx send failed: {err:?} (attempt {})", attempt + 1);
            }
        }
        // Poll for the mint account to exist on-chain.
        for _ in 0..20 {
            sleep(Duration::from_millis(500));
            if let Ok(acc) = client.get_account(&mint.pubkey()) {
                if acc.data.len() == MintState::LEN && acc.owner == *token_program {
                    info!("Mint confirmed on-chain at {}", mint.pubkey());
                    return;
                }
            }
        }
        warn!("Mint not yet visible on-chain; retrying");
    }
    panic!("Failed to create mint after 30 attempts");
}

fn create_and_init_token_accounts<T: 'static + TpsClient + Send + Sync + ?Sized>(
    client: Arc<T>,
    _funder: &Keypair,
    mint: &Pubkey,
    owners: &[Keypair],
    token_accounts: &[Keypair],
    token_acc_rent: u64,
    token_program: &Pubkey,
) {
    assert_eq!(owners.len(), token_accounts.len());
    let total = token_accounts.len();
    info!("Token setup: creating {} token accounts", total);

    // Each owner pays for and signs their own token-account create+init. This
    // avoids a single-fee-payer write-lock bottleneck at the funder; with
    // thousands of account creates, parallelism drops to one tx per slot
    // otherwise.
    type TokenSetupTx<'a> = (&'a Keypair, &'a Keypair, Transaction);

    let pairs: Vec<(&Keypair, &Keypair)> = token_accounts
        .iter()
        .zip(owners.iter())
        .map(|(tok, owner)| (tok, owner))
        .collect();

    // Send in chunks; each chunk re-signed with fresh blockhash and retried
    // until all accounts exist on-chain.
    const CHUNK_SIZE: usize = 200;
    let mut remaining: Vec<(&Keypair, &Keypair)> = pairs;

    let mut overall_attempts: usize = 0;
    while !remaining.is_empty() {
        overall_attempts += 1;
        info!(
            "Token-account create: {} remaining, attempt {}",
            remaining.len(),
            overall_attempts
        );
        let blockhash = latest_blockhash(client.as_ref());
        let txs: Vec<TokenSetupTx<'_>> = remaining
            .par_iter()
            .map(|(token_kp, owner_kp)| {
                let create_ix = system_instruction::create_account(
                    &owner_kp.pubkey(),
                    &token_kp.pubkey(),
                    token_acc_rent,
                    TokenAccountState::LEN as u64,
                    token_program,
                );
                let init_ix = spl_token_interface::instruction::initialize_account(
                    token_program,
                    &token_kp.pubkey(),
                    mint,
                    &owner_kp.pubkey(),
                )
                .expect("build initialize_account");
                let cu_limit_ix =
                    ComputeBudgetInstruction::set_compute_unit_limit(SETUP_COMPUTE_UNIT_LIMIT);
                let msg = Message::new(
                    &[cu_limit_ix, create_ix, init_ix],
                    Some(&owner_kp.pubkey()),
                );
                let tx = Transaction::new(&[*owner_kp, *token_kp], msg, blockhash);
                (*token_kp, *owner_kp, tx)
            })
            .collect();

        for chunk in txs.chunks(CHUNK_SIZE) {
            let batch: Vec<_> = chunk.iter().map(|(_, _, tx)| tx.clone().into()).collect();
            if let Err(err) = client.send_batch(batch) {
                warn!("send_batch failed for token-account create chunk: {err:?}");
            }
        }

        // Wait, then check which exist.
        sleep(Duration::from_secs(2));
        let still_pending: Vec<(&Keypair, &Keypair)> = remaining
            .par_iter()
            .filter_map(|(token_kp, owner_kp)| {
                match client.get_account(&token_kp.pubkey()) {
                    Ok(acc)
                        if acc.data.len() == TokenAccountState::LEN
                            && acc.owner == *token_program =>
                    {
                        None
                    }
                    _ => Some((*token_kp, *owner_kp)),
                }
            })
            .collect();

        let confirmed = remaining.len().saturating_sub(still_pending.len());
        info!(
            "Token-account create: confirmed {} this round, {} still pending",
            confirmed,
            still_pending.len()
        );
        remaining = still_pending;

        assert!(
            overall_attempts < 30,
            "Token-account creation did not converge in 30 attempts"
        );
    }
    info!("Token setup: all {} token accounts confirmed", total);
}

/// Mint tokens to every account, idempotently.
///
/// `mint_to` itself is *not* idempotent — calling it twice with the same args
/// just adds twice. So instead of the "send + sleep + retry-by-account-state"
/// pattern used elsewhere, we send each tx via `send_transaction`, hold its
/// `Signature`, and only consider an account funded once we've observed a
/// committed status for *that exact signature*. Only signatures that expire
/// (None status past blockhash validity) are retried.
fn mint_to_accounts<T: 'static + TpsClient + Send + Sync + ?Sized>(
    client: Arc<T>,
    funder: &Keypair,
    mint: &Keypair,
    token_accounts: &[Keypair],
    token_program: &Pubkey,
) {
    info!(
        "Token setup: minting {} units to each of {} token accounts",
        PER_SENDER_TOKEN_AMOUNT,
        token_accounts.len()
    );

    let mut remaining: Vec<&Keypair> = token_accounts.iter().collect();
    let mut overall_attempts: usize = 0;
    const BLOCKHASH_VALIDITY: Duration = Duration::from_secs(75); // ~150 slots * 0.5s slack

    while !remaining.is_empty() {
        overall_attempts += 1;
        let blockhash = latest_blockhash(client.as_ref());
        let send_start = Instant::now();
        info!(
            "mint_to: attempt {}, sending {} txs",
            overall_attempts,
            remaining.len()
        );

        // Build + sign all txs in parallel; extract per-tx signature so we can
        // confirm each individually. Then push them through send_batch (fast)
        // rather than per-tx send_transaction (slow at >1000-keypair scale).
        const SEND_CHUNK: usize = 200;
        let signed: Vec<(&Keypair, Transaction, Signature)> = remaining
            .par_iter()
            .map(|tok_kp| {
                let mint_to_ix = spl_token_interface::instruction::mint_to(
                    token_program,
                    &mint.pubkey(),
                    &tok_kp.pubkey(),
                    &funder.pubkey(),
                    &[],
                    PER_SENDER_TOKEN_AMOUNT,
                )
                .expect("build mint_to");
                let cu_limit_ix =
                    ComputeBudgetInstruction::set_compute_unit_limit(SETUP_COMPUTE_UNIT_LIMIT);
                let msg = Message::new(
                    &[cu_limit_ix, mint_to_ix],
                    Some(&funder.pubkey()),
                );
                let tx = Transaction::new(&[funder], msg, blockhash);
                let sig = tx.signatures[0];
                (*tok_kp, tx, sig)
            })
            .collect();

        let mut pending: Vec<(&Keypair, Signature)> =
            signed.iter().map(|(kp, _, sig)| (*kp, *sig)).collect();

        // Submit in batches.
        let send_txs: Vec<Transaction> = signed.into_iter().map(|(_, tx, _)| tx).collect();
        for chunk in send_txs.chunks(SEND_CHUNK) {
            let batch: Vec<_> = chunk.iter().map(|tx| tx.clone().into()).collect();
            if let Err(err) = client.send_batch(batch) {
                warn!("send_batch failed for mint_to chunk: {err:?}");
            }
        }

        // Poll signatures until each is committed, or blockhash window has
        // elapsed (in which case we drop them and retry with a fresh blockhash).
        let mut still_pending: Vec<&Keypair> = Vec::new();
        loop {
            let mut next_pending: Vec<(&Keypair, Signature)> = Vec::with_capacity(pending.len());
            for (tok_kp, sig) in pending.drain(..) {
                match client.get_signature_status(&sig) {
                    Ok(Some(Ok(()))) => {
                        // Confirmed and successful; do not retry under any circumstances.
                    }
                    Ok(Some(Err(err))) => {
                        warn!(
                            "mint_to to {} hard-failed: {err:?}; will retry",
                            tok_kp.pubkey()
                        );
                        still_pending.push(tok_kp);
                    }
                    _ => next_pending.push((tok_kp, sig)),
                }
            }
            pending = next_pending;
            if pending.is_empty() {
                break;
            }
            if send_start.elapsed() > BLOCKHASH_VALIDITY {
                // Blockhash expired; drain remaining as still_pending for retry.
                for (tok_kp, sig) in pending.drain(..) {
                    warn!(
                        "mint_to to {} not confirmed within {:?} (sig={}); blockhash expired, retrying",
                        tok_kp.pubkey(),
                        BLOCKHASH_VALIDITY,
                        sig,
                    );
                    still_pending.push(tok_kp);
                }
                break;
            }
            sleep(Duration::from_millis(500));
        }

        let confirmed = remaining.len().saturating_sub(still_pending.len());
        info!(
            "mint_to: confirmed {} this round, {} still pending (attempt {})",
            confirmed,
            still_pending.len(),
            overall_attempts
        );
        remaining = still_pending;

        assert!(
            overall_attempts < 10,
            "mint_to did not converge in 10 attempts; aborting"
        );
    }
    info!("Token setup: all senders funded");
}

/// Read the current SPL token amount from a token account on the cluster.
pub fn read_token_amount<T: TpsClient + ?Sized>(client: &T, token_account: &Pubkey) -> u64 {
    match client.get_account(token_account) {
        Ok(acc) if acc.data.len() == TokenAccountState::LEN => {
            TokenAccountState::unpack_from_slice(&acc.data)
                .map(|s| s.amount)
                .unwrap_or(0)
        }
        _ => 0,
    }
}
