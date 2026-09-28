#![cfg(test)]
#![allow(clippy::arithmetic_side_effects)]

//! Differential tests for the result-identical `SOLANA_VM_*` execution switches
//! (`SOLANA_VM_HEAP_ZERO_OPT`, `SOLANA_VM_PDA_CACHE`, `SOLANA_VM_SER_POOL`,
//! `SOLANA_VM_CHEAP_TIMERS`).
//!
//! Real mainnet programs are executed through the full SVM transaction pipeline with the
//! production syscall registry, once with every switch off and once per switch setting, and
//! everything a transaction produces is compared: status, logs, return data, CPI trace,
//! compute units, fee details, rollback and post-execution accounts.
//!
//! - `spl_programs_identical_with_vm_opts` uses the SPL binaries shipped in
//!   `solana-program-binaries` (p-token, Token-2022, ATA) and runs everywhere.
//! - `recorded_mainnet_transactions_identical_with_vm_opts` replays recorded mainnet
//!   transactions (e.g. the PumpSwap swaps of the vm_step_cost study) from a plain-text corpus
//!   exported by `vm_step_cost/scripts/export_agave_diff.py`; it runs only when
//!   `SOLANA_VM_OPTS_REPLAY_DIR` points at such a corpus.

use {
    crate::mock_bank::{MockBankCallback, MockForkGraph, register_builtins},
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_clock::{Clock, Epoch, Slot},
    solana_compute_budget_instruction::instructions_processor::process_compute_budget_instructions,
    solana_hash::Hash,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_message::{
        Message, MessageHeader, SimpleAddressLoader, VersionedMessage,
        compiled_instruction::CompiledInstruction,
        v0::{self, LoadedAddresses},
    },
    solana_program_runtime::{
        execution_budget::SVMTransactionExecutionBudget,
        loaded_programs::ProgramRuntimeEnvironments, vm_opts,
    },
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_signature::Signature,
    solana_signer::Signer,
    solana_svm::{
        account_loader::{CheckedTransactionDetails, TransactionCheckResult},
        transaction_processing_result::{ProcessedTransaction, TransactionProcessingResult},
        transaction_processor::{
            ExecutionRecordingConfig, TransactionBatchProcessor, TransactionProcessingConfig,
            TransactionProcessingEnvironment,
        },
    },
    solana_svm_feature_set::SVMFeatureSet,
    solana_svm_transaction::svm_message::SVMStaticMessage,
    solana_svm_type_overrides::sync::{Arc, RwLock},
    solana_sysvar_id::SysvarId,
    solana_transaction::{
        Transaction,
        sanitized::{MessageHash, SanitizedTransaction},
        versioned::VersionedTransaction,
    },
    std::{
        collections::{HashMap, HashSet},
        str::FromStr,
    },
};

mod mock_bank;

const LAMPORTS_PER_SIGNATURE: u64 = 5000;

/// `(label, heap_zero_opt, pda_cache, ser_pool, cheap_timers)`. The first entry is the reference.
const SWITCH_SETTINGS: [(&str, bool, bool, bool, bool); 6] = [
    ("stock", false, false, false, false),
    ("SOLANA_VM_HEAP_ZERO_OPT", true, false, false, false),
    ("SOLANA_VM_PDA_CACHE", false, true, false, false),
    ("SOLANA_VM_SER_POOL", false, false, true, false),
    ("SOLANA_VM_CHEAP_TIMERS", false, false, false, true),
    ("all", true, true, true, true),
];

fn set_switches(heap_zero_opt: bool, pda_cache: bool, ser_pool: bool, cheap_timers: bool) {
    vm_opts::HEAP_ZERO_OPT.set(heap_zero_opt);
    vm_opts::PDA_CACHE.set(pda_cache);
    vm_opts::SER_POOL.set(ser_pool);
    solana_svm_measure::clock::set_cheap_timers(cheap_timers);
}

/// Everything the cluster or a client can observe of one processed transaction.
#[derive(Debug, PartialEq, Eq)]
struct Observation {
    outcome: String,
    executed_units: Option<u64>,
    logs: Vec<String>,
    accounts: Vec<(Pubkey, AccountSharedData)>,
}

fn observe(result: &TransactionProcessingResult) -> Observation {
    match result {
        Ok(ProcessedTransaction::Executed(executed)) => {
            let details = &executed.execution_details;
            Observation {
                outcome: format!(
                    "executed: status={:?} return_data={:?} inner_instructions={:?} \
                     accounts_deltas={:?} fee_details={:?} rollback={:?} touched={:?} \
                     loaded_bytes={}",
                    details.status,
                    details.return_data,
                    details.inner_instructions,
                    details.accounts_deltas,
                    executed.loaded_transaction.fee_details,
                    executed.loaded_transaction.rollback_accounts,
                    executed.loaded_transaction.touched_flags,
                    executed.loaded_transaction.loaded_accounts_data_size,
                ),
                executed_units: Some(details.executed_units),
                logs: details.log_messages.clone().unwrap_or_default(),
                accounts: executed.loaded_transaction.accounts.clone(),
            }
        }
        Ok(ProcessedTransaction::FeesOnly(fees_only)) => Observation {
            outcome: format!("fees-only: {fees_only:?}"),
            executed_units: None,
            logs: vec![],
            accounts: vec![],
        },
        Ok(ProcessedTransaction::NoOp(error)) => Observation {
            outcome: format!("no-op: {error:?}"),
            executed_units: None,
            logs: vec![],
            accounts: vec![],
        },
        Err(error) => Observation {
            outcome: format!("discarded: {error:?}"),
            executed_units: None,
            logs: vec![],
            accounts: vec![],
        },
    }
}

/// An SVM over the given accounts (incl. sysvars and programs) with the production syscalls.
struct TestSvm {
    mock_bank: MockBankCallback,
    _fork_graph: Arc<RwLock<MockForkGraph>>,
    batch_processor: TransactionBatchProcessor<MockForkGraph>,
    processing_environment: TransactionProcessingEnvironment,
    feature_set: SVMFeatureSet,
}

impl TestSvm {
    fn new(
        accounts: &HashMap<Pubkey, AccountSharedData>,
        feature_set: &SVMFeatureSet,
        slot: Slot,
        epoch: Epoch,
        rent: &Rent,
        blockhash: Hash,
    ) -> Self {
        let mock_bank = MockBankCallback {
            feature_set: *feature_set,
            ..MockBankCallback::default()
        };
        mock_bank.account_shared_data.write().unwrap().extend(
            accounts
                .iter()
                .map(|(key, account)| (*key, account.clone())),
        );
        let fork_graph = Arc::new(RwLock::new(MockForkGraph {}));
        let program_runtime_environment = solana_syscalls::create_program_runtime_environment(
            feature_set,
            &SVMTransactionExecutionBudget::new_with_defaults(
                feature_set.raise_cpi_nesting_limit_to_8,
            ),
            false,
            false,
        )
        .unwrap();
        let batch_processor = TransactionBatchProcessor::new(
            slot,
            epoch,
            Arc::downgrade(&fork_graph),
            Some(program_runtime_environment),
        );
        batch_processor.fill_missing_sysvar_cache_entries(&mock_bank);
        register_builtins(&mock_bank, &batch_processor);
        let processing_environment = TransactionProcessingEnvironment {
            blockhash,
            blockhash_lamports_per_signature: LAMPORTS_PER_SIGNATURE,
            alpenglow_migration_succeeded: false,
            epoch_total_stake: 0,
            feature_set: *feature_set,
            program_runtime_environments: ProgramRuntimeEnvironments::new(
                batch_processor.program_runtime_environment_for_epoch(epoch),
                batch_processor.program_runtime_environment_for_epoch(epoch),
            ),
            rent: rent.clone(),
        };
        Self {
            mock_bank,
            _fork_graph: fork_graph,
            batch_processor,
            processing_environment,
            feature_set: *feature_set,
        }
    }

    /// Executes `transactions` as one batch on the calling thread (results are not committed,
    /// so the batch can be re-run against the same state).
    fn run(
        &self,
        agave_feature_set: &agave_feature_set::FeatureSet,
        transactions: &[SanitizedTransaction],
    ) -> Vec<Observation> {
        let processing_config = TransactionProcessingConfig {
            recording_config: ExecutionRecordingConfig::new_single_setting(true),
            log_messages_bytes_limit: Some(10_000),
            ..TransactionProcessingConfig::default()
        };
        let check_results: Vec<TransactionCheckResult> = transactions
            .iter()
            .map(|transaction| {
                let limits = process_compute_budget_instructions(
                    SVMStaticMessage::program_instructions_iter(transaction),
                    agave_feature_set,
                )?;
                let signature_count = transaction.num_transaction_signatures();
                let budget = limits.get_compute_budget_and_limits(
                    limits.loaded_accounts_bytes,
                    solana_fee_structure::FeeDetails::new(
                        signature_count * LAMPORTS_PER_SIGNATURE,
                        limits.get_prioritization_fee(),
                    ),
                    self.feature_set.raise_cpi_nesting_limit_to_8,
                );
                Ok(CheckedTransactionDetails::new(None, budget))
            })
            .collect();
        let output = self
            .batch_processor
            .load_and_execute_sanitized_transactions(
                &self.mock_bank,
                transactions,
                check_results,
                &self.processing_environment,
                &processing_config,
            );
        output.processing_results.iter().map(observe).collect()
    }
}

/// Builds a fresh SVM and executes `transactions` as one batch on the calling thread.
#[allow(clippy::too_many_arguments)]
fn execute_batch(
    accounts: &HashMap<Pubkey, AccountSharedData>,
    feature_set: &SVMFeatureSet,
    agave_feature_set: &agave_feature_set::FeatureSet,
    slot: Slot,
    epoch: Epoch,
    rent: &Rent,
    blockhash: Hash,
    transactions: &[SanitizedTransaction],
) -> Vec<Observation> {
    TestSvm::new(accounts, feature_set, slot, epoch, rent, blockhash)
        .run(agave_feature_set, transactions)
}

/// Runs the batch under every switch setting and asserts identical observations.
#[allow(clippy::too_many_arguments)]
fn assert_identical_under_all_switches(
    label: &str,
    accounts: &HashMap<Pubkey, AccountSharedData>,
    feature_set: &SVMFeatureSet,
    agave_feature_set: &agave_feature_set::FeatureSet,
    slot: Slot,
    epoch: Epoch,
    rent: &Rent,
    blockhash: Hash,
    transactions: &[SanitizedTransaction],
) -> Vec<Observation> {
    let initial = (
        vm_opts::HEAP_ZERO_OPT.enabled(),
        vm_opts::PDA_CACHE.enabled(),
        vm_opts::SER_POOL.enabled(),
        solana_svm_measure::clock::cheap_timers_enabled(),
    );
    let mut reference: Option<Vec<Observation>> = None;
    // Two rounds so that the second round runs on pools and caches dirtied by every setting.
    for round in 0..2 {
        for (setting, heap, pda, ser, timers) in SWITCH_SETTINGS {
            set_switches(heap, pda, ser, timers);
            let observed = execute_batch(
                accounts,
                feature_set,
                agave_feature_set,
                slot,
                epoch,
                rent,
                blockhash,
                transactions,
            );
            match &reference {
                None => reference = Some(observed),
                Some(reference) => {
                    assert_eq!(reference.len(), observed.len());
                    for (index, (expected, actual)) in
                        reference.iter().zip(observed.iter()).enumerate()
                    {
                        assert_eq!(
                            expected, actual,
                            "{label}: transaction {index} differs under {setting} (round {round})"
                        );
                    }
                }
            }
        }
    }
    set_switches(initial.0, initial.1, initial.2, initial.3);
    reference.unwrap()
}

// ---------------------------------------------------------------------------------------------
// SPL programs (p-token, Token-2022, ATA) from solana-program-binaries.

const TOKEN_PROGRAM: Pubkey = spl_generic_token::token::ID;
const TOKEN_2022_PROGRAM: Pubkey = spl_generic_token::token_2022::ID;
const ATA_PROGRAM: Pubkey = spl_generic_token::associated_token_account::ID;
const SYSTEM_PROGRAM: Pubkey = solana_sdk_ids::system_program::ID;

fn mint_data(authority: &Pubkey, supply: u64, decimals: u8) -> Vec<u8> {
    let mut data = vec![0u8; 82];
    data[0..4].copy_from_slice(&1u32.to_le_bytes());
    data[4..36].copy_from_slice(authority.as_ref());
    data[36..44].copy_from_slice(&supply.to_le_bytes());
    data[44] = decimals;
    data[45] = 1; // initialized
    data
}

fn token_account_data(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Vec<u8> {
    let mut data = vec![0u8; 165];
    data[0..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1; // AccountState::Initialized
    data
}

fn owned_account(rent: &Rent, owner: &Pubkey, data: Vec<u8>) -> AccountSharedData {
    let mut account = AccountSharedData::new(rent.minimum_balance(data.len()), 0, owner);
    account.set_data_from_slice(&data);
    account
}

fn ata_address(wallet: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[wallet.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ATA_PROGRAM,
    )
    .0
}

fn create_ata_idempotent(
    payer: &Pubkey,
    wallet: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction::new_with_bytes(
        ATA_PROGRAM,
        &[1],
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata_address(wallet, mint, token_program), false),
            AccountMeta::new_readonly(*wallet, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
    )
}

fn transfer_checked(
    token_program: &Pubkey,
    source: &Pubkey,
    mint: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
    amount: u64,
    decimals: u8,
) -> Instruction {
    let mut data = vec![12];
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    Instruction::new_with_bytes(
        *token_program,
        &data,
        vec![
            AccountMeta::new(*source, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*authority, true),
        ],
    )
}

fn compute_budget_ix(data: Vec<u8>) -> Instruction {
    Instruction::new_with_bytes(solana_sdk_ids::compute_budget::ID, &data, vec![])
}

fn spl_feature_sets() -> Vec<(&'static str, SVMFeatureSet)> {
    // Mainnet today: no virtual address space adjustments / direct mapping, so every account is
    // copied into the serialization buffer (including its 10 KiB realloc padding).
    let mut mainnet_like = SVMFeatureSet::all_enabled();
    mainnet_like.virtual_address_space_adjustments = false;
    mainnet_like.account_data_direct_mapping = false;
    mainnet_like.direct_account_pointers_in_program_input = false;
    vec![
        ("mainnet-like", mainnet_like),
        ("all features", SVMFeatureSet::all_enabled()),
    ]
}

#[test]
fn spl_programs_identical_with_vm_opts() {
    let rent = Rent::default();
    let blockhash = Hash::new_from_array([7; 32]);
    let payer = Keypair::new();
    let wallet_b = Pubkey::new_unique();
    let wallet_c = Pubkey::new_unique();
    let wallet_d = Pubkey::new_unique();
    let mint = Pubkey::new_unique();
    let mint_2022 = Pubkey::new_unique();
    let payer_token = Pubkey::new_unique();
    let payer_token_2022 = Pubkey::new_unique();
    let b_token_2022 = Pubkey::new_unique();

    let mut accounts: HashMap<Pubkey, AccountSharedData> =
        solana_program_binaries::spl_programs(&rent)
            .into_iter()
            .collect();
    let system_owned = |lamports| AccountSharedData::new(lamports, 0, &SYSTEM_PROGRAM);
    accounts.insert(payer.pubkey(), system_owned(100_000_000_000));
    for wallet in [wallet_b, wallet_c, wallet_d] {
        accounts.insert(wallet, system_owned(1_000_000_000));
    }
    accounts.insert(
        mint,
        owned_account(
            &rent,
            &TOKEN_PROGRAM,
            mint_data(&payer.pubkey(), 1_000_000, 6),
        ),
    );
    accounts.insert(
        mint_2022,
        owned_account(
            &rent,
            &TOKEN_2022_PROGRAM,
            mint_data(&payer.pubkey(), 50_000, 9),
        ),
    );
    accounts.insert(
        payer_token,
        owned_account(
            &rent,
            &TOKEN_PROGRAM,
            token_account_data(&mint, &payer.pubkey(), 1_000_000),
        ),
    );
    accounts.insert(
        payer_token_2022,
        owned_account(
            &rent,
            &TOKEN_2022_PROGRAM,
            token_account_data(&mint_2022, &payer.pubkey(), 50_000),
        ),
    );
    accounts.insert(
        b_token_2022,
        owned_account(
            &rent,
            &TOKEN_2022_PROGRAM,
            token_account_data(&mint_2022, &wallet_b, 0),
        ),
    );
    // Default sysvars.
    let clock = Clock {
        slot: 10,
        epoch: 0,
        ..Clock::default()
    };
    for (id, data) in [
        (Clock::id(), bincode::serialize(&clock).unwrap()),
        (Rent::id(), bincode::serialize(&rent).unwrap()),
        (
            solana_epoch_schedule::EpochSchedule::id(),
            bincode::serialize(&solana_epoch_schedule::EpochSchedule::without_warmup()).unwrap(),
        ),
    ] {
        let mut account = AccountSharedData::new(1, 0, &solana_sdk_ids::sysvar::ID);
        account.set_data_from_slice(&data);
        accounts.insert(id, account);
    }

    let b_ata = ata_address(&wallet_b, &mint, &TOKEN_PROGRAM);
    let pay = payer.pubkey();
    let signed = |instructions: &[Instruction]| {
        SanitizedTransaction::from_transaction_for_tests(Transaction::new_signed_with_payer(
            instructions,
            Some(&pay),
            &[&payer],
            blockhash,
        ))
    };
    let transactions = vec![
        // ATA create: find_program_address, CPIs into system (PDA-signed create_account) and
        // p-token (GetAccountDataSize, InitializeImmutableOwner, InitializeAccount3).
        signed(&[create_ata_idempotent(
            &pay,
            &wallet_b,
            &mint,
            &TOKEN_PROGRAM,
        )]),
        // p-token transfer into the new ATA.
        signed(&[transfer_checked(
            &TOKEN_PROGRAM,
            &payer_token,
            &mint,
            &b_ata,
            &pay,
            250_000,
            6,
        )]),
        // Idempotent re-create (existing-account path).
        signed(&[create_ata_idempotent(
            &pay,
            &wallet_b,
            &mint,
            &TOKEN_PROGRAM,
        )]),
        // Token-2022 transfer.
        signed(&[transfer_checked(
            &TOKEN_2022_PROGRAM,
            &payer_token_2022,
            &mint_2022,
            &b_token_2022,
            &pay,
            1_234,
            9,
        )]),
        // Token-2022 ATA create (extension sizing via return data).
        signed(&[create_ata_idempotent(
            &pay,
            &wallet_b,
            &mint_2022,
            &TOKEN_2022_PROGRAM,
        )]),
        // Failing transfer (insufficient funds): error path and logs.
        signed(&[transfer_checked(
            &TOKEN_PROGRAM,
            &payer_token,
            &mint,
            &b_ata,
            &pay,
            5_000_000,
            6,
        )]),
        // Large heap frame, then a default heap on the same thread (pooled heap reuse).
        signed(&[
            compute_budget_ix({
                let mut data = vec![1];
                data.extend_from_slice(&(256 * 1024u32).to_le_bytes());
                data
            }),
            create_ata_idempotent(&pay, &wallet_c, &mint, &TOKEN_PROGRAM),
        ]),
        signed(&[create_ata_idempotent(
            &pay,
            &wallet_d,
            &mint,
            &TOKEN_PROGRAM,
        )]),
        // Compute-unit exhaustion inside the ATA program.
        signed(&[
            compute_budget_ix({
                let mut data = vec![2];
                data.extend_from_slice(&3_000u32.to_le_bytes());
                data
            }),
            create_ata_idempotent(&pay, &wallet_d, &mint_2022, &TOKEN_2022_PROGRAM),
        ]),
    ];

    for (label, feature_set) in spl_feature_sets() {
        let agave_feature_set = agave_feature_set::FeatureSet::all_enabled();
        let observed = assert_identical_under_all_switches(
            label,
            &accounts,
            &feature_set,
            &agave_feature_set,
            100,
            0,
            &rent,
            blockhash,
            &transactions,
        );
        // Sanity: the batch exercised the intended paths.
        let succeeded = observed
            .iter()
            .filter(|o| o.outcome.starts_with("executed: status=Ok"))
            .count();
        assert_eq!(succeeded, 7, "{label}: {observed:#?}");
        assert!(observed[5].outcome.contains("Err"), "{:?}", observed[5]);
        assert!(observed[8].outcome.contains("Err"), "{:?}", observed[8]);
        assert!(
            observed[0]
                .logs
                .iter()
                .any(|log| log.contains("invoke [2]")),
            "{:?}",
            observed[0].logs
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Recorded mainnet transactions (optional corpus).

const REPLAY_DIR_ENV: &str = "SOLANA_VM_OPTS_REPLAY_DIR";

fn hex_decode(hex: &str) -> Vec<u8> {
    if hex == "-" {
        return vec![];
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

fn index_list(list: &str) -> Vec<u8> {
    if list == "-" {
        return vec![];
    }
    list.split(',').map(|i| i.parse().unwrap()).collect()
}

fn parse_account_line(fields: &[&str]) -> (Pubkey, AccountSharedData) {
    // acct <pubkey> <owner> <lamports> <exec> <rent_epoch> <hex>
    let key = Pubkey::from_str(fields[1]).unwrap();
    let owner = Pubkey::from_str(fields[2]).unwrap();
    let mut account = AccountSharedData::new(fields[3].parse().unwrap(), 0, &owner);
    account.set_executable(fields[4] == "1");
    account.set_rent_epoch(fields[5].parse().unwrap());
    account.set_data_from_slice(&hex_decode(fields.get(6).copied().unwrap_or("-")));
    (key, account)
}

struct RecordedTransaction {
    label: String,
    slot: Slot,
    block_time: i64,
    recorded_units: u64,
    recorded_logs: Vec<String>,
    transaction: SanitizedTransaction,
    blockhash: Hash,
    overrides: Vec<(Pubkey, Option<AccountSharedData>)>,
}

fn parse_transactions(text: &str) -> Vec<RecordedTransaction> {
    let mut transactions = vec![];
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(fields[0], "tx", "{line}");
        let label = fields[1].to_string();
        let slot = fields[2].parse().unwrap();
        let block_time = fields[3].parse().unwrap();
        let recorded_units = fields[4].parse().unwrap();
        let mut signatures = vec![];
        let mut header = MessageHeader::default();
        let mut keys = vec![];
        let mut blockhash = Hash::default();
        let mut instructions = vec![];
        let mut lookups = vec![];
        let mut loaded = LoadedAddresses::default();
        let mut is_v0 = false;
        let mut overrides = vec![];
        let mut recorded_logs = vec![];
        for line in lines.by_ref() {
            if let Some(log) = line.strip_prefix("log ") {
                recorded_logs.push(log.to_string());
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields.first().copied() {
                Some("end") => break,
                Some("sig") => signatures.push(Signature::from_str(fields[1]).unwrap()),
                Some("header") => {
                    header = MessageHeader {
                        num_required_signatures: fields[1].parse().unwrap(),
                        num_readonly_signed_accounts: fields[2].parse().unwrap(),
                        num_readonly_unsigned_accounts: fields[3].parse().unwrap(),
                    }
                }
                Some("keys") => {
                    keys = fields[1..]
                        .iter()
                        .map(|k| Pubkey::from_str(k).unwrap())
                        .collect()
                }
                Some("blockhash") => blockhash = Hash::from_str(fields[1]).unwrap(),
                Some("ix") => instructions.push(CompiledInstruction {
                    program_id_index: fields[1].parse().unwrap(),
                    accounts: index_list(fields[2]),
                    data: hex_decode(fields[3]),
                }),
                Some("version") => is_v0 = fields[1] == "v0",
                Some("alt") => lookups.push(v0::MessageAddressTableLookup {
                    account_key: Pubkey::from_str(fields[1]).unwrap(),
                    writable_indexes: index_list(fields[2]),
                    readonly_indexes: index_list(fields[3]),
                }),
                Some("loaded_w") => {
                    loaded.writable = fields[1..]
                        .iter()
                        .map(|k| Pubkey::from_str(k).unwrap())
                        .collect()
                }
                Some("loaded_r") => {
                    loaded.readonly = fields[1..]
                        .iter()
                        .map(|k| Pubkey::from_str(k).unwrap())
                        .collect()
                }
                Some("acct") => {
                    let (key, account) = parse_account_line(&fields);
                    overrides.push((key, Some(account)));
                }
                Some("del") => overrides.push((Pubkey::from_str(fields[1]).unwrap(), None)),
                other => panic!("unexpected line {other:?}"),
            }
        }
        let message = if is_v0 {
            VersionedMessage::V0(v0::Message {
                header,
                account_keys: keys,
                recent_blockhash: blockhash,
                instructions,
                address_table_lookups: lookups,
            })
        } else {
            VersionedMessage::Legacy(Message {
                header,
                account_keys: keys,
                recent_blockhash: blockhash,
                instructions,
            })
        };
        let transaction = SanitizedTransaction::try_create(
            VersionedTransaction {
                signatures,
                message,
            },
            MessageHash::Compute,
            Some(false),
            SimpleAddressLoader::Enabled(loaded),
            &HashSet::new(),
        )
        .unwrap();
        transactions.push(RecordedTransaction {
            label,
            slot,
            block_time,
            recorded_units,
            recorded_logs,
            transaction,
            blockhash,
            overrides,
        });
    }
    transactions
}

/// A recorded transaction with its reconstructed pre-state.
struct ReplayCase {
    recorded: RecordedTransaction,
    accounts: HashMap<Pubkey, AccountSharedData>,
    clock: Clock,
}

struct ReplayCorpus {
    snapshot_slot: Slot,
    agave_feature_set: agave_feature_set::FeatureSet,
    feature_set: SVMFeatureSet,
    rent: Rent,
    cases: Vec<ReplayCase>,
}

fn load_replay_corpus() -> Option<ReplayCorpus> {
    let Ok(dir) = std::env::var(REPLAY_DIR_ENV) else {
        eprintln!("{REPLAY_DIR_ENV} not set; skipping the recorded-transaction differential");
        return None;
    };
    let read = |name: &str| {
        std::fs::read_to_string(format!("{dir}/{name}"))
            .unwrap_or_else(|err| panic!("{dir}/{name}: {err}"))
    };
    let snapshot_slot: Slot = read("meta.txt")
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let mut agave_feature_set = agave_feature_set::FeatureSet::default();
    for line in read("features.txt").lines() {
        let mut fields = line.split_whitespace();
        let id = Pubkey::from_str(fields.next().unwrap()).unwrap();
        let slot = fields.next().unwrap().parse().unwrap();
        agave_feature_set.activate(&id, slot);
    }
    let feature_set = agave_feature_set.runtime_features();
    let base_accounts: HashMap<Pubkey, AccountSharedData> = read("accounts.txt")
        .lines()
        .map(|line| parse_account_line(&line.split_whitespace().collect::<Vec<_>>()))
        .collect();
    let rent: Rent = bincode::deserialize(base_accounts[&Rent::id()].data()).unwrap();
    let base_clock: Clock = bincode::deserialize(base_accounts[&Clock::id()].data()).unwrap();
    let cases = parse_transactions(&read("txs.txt"))
        .into_iter()
        .map(|recorded| {
            let mut accounts = base_accounts.clone();
            for (key, account) in &recorded.overrides {
                match account {
                    Some(account) => {
                        accounts.insert(*key, account.clone());
                    }
                    None => {
                        accounts.remove(key);
                    }
                }
            }
            // Clock at the transaction's slot and block time, as in the pumpstep harness.
            let clock = Clock {
                slot: recorded.slot,
                unix_timestamp: recorded.block_time,
                ..base_clock.clone()
            };
            accounts
                .get_mut(&Clock::id())
                .unwrap()
                .set_data_from_slice(&bincode::serialize(&clock).unwrap());
            ReplayCase {
                recorded,
                accounts,
                clock,
            }
        })
        .collect();
    Some(ReplayCorpus {
        snapshot_slot,
        agave_feature_set,
        feature_set,
        rent,
        cases,
    })
}

/// Replays each recorded transaction against its own reconstructed pre-state with every switch
/// setting. Set `SOLANA_VM_OPTS_REPLAY_DIR` to the output of `export_agave_diff.py`.
#[test]
fn recorded_mainnet_transactions_identical_with_vm_opts() {
    let Some(corpus) = load_replay_corpus() else {
        return;
    };
    for ReplayCase {
        recorded,
        accounts,
        clock,
    } in &corpus.cases
    {
        let (feature_set, agave_feature_set, rent, snapshot_slot) = (
            &corpus.feature_set,
            &corpus.agave_feature_set,
            &corpus.rent,
            corpus.snapshot_slot,
        );
        let observed = assert_identical_under_all_switches(
            &recorded.label,
            accounts,
            feature_set,
            agave_feature_set,
            // Newer than every programdata deployment in the snapshot.
            snapshot_slot + 1,
            clock.epoch,
            rent,
            recorded.blockhash,
            std::slice::from_ref(&recorded.transaction),
        );
        let observation = &observed[0];
        eprintln!(
            "{}: {} executed_units={:?} (recorded on mainnet {}), {} log lines",
            recorded.label,
            observation
                .outcome
                .split(' ')
                .take(2)
                .collect::<Vec<_>>()
                .join(" "),
            observation.executed_units,
            recorded.recorded_units,
            observation.logs.len(),
        );
        assert!(
            observation.outcome.starts_with("executed: status=Ok"),
            "{}: {observation:#?}",
            recorded.label
        );
        // The reconstruction reproduces mainnet (so the switches were exercised on the real
        // code paths): the same compute units and, when recorded, the same log lines. Event
        // payloads ("Program data:") may embed pool fields that the pre-state patch does not
        // restore (the snapshot is later than the transaction), so only their count is checked.
        assert_eq!(
            observation.executed_units,
            Some(recorded.recorded_units),
            "{}",
            recorded.label
        );
        if !recorded.recorded_logs.is_empty() {
            let comparable = |logs: &[String]| {
                logs.iter()
                    .map(|log| {
                        if log.starts_with("Program data: ") {
                            "Program data: <event>".to_string()
                        } else {
                            log.clone()
                        }
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                comparable(&observation.logs),
                comparable(&recorded.recorded_logs),
                "{}",
                recorded.label
            );
        }
    }
}

/// Microbenchmark (run in release with `-- --ignored --nocapture` and
/// `SOLANA_VM_OPTS_REPLAY_DIR`): wall time of executing each recorded transaction (load +
/// execute, no commit) under each switch setting, alternating settings on every run. On a non-x86_64
/// host the sBPF interpreter runs instead of the JIT, which inflates the instruction share and
/// dilutes the relative savings; only the absolute per-transaction deltas are indicative.
#[test]
#[ignore]
fn bench_recorded_mainnet_transactions() {
    let Some(corpus) = load_replay_corpus() else {
        return;
    };
    const ROUNDS: usize = 40;
    const RUNS_PER_ROUND: usize = 20;
    for case in &corpus.cases {
        // One SVM for all settings (execution results are not committed), so that settings do
        // not differ by program-cache instance or memory layout.
        let svm = TestSvm::new(
            &case.accounts,
            &corpus.feature_set,
            corpus.snapshot_slot + 1,
            case.clock.epoch,
            &corpus.rent,
            case.recorded.blockhash,
        );
        let transactions = std::slice::from_ref(&case.recorded.transaction);
        let mut samples: Vec<Vec<f64>> = vec![vec![]; SWITCH_SETTINGS.len()];
        // Settings alternate on every run, so slow drifts (other load, core migration between
        // performance and efficiency cores) hit all settings alike.
        for round in 0..ROUNDS {
            for _ in 0..RUNS_PER_ROUND {
                for (index, (_, heap, pda, ser, timers)) in SWITCH_SETTINGS.iter().enumerate() {
                    set_switches(*heap, *pda, *ser, *timers);
                    let start = std::time::Instant::now();
                    let observed = svm.run(&corpus.agave_feature_set, transactions);
                    let elapsed = start.elapsed().as_nanos() as f64 / 1000.0;
                    assert!(observed[0].outcome.starts_with("executed: status=Ok"));
                    // The first round warms program caches and pools.
                    if round > 0 {
                        samples[index].push(elapsed);
                    }
                }
            }
        }
        set_switches(false, false, false, false);
        let median = |samples: &mut Vec<f64>| {
            samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
            samples[samples.len() / 2]
        };
        let stock = median(&mut samples[0]);
        eprintln!(
            "{} ({} CU): stock p50 {stock:.1} us",
            case.recorded.label, case.recorded.recorded_units
        );
        for (index, (label, ..)) in SWITCH_SETTINGS.iter().enumerate().skip(1) {
            let p50 = median(&mut samples[index]);
            eprintln!("  {label:<24} p50 {p50:.1} us ({:+.1} us)", p50 - stock);
        }
    }
}
