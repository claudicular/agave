use {
    crate::{
        bank::{Bank, TransactionBalancesSet},
        bank_utils,
        dependency_tracker::DependencyTracker,
        prioritization_fee_cache::PrioritizationFeeCache,
        transaction_balances::compile_collected_balances,
        transaction_batch::TransactionBatch,
        vote_sender_types::{ReplayVoteSendType, ReplayVoteSender},
    },
    log::{trace, warn},
    solana_clock::{BankId, Slot},
    solana_cost_model::{cost_model::CostModel, transaction_cost::TransactionCost},
    solana_measure::measure::Measure,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_signature::Signature,
    solana_svm::{
        transaction_commit_result::{
            TransactionCommitResult, TransactionCommitResultExtensions as _,
        },
        transaction_processor::ExecutionRecordingConfig,
    },
    solana_svm_timings::{ExecuteTimingType, ExecuteTimings},
    solana_svm_transaction::{svm_message::SVMMessage, svm_transaction::SVMTransaction},
    solana_transaction::sanitized::SanitizedTransaction,
    solana_transaction_error::{TransactionError, TransactionResult},
    solana_transaction_status::token_balances::TransactionTokenBalancesSet,
    std::{borrow::Cow, sync::Arc},
};

type WorkSequence = u64;

#[derive(Debug)]
pub struct TransactionStatusBatch {
    pub slot: Slot,
    pub bank_id: BankId,
    pub transactions: Vec<SanitizedTransaction>,
    pub commit_results: Vec<TransactionCommitResult>,
    pub balances: TransactionBalancesSet,
    pub token_balances: TransactionTokenBalancesSet,
    pub costs: Vec<Option<u64>>,
    pub transaction_indexes: Vec<usize>,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum TransactionStatusMessage {
    Batch((TransactionStatusBatch, Option<WorkSequence>)),
    Freeze(Arc<Bank>),
}

pub struct TransactionBatchWithIndexes<'a, 'b, Tx: SVMMessage> {
    pub batch: TransactionBatch<'a, 'b, Tx>,
    pub transaction_indexes: Vec<usize>,
}

pub fn execute_batch<'a>(
    batch: &'a TransactionBatchWithIndexes<impl TransactionWithMeta>,
    bank: &'a Arc<Bank>,
    transaction_status_sender: Option<&'a TransactionStatusSender>,
    replay_vote_sender: Option<&'a ReplayVoteSender>,
    replay_vote_send_type: ReplayVoteSendType,
    timings: &'a mut ExecuteTimings,
    log_messages_bytes_limit: Option<usize>,
    prioritization_fee_cache: Option<&'a PrioritizationFeeCache>,
) -> TransactionResult<()> {
    let TransactionBatchWithIndexes {
        batch,
        transaction_indexes,
    } = batch;

    let transaction_indexes = Cow::from(transaction_indexes);

    // Fast-lane shadow check: a copy of what the commit consumes (see `fast_lane_commit`).
    let capture = crate::fast_lane_commit::capture_enabled();
    let mut captured = Vec::new();
    // Fast-lane commit mode: a sampled transaction is executed here and compared with the
    // fast lane's result before its commit.
    let sampled = transaction_indexes.len() == 1
        && crate::fast_lane_commit::is_sample(
            bank,
            &batch.sanitized_transactions()[0],
        );
    let pre_commit_callback = |processing_results: &[_]| -> TransactionResult<()> {
        if capture {
            captured = processing_results
                .iter()
                .map(crate::fast_lane_commit::clone_processing_result)
                .collect();
        }
        if sampled {
            if let Some(result) = processing_results.first() {
                crate::fast_lane_commit::verify_sample(bank, transaction_indexes[0], result);
            }
        }
        // We're entering into one of the block-verification methods.
        get_first_error(batch, processing_results)
    };

    let (commit_results, balance_collector) = batch
        .bank()
        .load_execute_and_commit_transactions_with_pre_commit_callback(
            batch,
            ExecutionRecordingConfig::new_single_setting(transaction_status_sender.is_some()),
            timings,
            log_messages_bytes_limit,
            pre_commit_callback,
        )?;

    // A sampled transaction of a fast-lane commit bank: compare balances with FL's result.
    let verify_balances = sampled.then_some(transaction_indexes.first().copied()).flatten();
    finish_committed_batch(
        batch,
        transaction_indexes,
        bank,
        commit_results,
        balance_collector,
        transaction_status_sender,
        replay_vote_sender,
        replay_vote_send_type,
        timings,
        prioritization_fee_cache,
        capture.then_some(captured),
        verify_balances,
    )
}

/// Everything `execute_batch` does after the commit: the block cost check, votes,
/// prioritization fees and the transaction status batch (and the fast-lane shadow capture).
/// Shared with [`commit_external`], so a fast-lane commit has exactly these effects.
#[allow(clippy::too_many_arguments)]
fn finish_committed_batch<Tx: TransactionWithMeta>(
    batch: &TransactionBatch<Tx>,
    transaction_indexes: Cow<[usize]>,
    bank: &Arc<Bank>,
    commit_results: Vec<TransactionCommitResult>,
    balance_collector: Option<solana_svm::transaction_balances::BalanceCollector>,
    transaction_status_sender: Option<&TransactionStatusSender>,
    replay_vote_sender: Option<&ReplayVoteSender>,
    replay_vote_send_type: ReplayVoteSendType,
    timings: &mut ExecuteTimings,
    prioritization_fee_cache: Option<&PrioritizationFeeCache>,
    captured: Option<Vec<solana_svm::transaction_processing_result::TransactionProcessingResult>>,
    verify_balances: Option<usize>,
) -> TransactionResult<()> {
    let capture = captured.is_some();
    let captured = captured.unwrap_or_default();
    let mut check_block_costs_elapsed = Measure::start("check_block_costs");

    let tx_costs = get_transaction_costs(bank, &commit_results, batch.sanitized_transactions());
    let checked_tx_costs_result = check_block_cost_limits(bank, &tx_costs);

    check_block_costs_elapsed.stop();
    timings.saturating_add_in_place(
        ExecuteTimingType::CheckBlockLimitsUs,
        check_block_costs_elapsed.as_us(),
    );

    checked_tx_costs_result?;

    bank_utils::find_and_send_votes(
        batch.sanitized_transactions(),
        &commit_results,
        replay_vote_sender,
        replay_vote_send_type,
    );

    if let Some(prioritization_fee_cache) = prioritization_fee_cache {
        let fee_paying_transactions = commit_results
            .iter()
            .zip(batch.sanitized_transactions())
            .filter_map(|(commit_result, tx)| commit_result.was_fee_paying().then_some(tx));
        prioritization_fee_cache.update(bank, fee_paying_transactions);
    }
    let capture = capture && !captured.is_empty();
    let want_balances = capture || verify_balances.is_some();
    let captured_costs: Vec<Option<u64>> = if capture {
        tx_costs
            .iter()
            .map(|cost| cost.as_ref().map(|cost| cost.sum()))
            .collect()
    } else {
        Vec::new()
    };
    let captured_indexes: Vec<usize> = if capture {
        transaction_indexes.to_vec()
    } else {
        Vec::new()
    };
    let mut captured_balances: Option<Vec<crate::fast_lane_commit::TxBalances>> = None;
    if let Some(transaction_status_sender) = transaction_status_sender {
        let transactions: Vec<SanitizedTransaction> = batch
            .sanitized_transactions()
            .iter()
            .map(|tx| tx.as_sanitized_transaction().into_owned())
            .collect();

        // There are two cases where balance_collector could be None:
        // * Balance recording is disabled. If that were the case, there would
        //   be no TransactionStatusSender, and we would not be in this branch.
        // * The batch was aborted in its entirety in SVM. In that case, nothing
        //   would have been committed.
        // Therefore this should always be true.
        debug_assert!(balance_collector.is_some());

        let (balances, token_balances) =
            compile_collected_balances(balance_collector.unwrap_or_default());
        if want_balances {
            captured_balances = Some(
                balances
                    .pre_balances
                    .iter()
                    .zip(&balances.post_balances)
                    .zip(
                        token_balances
                            .pre_token_balances
                            .iter()
                            .zip(&token_balances.post_token_balances),
                    )
                    .map(|((pre, post), (token_pre, token_post))| {
                        crate::fast_lane_commit::TxBalances {
                            pre: pre.clone(),
                            post: post.clone(),
                            token_pre: token_pre.clone(),
                            token_post: token_post.clone(),
                        }
                    })
                    .collect(),
            );
        }

        // The length of costs vector needs to be consistent with all other
        // vectors that are sent over (such as `transactions`). So, replace the
        // None elements with Some(0)
        let tx_costs = tx_costs
            .into_iter()
            .map(|tx_cost_option| tx_cost_option.map(|tx_cost| tx_cost.sum()).or(Some(0)))
            .collect();

        transaction_status_sender.send_transaction_status_batch(
            bank.slot(),
            bank.bank_id(),
            transactions,
            commit_results,
            balances,
            token_balances,
            tx_costs,
            transaction_indexes.into_owned(),
        );
    }
    if let (Some(index), Some(balances)) = (verify_balances, captured_balances.as_ref()) {
        if let Some(balances) = balances.first() {
            crate::fast_lane_commit::verify_sample_balances(bank, index, balances);
        }
    }
    if capture {
        crate::fast_lane_commit::send_captures(
            bank,
            batch.sanitized_transactions(),
            &captured_indexes,
            captured,
            captured_balances,
            captured_costs,
        );
    }

    Ok(())
}

fn check_block_cost_limits<Tx: TransactionWithMeta>(
    bank: &Bank,
    tx_costs: &[Option<TransactionCost<'_, Tx>>],
) -> TransactionResult<()> {
    let mut cost_tracker = bank.write_cost_tracker().unwrap();
    for tx_cost in tx_costs.iter().flatten() {
        cost_tracker
            .try_add(tx_cost)
            .map_err(TransactionError::from)?;
    }

    Ok(())
}

// Get actual transaction execution costs from transaction commit results
fn get_transaction_costs<'a, Tx: TransactionWithMeta>(
    bank: &Bank,
    commit_results: &[TransactionCommitResult],
    sanitized_transactions: &'a [Tx],
) -> Vec<Option<TransactionCost<'a, Tx>>> {
    assert_eq!(sanitized_transactions.len(), commit_results.len());

    commit_results
        .iter()
        .zip(sanitized_transactions)
        .map(|(commit_result, tx)| {
            if let Ok(committed_tx) = commit_result {
                Some(CostModel::calculate_cost_for_executed_transaction(
                    tx,
                    committed_tx.executed_units,
                    committed_tx.loaded_account_stats.loaded_accounts_data_size,
                    &bank.feature_set,
                ))
            } else {
                None
            }
        })
        .collect()
}

fn get_first_error<T, Tx: SVMTransaction>(
    batch: &TransactionBatch<Tx>,
    commit_results: &[TransactionResult<T>],
) -> TransactionResult<()> {
    do_get_first_error(batch, commit_results)
        .map(|(error, _signature)| error)
        .unwrap_or(Ok(()))
}

// Includes transaction signature for unit-testing
fn do_get_first_error<T, Tx: SVMTransaction>(
    batch: &TransactionBatch<Tx>,
    results: &[TransactionResult<T>],
) -> Option<(TransactionResult<()>, Signature)> {
    let mut first_err = None;
    for (result, transaction) in results.iter().zip(batch.sanitized_transactions()) {
        if let Err(err) = result {
            if first_err.is_none() {
                first_err = Some((Err(err.clone()), *transaction.signature()));
            }
            warn!("Unexpected validator error: {err:?}, transaction: {transaction:?}");
            datapoint_error!(
                "validator_process_entry_error",
                (
                    "error",
                    format!("error: {err:?}, transaction: {transaction:?}"),
                    String
                )
            );
        }
    }
    first_err
}

#[derive(Clone, Debug)]
pub struct TransactionStatusSender {
    pub sender: crossbeam_channel::Sender<TransactionStatusMessage>,
    pub dependency_tracker: Option<Arc<DependencyTracker>>,
}

impl TransactionStatusSender {
    pub fn send_transaction_status_batch(
        &self,
        slot: Slot,
        bank_id: BankId,
        transactions: Vec<SanitizedTransaction>,
        commit_results: Vec<TransactionCommitResult>,
        balances: TransactionBalancesSet,
        token_balances: TransactionTokenBalancesSet,
        costs: Vec<Option<u64>>,
        transaction_indexes: Vec<usize>,
    ) {
        let work_sequence = self
            .dependency_tracker
            .as_ref()
            .map(|dependency_tracker| dependency_tracker.declare_work());

        if let Err(e) = self.sender.send(TransactionStatusMessage::Batch((
            TransactionStatusBatch {
                slot,
                bank_id,
                transactions,
                commit_results,
                balances,
                token_balances,
                costs,
                transaction_indexes,
            },
            work_sequence,
        ))) {
            trace!("Slot {slot} transaction_status send batch failed: {e:?}");
        }
    }

    pub fn send_transaction_status_freeze_message(&self, bank: &Arc<Bank>) {
        if let Err(e) = self
            .sender
            .send(TransactionStatusMessage::Freeze(bank.clone()))
        {
            let slot = bank.slot();
            warn!("Slot {slot} transaction_status send freeze message failed: {e:?}");
        }
    }
}

/// Where a committed transaction's side effects go: exactly the unified scheduler's handler
/// context (`DefaultTaskHandler`).
#[derive(Clone, Copy, Default)]
pub struct CommitServices<'a> {
    pub transaction_status_sender: Option<&'a TransactionStatusSender>,
    pub replay_vote_sender: Option<&'a ReplayVoteSender>,
    pub prioritization_fee_cache: Option<&'a PrioritizationFeeCache>,
}

/// Outcome of [`commit_external`].
#[derive(Debug, PartialEq, Eq)]
pub enum ExternalCommit {
    /// Committed into the bank. The value is what replay's task for this transaction
    /// returns: `Ok`, or the block-level error of the commit (e.g. a block cost limit).
    Committed(TransactionResult<()>),
    /// Not committed and handed to agave (the cell is `FL_DECLINED`): the bank's runtime
    /// check differs from the one the result was executed with, or the result is not
    /// committable (an unprocessable transaction: agave's own execution fails the block).
    Declined(&'static str),
    /// Not committed: agave claimed the transaction first, or the bank's board is closed.
    Lost,
}

/// Commit a transaction the fast lane executed (`processing_result`, executed with the
/// runtime check result `executed_check` and replay's recording configuration) into `bank`
/// as transaction `index` of the slot, through exactly replay's commit path: the bank's own
/// `check_transactions` (blockhash age, nonce, status cache, at this point of the slot),
/// `Bank::finish_load_and_execute`, the block-verification pre-commit check,
/// `Bank::commit_transactions` (stores, lt hash, status cache, fees, stakes cache, geyser
/// notifications) and everything `execute_batch` does afterwards (cost tracker, votes,
/// prioritization fees, transaction status batch). The per-transaction claim on `board`
/// makes sure it is committed once, by the fast lane or by agave's replay.
///
/// The caller must only commit `index` after every earlier transaction of the slot that
/// conflicts with it (shares an account, one of the two writing it) is committed, by either
/// side, and must run this on a thread whose panics are contained: an unwinding commit
/// marks the cell failed and the slot for replay (see `fast_lane_commit`).
#[allow(clippy::too_many_arguments)]
pub fn commit_external<Tx: TransactionWithMeta>(
    bank: &Arc<Bank>,
    board: &crate::fast_lane_commit::Board,
    index: usize,
    transaction: &Tx,
    executed_check: &solana_svm::account_loader::TransactionCheckResult,
    processing_result: solana_svm::transaction_processing_result::TransactionProcessingResult,
    balance_collector: Option<solana_svm::transaction_balances::BalanceCollector>,
    services: CommitServices,
    timings: &mut ExecuteTimings,
) -> ExternalCommit {
    if bank.freeze_started() {
        return ExternalCommit::Lost;
    }
    let Some(guard) = board.begin_commit(index) else {
        return ExternalCommit::Lost;
    };
    if bank.freeze_started() {
        // Replay closes the board before freezing, so this cannot happen; never commit into
        // a freezing bank regardless (`commit_transactions` would panic).
        guard.decline();
        return ExternalCommit::Lost;
    }
    let batch = bank.prepare_unlocked_batch_from_single_tx(transaction);
    let mut error_counters = solana_svm::transaction_error_metrics::TransactionErrorMetrics::default();
    let check = bank.check_transactions::<Tx>(
        batch.sanitized_transactions(),
        batch.lock_results(),
        bank.max_processing_age(),
        false, // strict_nonce_size_check: never in replay
        &mut error_counters,
    );
    if check.first() != Some(executed_check) {
        guard.decline();
        return ExternalCommit::Declined("check");
    }
    let output = bank.finish_load_and_execute(
        batch.sanitized_transactions(),
        vec![processing_result],
        balance_collector,
        timings,
        &mut error_counters,
    );
    if get_first_error(&batch, &output.processing_results).is_err() {
        guard.decline();
        return ExternalCommit::Declined("unprocessable");
    }
    let commit_results = bank.commit_transactions(
        batch.sanitized_transactions(),
        output.processing_results,
        &output.processed_counts,
        timings,
    );
    let result = finish_committed_batch(
        &batch,
        Cow::Owned(vec![index]),
        bank,
        commit_results,
        output.balance_collector,
        services.transaction_status_sender,
        services.replay_vote_sender,
        ReplayVoteSendType::Executed {
            replay_bank_id: bank.bank_id(),
            replay_slot: bank.slot(),
        },
        timings,
        services.prioritization_fee_cache,
        None,
        None,
    );
    guard.done(&result, transaction.message_hash());
    ExternalCommit::Committed(result)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            genesis_utils::{
                GenesisConfigInfo, create_genesis_config, create_genesis_config_with_leader,
            },
            transaction_batch::OwnedOrBorrowed,
        },
        crossbeam_channel::bounded,
        solana_account::AccountSharedData,
        solana_compute_budget::compute_budget_limits::MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
        solana_cost_model::{
            block_cost_limits::{INSTRUCTION_DATA_BYTES_COST, SIGNATURE_COST, WRITE_LOCK_UNITS},
            cost_tracker::CostTrackerLimits,
        },
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_pubkey::Pubkey,
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_signer::Signer as _,
        solana_system_transaction as system_transaction,
        solana_transaction_error::TransactionError,
        std::{assert_matches, slice},
        test_case::test_matrix,
    };

    #[test]
    fn test_check_block_cost_limit() {
        let dummy_leader_pubkey = solana_pubkey::new_rand();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_with_leader(500, &dummy_leader_pubkey, 100);
        let bank = Bank::new_for_tests(&genesis_config);

        let tx =
            RuntimeTransaction::from_transaction_for_tests(solana_system_transaction::transfer(
                &mint_keypair,
                &Pubkey::new_unique(),
                1,
                genesis_config.hash(),
            ));
        let mut tx_cost = CostModel::calculate_cost(&tx, &bank.feature_set);
        let actual_execution_cu = 1;
        let actual_loaded_accounts_data_size = 64 * 1024;
        tx_cost.programs_execution_cost = actual_execution_cu;
        tx_cost.loaded_accounts_data_size_cost =
            CostModel::calculate_loaded_accounts_data_size_cost(
                actual_loaded_accounts_data_size,
                &bank.feature_set,
            );
        // set block-limit to be able to just have one transaction
        let block_limit = tx_cost.sum();
        bank.write_cost_tracker()
            .unwrap()
            .set_limits(CostTrackerLimits::new(u64::MAX, block_limit, u64::MAX));

        let tx_costs = vec![None, Some(tx_cost), None];
        // The transaction will fit when added the first time
        assert!(check_block_cost_limits(&bank, &tx_costs).is_ok());
        // But adding a second time will exceed the block limit
        assert_eq!(
            Err(TransactionError::WouldExceedMaxBlockCostLimit),
            check_block_cost_limits(&bank, &tx_costs)
        );
        // Adding another None will noop (even though the block is already full)
        assert!(check_block_cost_limits(&bank, &tx_costs[0..1]).is_ok());
    }

    #[test]
    fn test_get_first_error() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(1_000_000_000);
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let present_account_key = Keypair::new();
        let present_account = AccountSharedData::new(1, 10, &Pubkey::default());
        bank.store_account(&present_account_key.pubkey(), &present_account);

        // Create array of two transactions which throw different errors
        let already_processed_tx = solana_system_transaction::transfer(
            &mint_keypair,
            &solana_pubkey::new_rand(),
            42,
            bank.last_blockhash(),
        );
        let _ = bank.load_execute_and_commit_transactions(
            &bank.prepare_batch_for_tests(vec![already_processed_tx.clone()]),
            ExecutionRecordingConfig::new_single_setting(false),
            &mut ExecuteTimings::default(),
            None,
        );
        let already_processed_sig = already_processed_tx.signatures[0];
        let invalid_blockhash_tx = solana_system_transaction::transfer(
            &mint_keypair,
            &solana_pubkey::new_rand(),
            42,
            Hash::default(),
        );
        let txs = vec![already_processed_tx, invalid_blockhash_tx];
        let batch = bank.prepare_batch_for_tests(txs);
        let (commit_results, _) = bank.load_execute_and_commit_transactions(
            &batch,
            ExecutionRecordingConfig::new_single_setting(false),
            &mut ExecuteTimings::default(),
            None,
        );
        let (err, signature) = do_get_first_error(&batch, &commit_results).unwrap();
        assert_eq!(err.unwrap_err(), TransactionError::AlreadyProcessed);
        assert_eq!(signature, already_processed_sig);
    }

    enum TxResult {
        ExecutedWithSuccess,
        ExecutedWithFailure,
        NotExecuted,
    }

    #[test_matrix(
        [TxResult::ExecutedWithSuccess, TxResult::ExecutedWithFailure, TxResult::NotExecuted]
    )]
    fn test_execute_batch_cancels_commit_on_processing_error(tx_result: TxResult) {
        agave_logger::setup();
        let dummy_leader_pubkey = solana_pubkey::new_rand();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_with_leader(500, &dummy_leader_pubkey, 100);
        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let bank = Arc::new(bank);
        let pubkey = solana_pubkey::new_rand();
        let (tx, expected_tx_result) = match tx_result {
            TxResult::ExecutedWithSuccess => (
                RuntimeTransaction::from_transaction_for_tests(
                    solana_system_transaction::transfer(
                        &mint_keypair,
                        &pubkey,
                        1,
                        genesis_config.hash(),
                    ),
                ),
                Ok(()),
            ),
            TxResult::ExecutedWithFailure => (
                RuntimeTransaction::from_transaction_for_tests(
                    solana_system_transaction::transfer(
                        &mint_keypair,
                        &pubkey,
                        100000000,
                        genesis_config.hash(),
                    ),
                ),
                Ok(()),
            ),
            TxResult::NotExecuted => (
                RuntimeTransaction::from_transaction_for_tests(
                    solana_system_transaction::transfer(&mint_keypair, &pubkey, 1, Hash::default()),
                ),
                Err(TransactionError::BlockhashNotFound),
            ),
        };
        let mut batch = TransactionBatch::new(
            vec![Ok(()); 1],
            &bank,
            OwnedOrBorrowed::Borrowed(slice::from_ref(&tx)),
        );
        batch.set_needs_unlock(false);
        let batch = TransactionBatchWithIndexes {
            batch,
            transaction_indexes: vec![],
        };
        let mut timing = ExecuteTimings::default();
        let (sender, receiver) = bounded(1024);

        assert_eq!(bank.transaction_count(), 0);
        assert_eq!(bank.transaction_error_count(), 0);

        let result = execute_batch(
            &batch,
            &bank,
            Some(&TransactionStatusSender {
                sender,
                dependency_tracker: None,
            }),
            None,
            ReplayVoteSendType::VerifiedExecuted,
            &mut timing,
            None,
            None,
        );

        assert_eq!(result, expected_tx_result);
        if expected_tx_result.is_ok() {
            assert_eq!(bank.transaction_count(), 1);
            if matches!(tx_result, TxResult::ExecutedWithFailure) {
                assert_eq!(bank.transaction_error_count(), 1);
            } else {
                assert_eq!(bank.transaction_error_count(), 0);
            }
            assert_matches!(
                receiver.try_recv(),
                Ok(TransactionStatusMessage::Batch((TransactionStatusBatch{transaction_indexes, ..}, _sequence)))
                    if transaction_indexes.is_empty()
            );
        } else {
            // The pre-commit callback surfaced the processing error and
            // cancelled the commit
            assert_eq!(bank.transaction_count(), 0);
            assert_matches!(receiver.try_recv(), Err(_));
        }
    }

    #[test]
    fn test_check_noop_cost_units_and_replay() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let tx = system_transaction::transfer(
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            bank.last_blockhash(),
        );

        // one signature, two locks, instruction data
        let sig = SIGNATURE_COST;
        let locks = 2 * WRITE_LOCK_UNITS;
        let data = tx.message.instructions[0].data.len() as u64 / INSTRUCTION_DATA_BYTES_COST;

        let batch = bank.prepare_batch_for_tests(vec![tx.clone()]);

        // 3k compute for calling a builtin
        let compute =
            CostModel::calculate_cost(&batch.sanitized_transactions()[0], &bank.feature_set)
                .programs_execution_cost();

        // 64mb default loaded transaction data size limit
        let size = CostModel::calculate_loaded_accounts_data_size_cost(
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get(),
            &bank.feature_set,
        );

        let (commit_results, _) = bank.load_execute_and_commit_transactions(
            &batch,
            ExecutionRecordingConfig::new_single_setting(false),
            &mut ExecuteTimings::default(),
            None,
        );

        let committed = commit_results[0].as_ref().unwrap();
        assert_eq!(committed.status, Err(TransactionError::AccountNotFound));
        assert_eq!(committed.executed_units, compute);

        let tx_costs =
            get_transaction_costs(&bank, &commit_results, batch.sanitized_transactions());

        let noop_cost = tx_costs[0].as_ref().unwrap().sum();
        assert_eq!(noop_cost, sig + locks + data + compute + size);

        check_block_cost_limits(&bank, &tx_costs).unwrap();
        assert_eq!(bank.read_cost_tracker().unwrap().block_cost(), noop_cost);

        drop(batch);

        let (commit_results, _) = bank.load_execute_and_commit_transactions(
            &bank.prepare_batch_for_tests(vec![tx]),
            ExecutionRecordingConfig::new_single_setting(false),
            &mut ExecuteTimings::default(),
            None,
        );

        // no-ops are added to StatusCache and cannot be re-executed
        assert_eq!(
            commit_results,
            vec![Err(TransactionError::AlreadyProcessed)]
        );
    }
}
