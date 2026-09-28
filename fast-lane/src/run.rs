//! One fast-lane run: slot N executed over frozen parent P with agave's SVM.
//!
//! [`Run`] implements [`SchedRun`]: an incarnation executes one transaction as a one-tx
//! batch through `TransactionBatchProcessor::load_and_execute_sanitized_transactions`, with
//! [`FlCallback`] resolving every account through the multi-version overlay (recording each
//! read) and delegating epoch-stake/precompile questions to P, whose answers equal the
//! child's within an epoch. Its write set is agave's store filter
//! (`account_saver::collect_accounts_to_store`) and its frame is agave's grouped-notification
//! rule (`Bank::notify_transaction_accounts_to_plugins`).

use {
    crate::{
        forks::FlForkGraph,
        mv::{Overlay, Read, TxIdx},
        program_cache::ProgramCaches,
        sched::{ExecOutput, SchedRun, TxMeta},
    },
    solana_accounts_db::account_locks::validate_account_locks,
    solana_entry::entry::Entry,
    parking_lot::RwLock,
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::{BankId, Slot},
    solana_precompile_error::PrecompileError,
    solana_pubkey::Pubkey,
    solana_runtime::bank::{
        Bank,
        fast_lane::{FastLaneChildContext, FastLaneStaticCheck, FastLaneUnsupported},
    },
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_signature::Signature,
    solana_svm::{
        transaction_processing_result::ProcessedTransaction,
        transaction_processor::{
            TransactionBatchProcessor, TransactionProcessingConfig,
            TransactionProcessingEnvironment,
        },
    },
    solana_svm_callback::{InvokeContextCallback, TransactionProcessingCallback},
    solana_svm_transaction::svm_message::{SVMMessage, SVMStaticMessage},
    solana_transaction::sanitized::SanitizedTransaction,
    solana_transaction_error::TransactionError,
    std::{
        cell::RefCell,
        collections::HashMap,
        sync::Arc,
        time::Instant,
    },
};

/// A sanitized transaction of the run with its static check result.
pub struct TxEntry {
    pub rtx: RuntimeTransaction<SanitizedTransaction>,
    pub check: FastLaneStaticCheck,
    pub signature: Signature,
    pub is_vote: bool,
    /// Top-level program ids (for per-program exactness breakdowns).
    pub programs: Vec<Pubkey>,
    pub t_tap: Instant,
    pub t_tap_unix_ns: u64,
    /// Delivered by the proxy ring (tap time = ring publish time) rather than the blockstore.
    pub from_ring: bool,
}

/// What happened to a transaction, as agave would commit it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeKind {
    Executed,
    FeesOnly,
    NoOp,
    /// Agave would reject the block.
    Unprocessable,
}

/// Per-transaction result carried to the comparator at FINAL.
pub struct TxOutcome {
    pub slot: Slot,
    pub parent_slot: Slot,
    pub ordinal: TxIdx,
    pub signature: Signature,
    pub is_vote: bool,
    pub programs: Vec<Pubkey>,
    pub kind: OutcomeKind,
    pub status: Result<(), TransactionError>,
    /// Exactly the (pubkey, account) list agave's grouped notification would carry;
    /// `None` when agave sends no notification (no-op, unprocessable, empty).
    pub frame: Option<Vec<(Pubkey, AccountSharedData)>>,
    pub cu: u64,
    pub fee: u64,
    pub t_tap: Instant,
    pub t_tap_unix_ns: u64,
    pub from_ring: bool,
}

pub struct Run {
    pub id: u64,
    pub slot: Slot,
    pub parent_slot: Slot,
    pub parent_bank_id: BankId,
    pub parent: Arc<Bank>,
    pub ctx: FastLaneChildContext,
    pub processor: TransactionBatchProcessor<FlForkGraph>,
    pub env: TransactionProcessingEnvironment,
    pub overlay: Overlay,
    pub txs: RwLock<Vec<Arc<TxEntry>>>,
    pub readonly_owners: Arc<Vec<Pubkey>>,
}

/// Base reads come from the frozen parent (non-fixed-root load: FL runs off the replay
/// thread).
pub struct BankBase(pub Arc<Bank>);

impl crate::mv::BaseReader for BankBase {
    fn read(&self, key: &Pubkey) -> Option<(AccountSharedData, Slot)> {
        self.0.get_account_modified_slot(key)
    }
}

/// Fills the run processor's sysvar cache with the child's values.
pub struct SysvarFiller<'a> {
    pub ctx: &'a FastLaneChildContext,
    pub parent: &'a Bank,
}

impl TransactionProcessingCallback for SysvarFiller<'_> {
    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, Slot)> {
        if let Some(account) = self.ctx.sysvar_override(pubkey) {
            return Some((account.clone(), self.ctx.child_slot));
        }
        self.parent.get_account_modified_slot(pubkey)
    }
}

/// SVM callback of one incarnation.
struct FlCallback<'a> {
    run: &'a Run,
    k: TxIdx,
    reads: RefCell<Vec<Read>>,
    slots: RefCell<Vec<Slot>>,
    index: RefCell<HashMap<Pubkey, usize>>,
}

impl<'a> FlCallback<'a> {
    fn new(run: &'a Run, k: TxIdx) -> Self {
        Self {
            run,
            k,
            reads: RefCell::new(Vec::with_capacity(40)),
            slots: RefCell::new(Vec::with_capacity(40)),
            index: RefCell::new(HashMap::with_capacity(40)),
        }
    }

    fn load(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, Slot)> {
        // The child's Clock/SlotHashes (never written by transactions).
        if let Some(account) = self.run.ctx.sysvar_override(pubkey) {
            return Some((account.clone(), self.run.slot));
        }
        // Repeat reads within one incarnation return the first value (the overlay may move
        // concurrently; the SVM must see one consistent value per key).
        if let Some(&i) = self.index.borrow().get(pubkey) {
            let reads = self.reads.borrow();
            let slot = self.slots.borrow()[i];
            return reads[i].value.clone().map(|account| (account, slot));
        }
        let vis = self.run.overlay.visible_below(pubkey, self.k);
        let mut reads = self.reads.borrow_mut();
        self.index.borrow_mut().insert(*pubkey, reads.len());
        self.slots.borrow_mut().push(vis.slot);
        reads.push(Read {
            key: *pubkey,
            origin: vis.origin,
            value: vis.value.clone(),
        });
        vis.value.map(|account| (account, vis.slot))
    }
}

impl TransactionProcessingCallback for FlCallback<'_> {
    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, Slot)> {
        self.load(pubkey)
    }
}

impl InvokeContextCallback for FlCallback<'_> {
    fn get_epoch_stake(&self) -> u64 {
        InvokeContextCallback::get_epoch_stake(&*self.run.parent)
    }
    fn get_epoch_stake_for_vote_account(&self, vote_address: &Pubkey) -> u64 {
        InvokeContextCallback::get_epoch_stake_for_vote_account(&*self.run.parent, vote_address)
    }
    fn is_precompile(&self, program_id: &Pubkey) -> bool {
        InvokeContextCallback::is_precompile(&*self.run.parent, program_id)
    }
    fn process_precompile(
        &self,
        program_id: &Pubkey,
        data: &[u8],
        instruction_datas: Vec<&[u8]>,
    ) -> Result<(), PrecompileError> {
        InvokeContextCallback::process_precompile(
            &*self.run.parent,
            program_id,
            data,
            instruction_datas,
        )
    }
}

impl Run {
    /// Build a run for `slot` over the frozen `parent`: child context, per-run processor
    /// sharing the private program cache (with the child's sysvar cache), overlay.
    pub fn new(
        id: u64,
        slot: Slot,
        parent: &Arc<Bank>,
        programs: &mut ProgramCaches,
        readonly_owners: Arc<Vec<Pubkey>>,
    ) -> Result<Self, FastLaneUnsupported> {
        let ctx = parent.fast_lane_child_context(slot)?;
        // Entries FL may have built at the parent's deployment slot are dropped: this run
        // reloads such programs from the frozen parent (agave's state) instead.
        programs.prune_slot(parent.slot());
        let processor = programs.processor_for(parent, slot)?;
        processor.fill_missing_sysvar_cache_entries(&SysvarFiller {
            ctx: &ctx,
            parent,
        });
        if let Ok(mut graph) = programs.fork_graph().write() {
            graph.set_parent(slot, parent.slot());
        }
        let env = ctx.processing_environment();
        Ok(Self {
            id,
            slot,
            parent_slot: parent.slot(),
            parent_bank_id: parent.bank_id(),
            parent: Arc::clone(parent),
            overlay: Overlay::new(slot, Arc::new(BankBase(Arc::clone(parent)))),
            ctx,
            processor,
            env,
            txs: RwLock::new(Vec::new()),
            readonly_owners,
        })
    }

    /// Sanitize `entries` exactly as replay does and append their transactions. Returns
    /// the scheduling metadata of the appended transactions (in order), or the reason the
    /// block is invalid (agave will mark it dead) together with the metadata of the
    /// transactions appended before the failure.
    pub fn push_entries(
        &self,
        entries: Vec<Entry>,
        t_tap: Instant,
        t_tap_unix_ns: u64,
        from_ring: bool,
    ) -> (Vec<TxMeta>, Option<&'static str>) {
        let mut metas = Vec::new();
        let mut appended = Vec::new();
        let mut failure = None;
        'entries: for entry in entries {
            for tx in entry.transactions {
                let serialized = tx.message.serialize();
                let signature = tx.signatures.first().copied().unwrap_or_default();
                let rtx = match self
                    .parent
                    .fast_lane_verify_transaction(&self.ctx, tx, &serialized)
                {
                    Ok(rtx) => rtx,
                    Err(err) => {
                        log::warn!(
                            "fast lane: slot {} tx {signature} sanitize failed: {err}",
                            self.slot
                        );
                        failure = Some("sanitize");
                        break 'entries;
                    }
                };
                if validate_account_locks(rtx.account_keys(), self.ctx.transaction_account_lock_limit)
                    .is_err()
                {
                    failure = Some("account_locks");
                    break 'entries;
                }
                let check = self.parent.fast_lane_check_static(&self.ctx, &rtx);
                let locks: Vec<(Pubkey, bool)> = rtx
                    .account_keys()
                    .iter()
                    .enumerate()
                    .map(|(i, key)| (*key, rtx.is_writable(i)))
                    .collect();
                let mut certain_writes = vec![*rtx.fee_payer()];
                if let Some(nonce) = rtx.get_durable_nonce() {
                    certain_writes.push(*nonce);
                }
                let mut programs: Vec<Pubkey> = rtx
                    .program_instructions_iter()
                    .map(|(program, _)| *program)
                    .collect();
                programs.sort_unstable();
                programs.dedup();
                let is_vote = rtx.is_simple_vote_transaction();
                metas.push(TxMeta {
                    locks,
                    certain_writes,
                    is_vote,
                });
                appended.push(Arc::new(TxEntry {
                    rtx,
                    check,
                    signature,
                    is_vote,
                    programs,
                    t_tap,
                    t_tap_unix_ns,
                    from_ring,
                }));
            }
        }
        self.txs.write().extend(appended);
        (metas, failure)
    }

    pub fn tx(&self, k: TxIdx) -> Option<Arc<TxEntry>> {
        self.txs.read().get(k as usize).cloned()
    }

    pub fn len(&self) -> usize {
        self.txs.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.txs.read().is_empty()
    }
}

fn processing_config() -> TransactionProcessingConfig<'static> {
    // Replay's configuration (`Bank::do_load_execute_and_commit_...`), except that program
    // deployment slots are checked (the private cache is seeded from agave's rooted entries
    // and must never hand out an entry older than the program data it reads).
    TransactionProcessingConfig {
        account_overrides: None,
        check_program_deployment_slot: true,
        log_messages_bytes_limit: None,
        limit_to_load_programs: false,
        recording_config: Default::default(),
        drop_on_failure: false,
        all_or_nothing: false,
        strict_nonce_size_check: false,
        drop_noop_transactions: false,
    }
}

impl SchedRun for Run {
    fn overlay(&self) -> &Overlay {
        &self.overlay
    }

    fn execute(&self, k: TxIdx, _inc: u32) -> ExecOutput {
        let exec_start = Instant::now();
        let Some(entry) = self.tx(k) else {
            // Cannot happen: the coordinator only dispatches ingested transactions.
            return ExecOutput {
                reads: Vec::new(),
                writes: Vec::new(),
                payload: Box::new(()),
                unprocessable: true,
                exec_start,
                exec_end: Instant::now(),
            };
        };
        let callback = FlCallback::new(self, k);
        let check_result = match &entry.check {
            FastLaneStaticCheck::Ready(details) => Ok(details.clone()),
            FastLaneStaticCheck::NeedsNonce(limits) => {
                Bank::fast_lane_check_nonce(&self.ctx, &entry.rtx, *limits, |address| {
                    callback.load(address).map(|(account, _)| account)
                })
            }
            FastLaneStaticCheck::Err(err) => Err(err.clone()),
        };
        let output = self.processor.load_and_execute_sanitized_transactions(
            &callback,
            std::slice::from_ref(&entry.rtx),
            vec![check_result],
            &self.env,
            &processing_config(),
        );
        let result = output.processing_results.into_iter().next();
        let rtx = &entry.rtx;
        let mut outcome = TxOutcome {
            slot: self.slot,
            parent_slot: self.parent_slot,
            ordinal: k,
            signature: entry.signature,
            is_vote: entry.is_vote,
            programs: entry.programs.clone(),
            kind: OutcomeKind::Unprocessable,
            status: Ok(()),
            frame: None,
            cu: 0,
            fee: 0,
            t_tap: entry.t_tap,
            t_tap_unix_ns: entry.t_tap_unix_ns,
            from_ring: entry.from_ring,
        };
        let mut writes = Vec::new();
        let mut unprocessable = false;
        match result {
            Some(Ok(ProcessedTransaction::Executed(executed))) => {
                outcome.kind = OutcomeKind::Executed;
                outcome.status = executed.execution_details.status.clone();
                outcome.cu = executed.execution_details.executed_units;
                outcome.fee = executed.loaded_transaction.fee_details.total_fee();
                let loaded = &executed.loaded_transaction;
                if executed.execution_details.status.is_ok() {
                    let touched = &loaded.touched_flags;
                    let mut frame = Vec::new();
                    for (i, (key, account)) in loaded.accounts.iter().enumerate() {
                        let is_written = rtx.is_writable(i) && touched.get(i).copied().unwrap_or(false);
                        // Store filter (account_saver::collect_accounts_for_successful_tx).
                        if is_written && !(rtx.is_invoked(i) && !rtx.is_instruction_account(i)) {
                            writes.push((*key, account.clone()));
                        }
                        // Notification filter (notify_transaction_accounts_to_plugins).
                        let owner_in_filter = !self.readonly_owners.is_empty()
                            && self.readonly_owners.contains(account.owner());
                        if is_written || owner_in_filter {
                            frame.push((*key, account.clone()));
                        }
                    }
                    outcome.frame = (!frame.is_empty()).then_some(frame);
                } else {
                    for (key, account) in loaded.rollback_accounts.iter() {
                        writes.push((*key, account.clone()));
                    }
                    outcome.frame = (!writes.is_empty()).then(|| writes.clone());
                }
            }
            Some(Ok(ProcessedTransaction::FeesOnly(fees_only))) => {
                outcome.kind = OutcomeKind::FeesOnly;
                outcome.status = Err(fees_only.load_error.clone());
                outcome.fee = fees_only.fee_details.total_fee();
                for (key, account) in fees_only.rollback_accounts.iter() {
                    writes.push((*key, account.clone()));
                }
                outcome.frame = (!writes.is_empty()).then(|| writes.clone());
            }
            Some(Ok(ProcessedTransaction::NoOp(no_op))) => {
                outcome.kind = OutcomeKind::NoOp;
                outcome.status = Err(no_op.validation_error.clone());
            }
            Some(Err(err)) => {
                outcome.status = Err(err);
                unprocessable = true;
            }
            None => {
                unprocessable = true;
            }
        }
        ExecOutput {
            reads: callback.reads.into_inner(),
            writes,
            payload: Box::new(outcome),
            unprocessable,
            exec_start,
            exec_end: Instant::now(),
        }
    }
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn check<T: Send + Sync>() {}
    check::<Run>();
    let _ = |a: &AccountSharedData| a.lamports();
}
