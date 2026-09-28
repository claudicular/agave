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
    /// Executed over FL's own run of an unfrozen parent.
    pub chained: bool,
    pub status: Result<(), TransactionError>,
    /// Exactly the (pubkey, account) list agave's grouped notification would carry;
    /// `None` when agave sends no notification (no-op, unprocessable, empty).
    pub frame: Option<Vec<(Pubkey, AccountSharedData)>>,
    /// Per `frame` entry: written by the transaction (else a read-only account included
    /// because of its owner). Empty when there is no frame.
    pub frame_written: Vec<bool>,
    pub cu: u64,
    pub fee: u64,
    pub t_tap: Instant,
    pub t_tap_unix_ns: u64,
    pub from_ring: bool,
}

/// A chained run's link to its unfrozen parent.
pub struct ChainState {
    /// FL's (complete) run of the parent P.
    pub parent_run: Arc<Run>,
    /// Agave's bank P (unfrozen when the run started).
    pub parent_bank: Arc<Bank>,
    /// Accounts P's freeze writes, plus the SlotHashes sysvar: written by the run's
    /// pseudo-transaction 0 when agave freezes P.
    pub provisional_keys: Vec<Pubkey>,
    /// The child's SlotHashes account: provisional (placeholder parent hash) until resolved.
    pub slot_hashes_account: RwLock<AccountSharedData>,
    pub resolved: std::sync::atomic::AtomicBool,
}

pub struct Run {
    pub id: u64,
    pub slot: Slot,
    pub parent_slot: Slot,
    pub parent_bank_id: BankId,
    /// The frozen bank the run's state is rooted in: P, or P's parent for a chained run.
    pub parent: Arc<Bank>,
    pub ctx: Arc<FastLaneChildContext>,
    /// Index of the slot's first transaction (1 for a chained run: 0 is its pseudo-tx).
    pub ordinal_base: TxIdx,
    pub chain: Option<ChainState>,
    /// Every transaction is FINAL (set once, by the coordinator).
    pub complete: std::sync::atomic::AtomicBool,
    /// ... and none was unprocessable (a block agave would mark dead).
    pub complete_ok: std::sync::atomic::AtomicBool,
    pub complete_notify: Option<crossbeam_channel::Sender<Slot>>,
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

/// Base reads of a chained run: FL's final state of the parent P, else P's frozen parent.
pub struct ChainedBase(pub Arc<Run>);

impl crate::mv::BaseReader for ChainedBase {
    fn read(&self, key: &Pubkey) -> Option<(AccountSharedData, Slot)> {
        let vis = self.0.overlay.latest(key);
        vis.value.map(|account| (account, vis.slot))
    }
}

/// Fills the run processor's sysvar cache with the child's values.
pub struct SysvarFiller<'a> {
    pub ctx: &'a FastLaneChildContext,
    pub parent: &'a Bank,
    /// SlotHashes to use instead of the context's (a chained run's resolved value).
    pub slot_hashes: Option<AccountSharedData>,
}

impl TransactionProcessingCallback for SysvarFiller<'_> {
    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, Slot)> {
        if *pubkey == solana_sdk_ids::sysvar::slot_hashes::id() {
            if let Some(account) = &self.slot_hashes {
                return Some((account.clone(), self.ctx.child_slot));
            }
        }
        if let Some(account) = self.ctx.sysvar_override(pubkey) {
            return Some((account.clone(), self.ctx.child_slot));
        }
        self.parent.get_account_modified_slot(pubkey)
    }
}

/// An account value no real account can have: a provisional read recorded with it always
/// fails validation, forcing re-execution once the true value is known.
fn poison_account() -> AccountSharedData {
    AccountSharedData::new(u64::MAX, 0, &Pubkey::new_from_array([0xfe; 32]))
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
        // A chained run's SlotHashes is provisional until the parent is frozen: recorded as
        // a read so the pseudo-transaction that installs the true value validates it.
        if let Some(chain) = &self.run.chain {
            if *pubkey == solana_sdk_ids::sysvar::slot_hashes::id() {
                if let Some(&i) = self.index.borrow().get(pubkey) {
                    return self.reads.borrow()[i].value.clone().map(|a| (a, self.run.slot));
                }
                let account = chain.slot_hashes_account.read().clone();
                let mut reads = self.reads.borrow_mut();
                self.index.borrow_mut().insert(*pubkey, reads.len());
                self.slots.borrow_mut().push(self.run.slot);
                reads.push(Read {
                    key: *pubkey,
                    origin: crate::mv::Origin::Base,
                    value: Some(account.clone()),
                });
                return Some((account, self.run.slot));
            }
        }
        // The child's Clock/SlotHashes/RecentBlockhashes (never written by transactions).
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
        let ctx = Arc::new(parent.fast_lane_child_context(slot)?);
        // Entries FL may have built at the parent's deployment slot are dropped: this run
        // reloads such programs from the frozen parent (agave's state) instead.
        programs.prune_slot(parent.slot());
        let processor = programs.processor_for(parent, slot)?;
        processor.fill_missing_sysvar_cache_entries(&SysvarFiller {
            ctx: &ctx,
            parent,
            slot_hashes: None,
        });
        if let Ok(mut graph) = programs.fork_graph().write() {
            graph.set_parent(slot, parent.slot());
        }
        let env = ctx.processing_environment();
        crate::mem::LIVE_RUNS.add(1);
        Ok(Self {
            id,
            slot,
            parent_slot: parent.slot(),
            parent_bank_id: parent.bank_id(),
            parent: Arc::clone(parent),
            overlay: Overlay::new(slot, Arc::new(BankBase(Arc::clone(parent)))),
            ctx,
            ordinal_base: 0,
            chain: None,
            complete: std::sync::atomic::AtomicBool::new(false),
            complete_ok: std::sync::atomic::AtomicBool::new(false),
            complete_notify: None,
            processor,
            env,
            txs: RwLock::new(Vec::new()),
            readonly_owners,
        })
    }

    /// Build a run for `slot` whose parent P (`parent_run.slot`) is **not frozen**: on top of
    /// FL's complete, non-chained run of P. `parent_bank` is agave's (unfrozen) bank P.
    /// Transaction 0 is the pseudo-transaction for P's freeze (see [`Self::provisional_meta`]).
    pub fn new_chained(
        id: u64,
        slot: Slot,
        parent_run: &Arc<Run>,
        parent_bank: &Arc<Bank>,
        last_blockhash: solana_hash::Hash,
        programs: &mut ProgramCaches,
        readonly_owners: Arc<Vec<Pubkey>>,
    ) -> Result<Self, FastLaneUnsupported> {
        if parent_run.chain.is_some() {
            return Err(FastLaneUnsupported::ParentNotFrozen);
        }
        let grandparent = &parent_run.parent;
        let vote_accounts: Vec<(Pubkey, AccountSharedData)> = parent_run
            .overlay
            .written_keys()
            .into_iter()
            .filter_map(|key| {
                let account = parent_run.overlay.latest_raw(&key)?;
                solana_sdk_ids::vote::check_id(account.owner()).then_some((key, account))
            })
            .collect();
        let ctx = Arc::new(grandparent.fast_lane_chained_child_context(
            &solana_runtime::bank::fast_lane::FastLaneChainedParent {
                ctx: Arc::clone(&parent_run.ctx),
                last_blockhash,
                lamports_per_signature: parent_bank.fast_lane_lamports_per_signature(),
                vote_accounts,
            },
            slot,
        )?);
        let processor = programs.processor_for(grandparent, slot)?;
        let base: Arc<dyn crate::mv::BaseReader> = Arc::new(ChainedBase(Arc::clone(parent_run)));
        // Sysvars other than the overridden ones are the grandparent's (P changes none).
        processor.fill_missing_sysvar_cache_entries(&SysvarFiller {
            ctx: &ctx,
            parent: grandparent,
            slot_hashes: None,
        });
        if let Ok(mut graph) = programs.fork_graph().write() {
            graph.set_parent(slot, parent_run.slot);
        }
        let env = ctx.processing_environment();
        #[allow(deprecated)]
        let provisional_keys = vec![
            parent_bank
                .fast_lane_collector_id()
                .ok_or(FastLaneUnsupported::UnknownFeeCollector)?,
            solana_sdk_ids::sysvar::slot_history::id(),
            solana_sdk_ids::incinerator::id(),
            solana_sdk_ids::sysvar::slot_hashes::id(),
        ];
        let slot_hashes_account = ctx.slot_hashes_account.clone();
        crate::mem::LIVE_RUNS.add(1);
        Ok(Self {
            id,
            slot,
            parent_slot: parent_run.slot,
            parent_bank_id: parent_bank.bank_id(),
            parent: Arc::clone(grandparent),
            overlay: Overlay::new(slot, base),
            ctx,
            ordinal_base: 1,
            chain: Some(ChainState {
                parent_run: Arc::clone(parent_run),
                parent_bank: Arc::clone(parent_bank),
                provisional_keys,
                slot_hashes_account: RwLock::new(slot_hashes_account),
                resolved: std::sync::atomic::AtomicBool::new(false),
            }),
            complete: std::sync::atomic::AtomicBool::new(false),
            complete_ok: std::sync::atomic::AtomicBool::new(false),
            complete_notify: None,
            processor,
            env,
            txs: RwLock::new(Vec::new()),
            readonly_owners,
        })
    }

    /// Scheduling metadata of a chained run's pseudo-transaction 0: it write-locks every
    /// provisional key and completes when agave freezes the parent.
    pub fn provisional_meta(&self) -> Option<TxMeta> {
        let chain = self.chain.as_ref()?;
        Some(TxMeta {
            locks: chain.provisional_keys.iter().map(|k| (*k, true)).collect(),
            certain_writes: chain.provisional_keys.clone(),
            is_vote: false,
            external: true,
        })
    }

    /// The parent was frozen as `frozen_parent`: switch to the true SlotHashes and return
    /// the pseudo-transaction's writes (the frozen parent's values of the provisional keys).
    pub fn resolve_parent_freeze(
        &self,
        frozen_parent: &Bank,
    ) -> Option<Vec<(Pubkey, AccountSharedData)>> {
        let chain = self.chain.as_ref()?;
        let (_, slot_hashes) = self
            .parent
            .fast_lane_resolve_slot_hashes(&self.ctx, frozen_parent.hash())?;
        *chain.slot_hashes_account.write() = slot_hashes.clone();
        // Workers hold the sysvar cache's read lock while executing; this waits for them.
        self.processor.reset_and_fill_sysvar_cache_entries(&SysvarFiller {
            ctx: &self.ctx,
            parent: &self.parent,
            slot_hashes: Some(slot_hashes.clone()),
        });
        chain
            .resolved
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Some(
            chain
                .provisional_keys
                .iter()
                .map(|key| {
                    let value = if *key == solana_sdk_ids::sysvar::slot_hashes::id() {
                        slot_hashes.clone()
                    } else {
                        frozen_parent.get_account(key).unwrap_or_default()
                    };
                    (*key, value)
                })
                .collect(),
        )
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
                let verified = match &self.chain {
                    None => self
                        .parent
                        .fast_lane_verify_transaction(&self.ctx, tx, &serialized),
                    // Lookup tables as of the end of the unfrozen parent (FL's run of it).
                    Some(chain) => {
                        let base = ChainedBase(Arc::clone(&chain.parent_run));
                        self.parent.fast_lane_verify_transaction_with(
                            &self.ctx,
                            tx,
                            &serialized,
                            &|key: &Pubkey| {
                                crate::mv::BaseReader::read(&base, key).map(|(a, _)| a)
                            },
                        )
                    }
                };
                let rtx = match verified {
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
                    external: false,
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

    /// The transaction at run index `k` (`None` for a chained run's pseudo-transaction).
    pub fn tx(&self, k: TxIdx) -> Option<Arc<TxEntry>> {
        let i = k.checked_sub(self.ordinal_base)?;
        self.txs.read().get(i as usize).cloned()
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

impl Drop for Run {
    fn drop(&mut self) {
        crate::mem::LIVE_RUNS.sub(1);
    }
}

impl SchedRun for Run {
    fn overlay(&self) -> &Overlay {
        &self.overlay
    }

    fn on_complete(&self, summary: &crate::sched::RunSummary) {
        self.complete_ok
            .store(summary.unprocessable == 0, std::sync::atomic::Ordering::SeqCst);
        self.complete
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(notify) = &self.complete_notify {
            let _ = notify.try_send(self.slot);
        }
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
        let chain_resolved_before = self
            .chain
            .as_ref()
            .map(|c| c.resolved.load(std::sync::atomic::Ordering::SeqCst));
        let slot_hashes_count_before =
            solana_program_runtime::sysvar_cache::slot_hashes_access_count();
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
        // A chained run's SlotHashes is provisional: an execution that read it through the
        // sysvar cache (syscall, builtin) records it as a read of the SlotHashes account.
        if let (Some(chain), Some(resolved_before)) = (&self.chain, chain_resolved_before) {
            if solana_program_runtime::sysvar_cache::slot_hashes_access_count()
                != slot_hashes_count_before
            {
                // Resolved before execution started: the sysvar cache (read-locked for the
                // whole execution) and the account both held the true value. Otherwise
                // the cache may have held the placeholder whatever the account read
                // returned, so the read is recorded with a value that never validates.
                let resolved_after = chain.resolved.load(std::sync::atomic::Ordering::SeqCst);
                let key = solana_sdk_ids::sysvar::slot_hashes::id();
                let existing = callback.index.borrow().get(&key).copied();
                let mut reads = callback.reads.borrow_mut();
                if resolved_before && resolved_after {
                    if existing.is_none() {
                        callback.index.borrow_mut().insert(key, reads.len());
                        reads.push(Read {
                            key,
                            origin: crate::mv::Origin::Base,
                            value: Some(chain.slot_hashes_account.read().clone()),
                        });
                    }
                } else if let Some(i) = existing {
                    reads[i].value = Some(poison_account());
                } else {
                    callback.index.borrow_mut().insert(key, reads.len());
                    reads.push(Read {
                        key,
                        origin: crate::mv::Origin::Base,
                        value: Some(poison_account()),
                    });
                }
            }
        }
        let rtx = &entry.rtx;
        let mut outcome = TxOutcome {
            slot: self.slot,
            parent_slot: self.parent_slot,
            ordinal: k - self.ordinal_base,
            signature: entry.signature,
            is_vote: entry.is_vote,
            programs: entry.programs.clone(),
            kind: OutcomeKind::Unprocessable,
            chained: self.chain.is_some(),
            status: Ok(()),
            frame: None,
            frame_written: Vec::new(),
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
                            outcome.frame_written.push(is_written);
                        }
                    }
                    outcome.frame = (!frame.is_empty()).then_some(frame);
                } else {
                    for (key, account) in loaded.rollback_accounts.iter() {
                        writes.push((*key, account.clone()));
                    }
                    outcome.frame = (!writes.is_empty()).then(|| writes.clone());
                    outcome.frame_written = vec![true; writes.len()];
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
                outcome.frame_written = vec![true; writes.len()];
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
