//! Integration points between agave's replay and the fast lane (`agave-fast-lane`, which
//! depends on this crate and installs [`FastLaneHooks`] at startup). Everything here is inert
//! unless the fast lane installed its hooks and set a mode: the replay hot path then pays
//! one relaxed atomic load per transaction.
//!
//! Modes (process-wide, switchable at runtime; a bank's commit mode is fixed when it is
//! inserted into `BankForks`):
//! - [`MODE_OFF`]: nothing.
//! - [`MODE_SHADOW`] (milestone 1): after agave executes and commits a replayed transaction,
//!   a copy of everything its commit consumed (the full `TransactionProcessingResult`: post
//!   accounts, fee details, rollback accounts, status, logs, inner instructions, return data,
//!   executed units, account-size deltas, loaded data size, modified programs) plus its
//!   balances and cost is handed to the fast lane, whose comparator checks it field by field
//!   against the fast lane's own result for the same transaction. Nothing else changes.
//! - [`MODE_ON`] (milestone 2, "execute once"): the fast lane commits its own validated
//!   results into agave's bank N through agave's commit path
//!   (`transaction_execution::commit_external`), and agave's replay of N follows.
//!
//! ## Commit protocol (MODE_ON)
//!
//! Every bank inserted into `BankForks` while the mode is on gets a [`Board`]: one cell per
//! transaction index of the slot. A cell is claimed exactly once, with a compare-and-swap,
//! either by the fast lane (`FL_CLAIMED`, then `FL_DONE`) or by agave's replay
//! (`AGAVE_CLAIMED`, then `AGAVE_DONE`). The fast lane may also decline a cell
//! (`FL_DECLINED`: excluded class, sample, runtime-check difference), which agave then
//! claims. Replay's task handler ([`follow`]) returns the stored result of a cell the fast
//! lane committed (after checking it is the same transaction), claims a free cell when the
//! fast lane will not commit it (board unbound or abandoned, fast lane off or poisoned,
//! statically excluded, sampled, or not claimed within the follow timeout), and otherwise
//! waits for the fast lane.
//!
//! Ordering: the fast lane commits transaction k only after every earlier transaction of
//! the slot that conflicts with k (shares an account, one of the two writing it) is done,
//! by either side; replay's scheduler runs a task only after its conflicting predecessors
//! completed (and a task the fast lane committed completes only when its commit is done).
//! So every account sees its writes in block order and agave's own executions read exactly
//! the state serial replay would. Any mix of fast-lane and agave commits yields serial
//! replay's state.
//!
//! Safety: a commit that unwinds marks its cell failed; an identity or sample mismatch, a
//! fast-lane commit agave's replay never verified, or a replay error on a bank the fast lane
//! committed into makes replay clear the bank and replay the slot without the fast lane
//! ([`close_bank`], [`abandon_bank`], [`mark_agave_only`]). Poisoning the fast lane stops
//! commit mode at once: agave's handlers stop waiting and claim every free cell.

use {
    crate::{bank::Bank, transaction_balances::compile_collected_balances},
    log::{error, warn},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::{BankId, Slot},
    solana_hash::Hash,
    solana_program_runtime::program_cache_entry::{ProgramCacheEntry, ProgramCacheEntryType},
    solana_pubkey::Pubkey,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, loader_v4},
    solana_signature::Signature,
    solana_svm::{
        transaction_balances::BalanceCollector,
        transaction_processing_result::{ProcessedTransaction, TransactionProcessingResult},
    },
    solana_svm_transaction::svm_message::SVMMessage,
    solana_transaction_error::{TransactionError, TransactionResult},
    solana_transaction_status::TransactionTokenBalance,
    std::{
        cell::RefCell,
        collections::{HashMap, VecDeque},
        sync::{
            Arc, Mutex, OnceLock, RwLock,
            atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    },
};

pub const MODE_OFF: u8 = 0;
pub const MODE_SHADOW: u8 = 1;
pub const MODE_ON: u8 = 2;

static MODE: AtomicU8 = AtomicU8::new(MODE_OFF);
static HOOKS: RwLock<Option<Arc<dyn FastLaneHooks>>> = RwLock::new(None);
/// The fast lane will commit (active, not poisoned, commit mode on). Agave's handlers stop
/// waiting for it the moment this turns false.
static FL_LIVE: AtomicBool = AtomicBool::new(false);
/// Parts per million of transactions agave executes itself and compares with the fast
/// lane's result (snapshotted per bank at insertion).
static SAMPLE_PPM: AtomicU32 = AtomicU32::new(0);
/// How long a replay handler waits for the fast lane to claim a free cell before claiming it.
static FOLLOW_WAIT_US: AtomicU64 = AtomicU64::new(100_000);
/// How long a sampled transaction's handler waits for the fast lane's result to compare.
static SAMPLE_WAIT_US: AtomicU64 = AtomicU64::new(200_000);
/// How long a waiting replay handler spins before sleeping between checks (a handler that
/// waits for the fast lane then costs almost no CPU).
static FOLLOW_SPIN_US: AtomicU64 = AtomicU64::new(200);
/// How samples are chosen (snapshotted per bank): [`SAMPLE_HASH`] (deterministic on the
/// signature, both sides decide) or [`SAMPLE_FL`] (the fast lane picks transactions no later
/// transaction conflicts with and deposits its result; agave compares what it finds).
static SAMPLE_MODE: AtomicU8 = AtomicU8::new(SAMPLE_FL);
pub const SAMPLE_HASH: u8 = 0;
pub const SAMPLE_FL: u8 = 1;
/// How long a replay handler waits for the fast lane to bind a bank it has a run for (the
/// bank was just inserted; the fast lane's binding is on its way).
static BIND_WAIT_US: AtomicU64 = AtomicU64::new(2_000);
/// Slots (with the parent bank) the fast lane runs: a bank inserted for one is expected to be
/// bound by the fast lane shortly.
static EXPECTED: Mutex<VecDeque<(Slot, BankId)>> = Mutex::new(VecDeque::new());
/// Registered commit boards (fast path of the handler hooks when zero).
static COMMIT_BOARDS: AtomicUsize = AtomicUsize::new(0);

/// Implemented by the fast lane. Called on agave's replay threads: implementations must be
/// wait-free and must never panic (agave's panic hook exits the process).
pub trait FastLaneHooks: Send + Sync {
    /// Whether the fast lane wants agave's processing results of `slot` (shadow mode): replay
    /// does not even copy them otherwise (e.g. the catch-up backlog after a restart).
    fn wants_slot(&self, _slot: Slot) -> bool {
        true
    }
    /// Agave executed and committed transaction `index` of bank `slot` (shadow mode).
    fn on_agave_processed(&self, processed: AgaveProcessed);
    /// Agave's replay executed and committed transaction `index` it claimed on a commit
    /// board (commit mode).
    fn on_agave_done(&self, _slot: Slot, _bank_id: BankId, _index: usize) {}
    /// A bank was inserted into `BankForks` (its board, if any, exists already).
    fn on_bank_inserted(&self, _bank: &Arc<Bank>) {}
    /// A safety check failed: disable the fast lane permanently.
    fn poison(&self, _reason: &'static str) {}
}

/// Native and token balances of one transaction, as sent to the `TransactionStatusSender`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct TxBalances {
    pub pre: Vec<u64>,
    pub post: Vec<u64>,
    pub token_pre: Vec<TransactionTokenBalance>,
    pub token_post: Vec<TransactionTokenBalance>,
}

/// Everything agave's commit of one replayed transaction consumed, captured just before the
/// commit (shadow mode).
pub struct AgaveProcessed {
    pub slot: Slot,
    pub bank_id: BankId,
    pub parent_slot: Slot,
    /// Transaction index in the slot (agave's `transaction_indexes`).
    pub index: usize,
    pub signature: Signature,
    pub message_hash: Hash,
    pub result: TransactionProcessingResult,
    /// Present when agave records transaction statuses (a `TransactionStatusSender` exists).
    pub balances: Option<TxBalances>,
    /// Cost charged to the block's cost tracker (`None` when not processed).
    pub cost: Option<u64>,
    pub t: Instant,
}

/// Install the fast lane's hooks (replaces any previous ones).
pub fn install_hooks(hooks: Arc<dyn FastLaneHooks>) {
    if let Ok(mut slot) = HOOKS.write() {
        *slot = Some(hooks);
    }
}

/// Remove the hooks and turn every mode off.
pub fn uninstall_hooks() {
    MODE.store(MODE_OFF, Ordering::SeqCst);
    FL_LIVE.store(false, Ordering::SeqCst);
    if let Ok(mut slot) = HOOKS.write() {
        *slot = None;
    }
}

pub fn set_mode(mode: u8) {
    MODE.store(mode.min(MODE_ON), Ordering::SeqCst);
}

#[inline]
pub fn mode() -> u8 {
    MODE.load(Ordering::Relaxed)
}

/// Whether the fast lane will commit (set by the fast lane: active, not poisoned, mode on).
pub fn set_fl_live(live: bool) {
    FL_LIVE.store(live, Ordering::SeqCst);
}

#[inline]
pub fn fl_live() -> bool {
    FL_LIVE.load(Ordering::Acquire)
}

pub fn set_sample_ppm(ppm: u32) {
    SAMPLE_PPM.store(ppm.min(1_000_000), Ordering::Relaxed);
}

pub fn set_follow_wait(wait: Duration) {
    FOLLOW_WAIT_US.store(wait.as_micros() as u64, Ordering::Relaxed);
}

pub fn set_follow_spin(spin: Duration) {
    FOLLOW_SPIN_US.store(spin.as_micros() as u64, Ordering::Relaxed);
}

pub fn sample_mode() -> u8 {
    SAMPLE_MODE.load(Ordering::Relaxed)
}

pub fn sample_ppm() -> u32 {
    SAMPLE_PPM.load(Ordering::Relaxed)
}

pub fn set_sample_mode(mode: u8) {
    SAMPLE_MODE.store(mode.min(SAMPLE_FL), Ordering::Relaxed);
}

pub fn set_bind_wait(wait: Duration) {
    BIND_WAIT_US.store(wait.as_micros() as u64, Ordering::Relaxed);
}

/// The fast lane runs `slot` over the parent bank `parent_bank_id`: a bank of it inserted
/// meanwhile waits (bounded) for the fast lane's binding instead of being executed by agave.
pub fn expect_fl_run(slot: Slot, parent_bank_id: BankId) {
    if let Ok(mut expected) = EXPECTED.lock() {
        if !expected.contains(&(slot, parent_bank_id)) {
            expected.push_back((slot, parent_bank_id));
            while expected.len() > 256 {
                expected.pop_front();
            }
        }
    }
    // A bank inserted before the expectation: mark its board too.
    if COMMIT_BOARDS.load(Ordering::Acquire) > 0 {
        if let Some(boards) = BOARDS.read().ok().as_ref().and_then(|b| b.as_ref()) {
            for board in boards.values() {
                if board.slot == slot && board.parent_bank_id == Some(parent_bank_id) {
                    board.fl_expected.store(true, Ordering::Release);
                }
            }
        }
    }
}

/// The fast lane gave up on `slot`: nobody waits for its binding.
pub fn unexpect_fl_run(slot: Slot) {
    if let Ok(mut expected) = EXPECTED.lock() {
        expected.retain(|(s, _)| *s != slot);
    }
    if COMMIT_BOARDS.load(Ordering::Acquire) > 0 {
        if let Some(boards) = BOARDS.read().ok().as_ref().and_then(|b| b.as_ref()) {
            for board in boards.values() {
                if board.slot == slot {
                    board.fl_expected.store(false, Ordering::Release);
                }
            }
        }
    }
}

fn is_expected(slot: Slot, parent_bank_id: Option<BankId>) -> bool {
    parent_bank_id.is_some_and(|parent| {
        EXPECTED
            .lock()
            .map(|e| e.contains(&(slot, parent)))
            .unwrap_or(false)
    })
}

pub fn set_sample_wait(wait: Duration) {
    SAMPLE_WAIT_US.store(wait.as_micros() as u64, Ordering::Relaxed);
}

fn hooks() -> Option<Arc<dyn FastLaneHooks>> {
    HOOKS.read().ok()?.clone()
}

fn poison(reason: &'static str) {
    STATS.poisons.fetch_add(1, Ordering::Relaxed);
    FL_LIVE.store(false, Ordering::SeqCst);
    if let Some(hooks) = hooks() {
        hooks.poison(reason);
    }
}

/// Whether replay should capture its processing results of `slot` for the fast lane (shadow
/// mode, and the fast lane wants the slot).
#[inline]
pub fn capture_enabled(slot: Slot) -> bool {
    MODE.load(Ordering::Relaxed) == MODE_SHADOW && hooks().is_some_and(|h| h.wants_slot(slot))
}

/// A deep copy of a processing result (`ProcessedTransaction` is not `Clone`; its parts are).
pub fn clone_processing_result(result: &TransactionProcessingResult) -> TransactionProcessingResult {
    match result {
        Ok(ProcessedTransaction::Executed(executed)) => {
            Ok(ProcessedTransaction::Executed(Box::new((**executed).clone())))
        }
        Ok(ProcessedTransaction::FeesOnly(fees_only)) => {
            Ok(ProcessedTransaction::FeesOnly(Box::new((**fees_only).clone())))
        }
        Ok(ProcessedTransaction::NoOp(no_op)) => {
            Ok(ProcessedTransaction::NoOp(Box::new((**no_op).clone())))
        }
        Err(err) => Err(err.clone()),
    }
}

/// Per-transaction balances of a batch's balance collector (what `execute_batch` hands to the
/// `TransactionStatusSender`).
pub fn tx_balances(collector: BalanceCollector) -> Vec<TxBalances> {
    let (native, token) = compile_collected_balances(collector);
    native
        .pre_balances
        .into_iter()
        .zip(native.post_balances)
        .zip(token.pre_token_balances.into_iter().zip(token.post_token_balances))
        .map(|((pre, post), (token_pre, token_post))| TxBalances {
            pre,
            post,
            token_pre,
            token_post,
        })
        .collect()
}

/// Hand agave's processing results of a committed replay batch to the fast lane.
pub(crate) fn send_captures(
    bank: &Bank,
    transactions: &[impl TransactionWithMeta],
    transaction_indexes: &[usize],
    results: Vec<TransactionProcessingResult>,
    mut balances: Option<Vec<TxBalances>>,
    costs: Vec<Option<u64>>,
) {
    let Some(hooks) = hooks() else {
        return;
    };
    let t = Instant::now();
    for (i, (result, tx)) in results.into_iter().zip(transactions).enumerate() {
        let Some(&index) = transaction_indexes.get(i) else {
            break;
        };
        hooks.on_agave_processed(AgaveProcessed {
            slot: bank.slot(),
            bank_id: bank.bank_id(),
            parent_slot: bank.parent_slot(),
            index,
            signature: *tx.signature(),
            message_hash: *tx.message_hash(),
            result,
            balances: balances
                .as_mut()
                .and_then(|b| b.get_mut(i).map(std::mem::take)),
            cost: costs.get(i).copied().flatten(),
            t,
        });
    }
}

// ---------------------------------------------------------------------------------------
// Result comparison (the shadow check's fields; also the commit-mode sample verification).
// ---------------------------------------------------------------------------------------

/// Every field [`diff_results`] reports, in reporting order.
pub const DIFF_FIELDS: [&str; 18] = [
    "kind",
    "status",
    "accounts",
    "touched",
    "fee",
    "rollback",
    "loaded_size",
    "budget",
    "logs",
    "inner_ix",
    "return_data",
    "cu",
    "deltas",
    "programs",
    "bal_native",
    "bal_token",
    "noop",
    "parent",
];

/// Byte-exact account equality (lamports, owner, executable, rent epoch, data).
pub fn accounts_equal(a: &AccountSharedData, b: &AccountSharedData) -> bool {
    a.lamports() == b.lamports()
        && a.owner() == b.owner()
        && a.executable() == b.executable()
        && a.rent_epoch() == b.rent_epoch()
        && a.data() == b.data()
}

fn keyed_equal(a: &[(Pubkey, AccountSharedData)], b: &[(Pubkey, AccountSharedData)]) -> bool {
    a.len() == b.len()
        && a
            .iter()
            .zip(b)
            .all(|((ka, aa), (kb, ab))| ka == kb && accounts_equal(aa, ab))
}

fn program_type_tag(entry: &ProgramCacheEntry) -> u8 {
    match entry.program {
        ProgramCacheEntryType::FailedVerification(_) => 1,
        ProgramCacheEntryType::Closed => 2,
        ProgramCacheEntryType::DelayVisibility => 3,
        ProgramCacheEntryType::Unloaded(_) => 4,
        ProgramCacheEntryType::Loaded(_) => 5,
        ProgramCacheEntryType::Builtin(_) => 6,
    }
}

fn programs_equal(
    a: &HashMap<Pubkey, Arc<ProgramCacheEntry>>,
    b: &HashMap<Pubkey, Arc<ProgramCacheEntry>>,
) -> bool {
    a.len() == b.len()
        && a.iter().all(|(key, ea)| {
            b.get(key).is_some_and(|eb| {
                ea.deployment_slot == eb.deployment_slot
                    && ea.account_owner == eb.account_owner
                    && program_type_tag(ea) == program_type_tag(eb)
            })
        })
}

/// Compare two processing results of the same transaction; pushes the name of every
/// differing field. Recording fields (logs, inner instructions, return data, balances) are
/// compared only when `recorded` (both executed with recording on).
pub fn diff_results(
    a: &TransactionProcessingResult,
    a_balances: Option<&TxBalances>,
    recorded: bool,
    b: &TransactionProcessingResult,
    b_balances: Option<&TxBalances>,
    out: &mut Vec<&'static str>,
) {
    match (a, b) {
        (Err(a), Err(b)) => {
            if a != b {
                out.push("status");
            }
        }
        (Ok(a), Ok(b)) => match (a, b) {
            (ProcessedTransaction::Executed(a), ProcessedTransaction::Executed(b)) => {
                let (la, lb) = (&a.loaded_transaction, &b.loaded_transaction);
                let (ea, eb) = (&a.execution_details, &b.execution_details);
                if ea.status != eb.status {
                    out.push("status");
                }
                let before = out.len();
                if !keyed_equal(&la.accounts, &lb.accounts) {
                    out.push("accounts");
                }
                if la.touched_flags != lb.touched_flags {
                    out.push("touched");
                }
                if la.fee_details != lb.fee_details {
                    out.push("fee");
                }
                if la.rollback_accounts != lb.rollback_accounts {
                    out.push("rollback");
                }
                if la.loaded_accounts_data_size != lb.loaded_accounts_data_size {
                    out.push("loaded_size");
                }
                // The only other loaded-transaction field is the compute budget.
                if out.len() == before && la != lb {
                    out.push("budget");
                }
                if recorded {
                    if ea.log_messages != eb.log_messages {
                        out.push("logs");
                    }
                    if ea.inner_instructions != eb.inner_instructions {
                        out.push("inner_ix");
                    }
                    if ea.return_data != eb.return_data {
                        out.push("return_data");
                    }
                }
                if ea.executed_units != eb.executed_units {
                    out.push("cu");
                }
                if ea.accounts_deltas != eb.accounts_deltas {
                    out.push("deltas");
                }
                if !programs_equal(&a.programs_modified_by_tx, &b.programs_modified_by_tx) {
                    out.push("programs");
                }
            }
            (ProcessedTransaction::FeesOnly(a), ProcessedTransaction::FeesOnly(b)) => {
                if a.load_error != b.load_error {
                    out.push("status");
                }
                if a.rollback_accounts != b.rollback_accounts {
                    out.push("rollback");
                }
                if a.fee_details != b.fee_details {
                    out.push("fee");
                }
                if a.loaded_accounts_data_size != b.loaded_accounts_data_size {
                    out.push("loaded_size");
                }
            }
            (ProcessedTransaction::NoOp(a), ProcessedTransaction::NoOp(b)) => {
                if a.validation_error != b.validation_error {
                    out.push("status");
                } else if a != b {
                    out.push("noop");
                }
            }
            _ => out.push("kind"),
        },
        _ => out.push("kind"),
    }
    if recorded {
        diff_balances(a_balances, b_balances, out);
    }
}

pub fn diff_balances(a: Option<&TxBalances>, b: Option<&TxBalances>, out: &mut Vec<&'static str>) {
    match (a, b) {
        (Some(a), Some(b)) => {
            if a.pre != b.pre || a.post != b.post {
                out.push("bal_native");
            }
            if a.token_pre != b.token_pre || a.token_post != b.token_post {
                out.push("bal_token");
            }
        }
        (None, None) => {}
        _ => out.push("bal_native"),
    }
}

// ---------------------------------------------------------------------------------------
// Commit mode (milestone 2).
// ---------------------------------------------------------------------------------------

/// Cell states.
pub const FREE: u8 = 0;
pub const FL_DECLINED: u8 = 1;
pub const FL_CLAIMED: u8 = 2;
pub const FL_DONE: u8 = 3;
pub const FL_FAILED: u8 = 4;
pub const AGAVE_CLAIMED: u8 = 5;
pub const AGAVE_DONE: u8 = 6;

/// Board states.
const UNBOUND: u8 = 0;
const BOUND: u8 = 1;
const ABANDONED: u8 = 2;

const CHUNK: usize = 1024;
/// Transactions per slot a board can hold (beyond it agave executes: the fast lane cannot
/// claim).
pub const MAX_BOARD_TXS: usize = 256 * CHUNK;

/// The error a follower task returns to stop replay of a bank that must be replayed without
/// the fast lane (replay clears the bank instead of marking the slot dead).
pub const REPLAY_REQUIRED_ERROR: TransactionError = TransactionError::ClusterMaintenance;

struct Cell {
    state: AtomicU8,
    /// Message hash of the transaction the fast lane committed (written before FL_DONE).
    hash: [AtomicU64; 4],
}

impl Cell {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(FREE),
            hash: [0; 4].map(AtomicU64::new),
        }
    }

    fn cas(&self, from: u8, to: u8) -> bool {
        self.state
            .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn set_hash(&self, hash: &Hash) {
        for (i, word) in hash.as_ref().chunks_exact(8).enumerate() {
            self.hash[i].store(u64::from_le_bytes(word.try_into().unwrap()), Ordering::Relaxed);
        }
    }

    fn hash_equals(&self, hash: &Hash) -> bool {
        hash.as_ref()
            .chunks_exact(8)
            .enumerate()
            .all(|(i, word)| {
                self.hash[i].load(Ordering::Relaxed) == u64::from_le_bytes(word.try_into().unwrap())
            })
    }
}

/// The fast lane's result of a sampled transaction, for agave's handler to compare.
pub struct SampleDeposit {
    pub result: TransactionProcessingResult,
    pub balances: Option<TxBalances>,
    pub recorded: bool,
}

/// Per-bank claim table of the commit protocol (see the module docs).
pub struct Board {
    pub slot: Slot,
    pub bank_id: BankId,
    pub parent_slot: Slot,
    pub parent_bank_id: Option<BankId>,
    pub sample_ppm: u32,
    /// [`SAMPLE_HASH`] or [`SAMPLE_FL`].
    pub sample_mode: u8,
    /// The fast lane runs this slot over this bank's parent: while unbound, agave waits
    /// (bounded) for the binding.
    fl_expected: AtomicBool,
    state: AtomicU8,
    closed: AtomicBool,
    inflight: AtomicU32,
    replay_reason: Mutex<Option<&'static str>>,
    replay_required: AtomicBool,
    chunks: Box<[OnceLock<Box<[Cell]>>]>,
    errors: Mutex<HashMap<usize, TransactionError>>,
    deposits: Mutex<HashMap<usize, SampleDeposit>>,
    pending_balance_checks: Mutex<HashMap<usize, Option<TxBalances>>>,
    fl_committed: AtomicU32,
    verified: AtomicU32,
    agave_executed: AtomicU32,
    /// Transactions in the fast lane's (complete) run of the slot: agave does not wait for
    /// the fast lane on indexes at or beyond it.
    fl_total: AtomicUsize,
    pub created: Instant,
}

impl Board {
    fn new(bank: &Bank, sample_ppm: u32) -> Self {
        let parent_bank_id = bank.parent().map(|p| p.bank_id());
        Self {
            slot: bank.slot(),
            bank_id: bank.bank_id(),
            parent_slot: bank.parent_slot(),
            parent_bank_id,
            sample_ppm,
            sample_mode: SAMPLE_MODE.load(Ordering::Relaxed),
            fl_expected: AtomicBool::new(is_expected(bank.slot(), parent_bank_id)),
            state: AtomicU8::new(UNBOUND),
            closed: AtomicBool::new(false),
            inflight: AtomicU32::new(0),
            replay_reason: Mutex::new(None),
            replay_required: AtomicBool::new(false),
            chunks: (0..MAX_BOARD_TXS / CHUNK).map(|_| OnceLock::new()).collect(),
            errors: Mutex::new(HashMap::new()),
            deposits: Mutex::new(HashMap::new()),
            pending_balance_checks: Mutex::new(HashMap::new()),
            fl_committed: AtomicU32::new(0),
            verified: AtomicU32::new(0),
            agave_executed: AtomicU32::new(0),
            fl_total: AtomicUsize::new(usize::MAX),
            created: Instant::now(),
        }
    }

    /// A board not tied to a real bank (tests).
    pub fn new_for_tests(slot: Slot, bank_id: BankId, sample_ppm: u32) -> Self {
        Self {
            slot,
            bank_id,
            parent_slot: slot.saturating_sub(1),
            parent_bank_id: None,
            sample_ppm,
            sample_mode: SAMPLE_HASH,
            fl_expected: AtomicBool::new(false),
            state: AtomicU8::new(UNBOUND),
            closed: AtomicBool::new(false),
            inflight: AtomicU32::new(0),
            replay_reason: Mutex::new(None),
            replay_required: AtomicBool::new(false),
            chunks: (0..MAX_BOARD_TXS / CHUNK).map(|_| OnceLock::new()).collect(),
            errors: Mutex::new(HashMap::new()),
            deposits: Mutex::new(HashMap::new()),
            pending_balance_checks: Mutex::new(HashMap::new()),
            fl_committed: AtomicU32::new(0),
            verified: AtomicU32::new(0),
            agave_executed: AtomicU32::new(0),
            fl_total: AtomicUsize::new(usize::MAX),
            created: Instant::now(),
        }
    }

    fn cell(&self, index: usize) -> Option<&Cell> {
        let chunk = self.chunks.get(index / CHUNK)?.get_or_init(|| {
            BOARD_BYTES.fetch_add(CHUNK * std::mem::size_of::<Cell>(), Ordering::Relaxed);
            (0..CHUNK).map(|_| Cell::new()).collect()
        });
        chunk.get(index % CHUNK)
    }

    fn peek(&self, index: usize) -> u8 {
        self.chunks
            .get(index / CHUNK)
            .and_then(|chunk| chunk.get())
            .map(|chunk| chunk[index % CHUNK].state.load(Ordering::Acquire))
            .unwrap_or(FREE)
    }

    /// State of cell `index` (see the cell-state constants).
    pub fn state_of(&self, index: usize) -> u8 {
        self.peek(index)
    }

    /// Transaction `index`'s effects are in the bank (committed by either side).
    pub fn is_done(&self, index: usize) -> bool {
        matches!(self.peek(index), FL_DONE | AGAVE_DONE)
    }

    /// The fast lane takes the bank on: agave's handlers wait for it on free cells.
    pub fn bind(&self) -> bool {
        !self.closed.load(Ordering::SeqCst)
            && self
                .state
                .compare_exchange(UNBOUND, BOUND, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }

    pub fn is_bound(&self) -> bool {
        self.state.load(Ordering::Acquire) == BOUND
    }

    /// The fast lane gives up on this bank: agave's handlers claim every free cell at once.
    pub fn abandon(&self) {
        self.state.store(ABANDONED, Ordering::SeqCst);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Decline transaction `index` (agave executes it). False if already claimed.
    pub fn decline(&self, index: usize) -> bool {
        self.cell(index).is_some_and(|cell| cell.cas(FREE, FL_DECLINED))
    }

    /// Deposit the fast lane's result of a sampled transaction (and decline the cell).
    pub fn deposit_sample(&self, index: usize, deposit: SampleDeposit) {
        // The deposit first: a handler that finds the cell declined finds it too.
        if let Ok(mut deposits) = self.deposits.lock() {
            deposits.insert(index, deposit);
        }
        self.decline(index);
    }

    fn has_deposit(&self, index: usize) -> bool {
        self.deposits
            .lock()
            .map(|d| d.contains_key(&index))
            .unwrap_or(false)
    }

    /// Whether transaction `index` (`signature`) is in this bank's verification sample.
    pub fn is_sample(&self, index: usize, signature: &Signature) -> bool {
        match self.sample_mode {
            SAMPLE_HASH => sampled(signature, self.sample_ppm),
            _ => self.has_deposit(index),
        }
    }

    /// Start committing transaction `index`: registers an in-flight commit (so a closing
    /// replay waits for it) and claims the cell. `None` if the board is closed or the cell is
    /// taken.
    pub fn begin_commit(&self, index: usize) -> Option<CommitGuard<'_>> {
        self.inflight.fetch_add(1, Ordering::SeqCst);
        if self.closed.load(Ordering::SeqCst) {
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        match self.cell(index) {
            Some(cell) if cell.cas(FREE, FL_CLAIMED) => Some(CommitGuard {
                board: self,
                index,
                finished: false,
            }),
            _ => {
                if self.cell(index).is_some_and(|c| c.state.load(Ordering::Acquire) != FL_DECLINED) {
                    STATS.fl_lost.fetch_add(1, Ordering::Relaxed);
                }
                self.inflight.fetch_sub(1, Ordering::SeqCst);
                None
            }
        }
    }

    /// Close the board (no fast-lane commit starts after this) and wait for commits in
    /// flight. They never wait on anything replay holds, so this returns promptly.
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let start = Instant::now();
        while self.inflight.load(Ordering::SeqCst) > 0 {
            if start.elapsed() > Duration::from_millis(1) {
                std::thread::sleep(Duration::from_micros(50));
            } else {
                std::hint::spin_loop();
            }
        }
    }

    pub fn require_replay(&self, reason: &'static str) {
        if !self.replay_required.swap(true, Ordering::SeqCst) {
            STATS.replay_required.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut r) = self.replay_reason.lock() {
                *r = Some(reason);
            }
            warn!(
                "fast lane commit: slot {} (bank {}) must be replayed without the fast lane: \
                 {reason}",
                self.slot, self.bank_id
            );
        }
    }

    pub fn replay_required(&self) -> Option<&'static str> {
        self.replay_required
            .load(Ordering::SeqCst)
            .then(|| self.replay_reason.lock().ok().and_then(|r| *r).unwrap_or("?"))
    }

    /// The fast lane's run of the slot is complete with `total` transactions.
    pub fn set_fl_total(&self, total: usize) {
        self.fl_total.store(total, Ordering::Release);
    }

    pub fn fl_committed(&self) -> u32 {
        self.fl_committed.load(Ordering::SeqCst)
    }

    pub fn verified(&self) -> u32 {
        self.verified.load(Ordering::SeqCst)
    }

    pub fn agave_executed(&self) -> u32 {
        self.agave_executed.load(Ordering::SeqCst)
    }

    fn stored_result(&self, index: usize) -> TransactionResult<()> {
        match self.errors.lock().ok().and_then(|e| e.get(&index).cloned()) {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

impl Drop for Board {
    fn drop(&mut self) {
        let chunks = self.chunks.iter().filter(|c| c.get().is_some()).count();
        BOARD_BYTES.fetch_sub(chunks * CHUNK * std::mem::size_of::<Cell>(), Ordering::Relaxed);
    }
}

/// An in-flight fast-lane commit of one transaction (see [`Board::begin_commit`]). Dropped
/// without `done`/`decline` (the commit unwound) it marks the cell failed and the slot for
/// replay.
pub struct CommitGuard<'a> {
    board: &'a Board,
    index: usize,
    finished: bool,
}

impl CommitGuard<'_> {
    /// Nothing was committed: hand the transaction to agave.
    pub fn decline(mut self) {
        if let Some(cell) = self.board.cell(self.index) {
            cell.cas(FL_CLAIMED, FL_DECLINED);
        }
        STATS.fl_declined_at_commit.fetch_add(1, Ordering::Relaxed);
        self.finished = true;
    }

    /// Committed: record the task result and the transaction's identity, publish FL_DONE.
    pub fn done(mut self, result: &TransactionResult<()>, message_hash: &Hash) {
        if let Err(err) = result {
            if let Ok(mut errors) = self.board.errors.lock() {
                errors.insert(self.index, err.clone());
            }
        }
        if let Some(cell) = self.board.cell(self.index) {
            cell.set_hash(message_hash);
            cell.state.store(FL_DONE, Ordering::Release);
        }
        self.board.fl_committed.fetch_add(1, Ordering::SeqCst);
        STATS.fl_committed.fetch_add(1, Ordering::Relaxed);
        self.finished = true;
    }
}

impl Drop for CommitGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            if let Some(cell) = self.board.cell(self.index) {
                cell.state.store(FL_FAILED, Ordering::Release);
            }
            self.board.require_replay("fl_commit_failed");
        }
        self.board.inflight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Process-wide counters of the commit protocol (cumulative; the fast lane reports deltas).
#[derive(Default)]
pub struct CommitStats {
    pub boards_created: AtomicU64,
    pub fl_committed: AtomicU64,
    /// Fast-lane claims lost to agave.
    pub fl_lost: AtomicU64,
    /// Commits declined at commit time (runtime check difference, unprocessable).
    pub fl_declined_at_commit: AtomicU64,
    /// Transactions agave's replay executed on a commit board: bound (fast lane active on the
    /// bank) or unbound.
    pub agave_bound: AtomicU64,
    pub agave_unbound: AtomicU64,
    /// Agave claimed a free cell the fast lane had not claimed within the follow timeout.
    pub follow_timeouts: AtomicU64,
    /// Tasks whose handler waited for the fast lane, and the total wait.
    pub follow_waits: AtomicU64,
    pub follow_wait_us: AtomicU64,
    /// Handler found the fast lane's commit already done.
    pub follow_done: AtomicU64,
    pub identity_mismatches: AtomicU64,
    pub replay_required: AtomicU64,
    pub samples: AtomicU64,
    pub sample_mismatches: AtomicU64,
    pub sample_timeouts: AtomicU64,
    pub poisons: AtomicU64,
    pub unverified_commits: AtomicU64,
    pub agave_only_slots: AtomicU64,
    /// Handlers that waited for the fast lane's binding of a bank it runs, and those whose
    /// wait ran out (executed by agave).
    pub bind_waits: AtomicU64,
    pub bind_wait_timeouts: AtomicU64,
}

pub static STATS: CommitStats = CommitStats {
    boards_created: AtomicU64::new(0),
    fl_committed: AtomicU64::new(0),
    fl_lost: AtomicU64::new(0),
    fl_declined_at_commit: AtomicU64::new(0),
    agave_bound: AtomicU64::new(0),
    agave_unbound: AtomicU64::new(0),
    follow_timeouts: AtomicU64::new(0),
    follow_waits: AtomicU64::new(0),
    follow_wait_us: AtomicU64::new(0),
    follow_done: AtomicU64::new(0),
    identity_mismatches: AtomicU64::new(0),
    replay_required: AtomicU64::new(0),
    samples: AtomicU64::new(0),
    sample_mismatches: AtomicU64::new(0),
    sample_timeouts: AtomicU64::new(0),
    poisons: AtomicU64::new(0),
    unverified_commits: AtomicU64::new(0),
    agave_only_slots: AtomicU64::new(0),
    bind_waits: AtomicU64::new(0),
    bind_wait_timeouts: AtomicU64::new(0),
};

/// Bytes held by board cells (reported by the fast lane as `mem_board_kb`).
pub static BOARD_BYTES: AtomicUsize = AtomicUsize::new(0);

static BOARDS: RwLock<Option<HashMap<BankId, Arc<Board>>>> = RwLock::new(None);
static AGAVE_ONLY: Mutex<VecDeque<Slot>> = Mutex::new(VecDeque::new());

thread_local! {
    static LAST_BOARD: RefCell<Option<Arc<Board>>> = const { RefCell::new(None) };
}

/// The commit board of `bank_id`, if it has one.
pub fn board_of(bank_id: BankId) -> Option<Arc<Board>> {
    if COMMIT_BOARDS.load(Ordering::Acquire) == 0 {
        return None;
    }
    BOARDS.read().ok()?.as_ref()?.get(&bank_id).cloned()
}

/// The board of `bank`, cached per thread (replay handlers run one bank at a time).
fn board_for(bank: &Bank) -> Option<Arc<Board>> {
    if COMMIT_BOARDS.load(Ordering::Acquire) == 0 {
        return None;
    }
    let bank_id = bank.bank_id();
    LAST_BOARD.with(|last| {
        let mut last = last.borrow_mut();
        if let Some(board) = last.as_ref().filter(|b| b.bank_id == bank_id && !b.is_closed()) {
            return Some(Arc::clone(board));
        }
        let board = board_of(bank_id);
        *last = board.clone();
        board
    })
}

fn remove_board(bank_id: BankId) -> Option<Arc<Board>> {
    let mut guard = BOARDS.write().ok()?;
    let board = guard.as_mut()?.remove(&bank_id)?;
    COMMIT_BOARDS.fetch_sub(1, Ordering::AcqRel);
    Some(board)
}

fn is_agave_only(slot: Slot) -> bool {
    AGAVE_ONLY.lock().map(|s| s.contains(&slot)).unwrap_or(true)
}

/// Whether `slot` is replayed without the fast lane (see [`mark_agave_only`]).
pub fn is_agave_only_slot(slot: Slot) -> bool {
    is_agave_only(slot)
}

/// Replay `slot` without the fast lane from now on (a cleared bank's replacement).
pub fn mark_agave_only(slot: Slot) {
    STATS.agave_only_slots.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut slots) = AGAVE_ONLY.lock() {
        if !slots.contains(&slot) {
            slots.push_back(slot);
            while slots.len() > 1024 {
                slots.pop_front();
            }
        }
    }
}

/// `BankForks::insert`: a bank inserted while the commit mode is on gets a board.
pub fn on_bank_inserted(bank: &Arc<Bank>) {
    let mode = MODE.load(Ordering::SeqCst);
    if mode == MODE_OFF {
        return;
    }
    if mode == MODE_ON && !is_agave_only(bank.slot()) && bank.parent().is_some() {
        let board = Arc::new(Board::new(bank, SAMPLE_PPM.load(Ordering::Relaxed)));
        if let Ok(mut guard) = BOARDS.write() {
            let boards = guard.get_or_insert_with(HashMap::new);
            // Bounded: forget boards far behind the newest slot (their banks are gone).
            if boards.len() >= 512 {
                let horizon = bank.slot().saturating_sub(512);
                let before = boards.len();
                boards.retain(|_, b| b.slot >= horizon);
                COMMIT_BOARDS.fetch_sub(before - boards.len(), Ordering::AcqRel);
            }
            if boards.insert(bank.bank_id(), board).is_none() {
                COMMIT_BOARDS.fetch_add(1, Ordering::AcqRel);
            }
            STATS.boards_created.fetch_add(1, Ordering::Relaxed);
        }
    }
    if let Some(hooks) = hooks() {
        hooks.on_bank_inserted(bank);
    }
}

/// `BankForks::remove`: a bank leaves bank forks (pruned, dumped, cleared). No fast-lane
/// commit may reach it afterwards (its slot's accounts may be purged).
pub fn on_bank_removed(bank_id: BankId) {
    if let Some(board) = remove_board(bank_id) {
        board.abandon();
        board.close();
    }
}

/// Replay's handler for task `index` of `bank` (the unified scheduler's `DefaultTaskHandler`).
pub enum Follow {
    /// Agave executes the transaction (and then calls [`agave_done`]).
    Execute,
    /// The fast lane committed it: this is the task's result.
    Done(TransactionResult<()>),
}

/// Transactions the fast lane never commits: they touch a program loader (deploy, upgrade,
/// close, extend, ...), whose program-cache side effects the fast lane does not reproduce.
pub fn static_exclusion(tx: &impl SVMMessage) -> Option<&'static str> {
    tx.account_keys().iter().any(|key| {
        *key == bpf_loader_upgradeable::id()
            || *key == bpf_loader::id()
            || *key == bpf_loader_deprecated::id()
            || *key == loader_v4::id()
    })
    .then_some("loader")
}

/// Whether `signature` is in the verification sample at `ppm` parts per million.
pub fn sampled(signature: &Signature, ppm: u32) -> bool {
    ppm > 0 && {
        let bytes: [u8; 8] = signature.as_ref()[..8].try_into().unwrap();
        u64::from_le_bytes(bytes) % 1_000_000 < u64::from(ppm)
    }
}

fn wait_step(start: Instant) {
    if start.elapsed() < Duration::from_micros(FOLLOW_SPIN_US.load(Ordering::Relaxed)) {
        std::hint::spin_loop();
    } else {
        std::thread::sleep(Duration::from_micros(20));
    }
}

/// See [`Follow`]. Called by replay's task handler before executing task `index`.
pub fn follow(bank: &Bank, index: usize, tx: &impl TransactionWithMeta) -> Follow {
    let Some(board) = board_for(bank) else {
        return Follow::Execute;
    };
    let Some(cell) = board.cell(index) else {
        return Follow::Execute;
    };
    let start = Instant::now();
    let mut waited = false;
    let follow_wait = Duration::from_micros(FOLLOW_WAIT_US.load(Ordering::Relaxed));
    // Transactions the fast lane never commits (agave executes them without waiting).
    let agave_only_tx = static_exclusion(tx).is_some()
        || (board.sample_mode == SAMPLE_HASH && sampled(tx.signature(), board.sample_ppm));
    let bind_wait = Duration::from_micros(BIND_WAIT_US.load(Ordering::Relaxed));
    let mut bind_waited = false;
    let finish_wait = |waited: bool| {
        if waited {
            STATS.follow_waits.fetch_add(1, Ordering::Relaxed);
            STATS
                .follow_wait_us
                .fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
    };
    loop {
        match cell.state.load(Ordering::Acquire) {
            FL_DONE => {
                finish_wait(waited);
                if !cell.hash_equals(tx.message_hash()) {
                    STATS.identity_mismatches.fetch_add(1, Ordering::Relaxed);
                    error!(
                        "fast lane commit: slot {} index {index}: committed transaction differs \
                         from replay's {}",
                        board.slot,
                        tx.signature()
                    );
                    board.require_replay("identity");
                    poison("fast lane committed a different transaction than replay's");
                    return Follow::Done(Err(REPLAY_REQUIRED_ERROR));
                }
                board.verified.fetch_add(1, Ordering::SeqCst);
                STATS.follow_done.fetch_add(1, Ordering::Relaxed);
                return Follow::Done(board.stored_result(index));
            }
            FL_FAILED => {
                finish_wait(waited);
                board.require_replay("fl_commit_failed");
                return Follow::Done(Err(REPLAY_REQUIRED_ERROR));
            }
            FL_CLAIMED => {
                // A commit in progress completes (or fails) promptly; never claim over it.
                if start.elapsed() > Duration::from_secs(10) {
                    board.require_replay("fl_commit_stuck");
                    poison("a fast-lane commit did not complete in 10 s");
                    return Follow::Done(Err(REPLAY_REQUIRED_ERROR));
                }
                waited = true;
                wait_step(start);
            }
            FL_DECLINED => {
                if cell.cas(FL_DECLINED, AGAVE_CLAIMED) {
                    finish_wait(waited);
                    board.agave_executed.fetch_add(1, Ordering::SeqCst);
                    STATS.agave_bound.fetch_add(1, Ordering::Relaxed);
                    return Follow::Execute;
                }
            }
            FREE => {
                // The fast lane runs this slot and its binding is on its way: wait for it
                // (bounded) rather than execute the transaction here.
                if !agave_only_tx
                    && !board.is_bound()
                    && fl_live()
                    && board.fl_expected.load(Ordering::Acquire)
                    && board.state.load(Ordering::Acquire) == UNBOUND
                    && start.elapsed() < bind_wait
                {
                    if !bind_waited {
                        bind_waited = true;
                        STATS.bind_waits.fetch_add(1, Ordering::Relaxed);
                    }
                    waited = true;
                    wait_step(start);
                    continue;
                }
                let fl_will_commit = !agave_only_tx
                    && fl_live()
                    && board.is_bound()
                    && !board.is_closed()
                    && index < board.fl_total.load(Ordering::Acquire);
                let timed_out = fl_will_commit && start.elapsed() >= follow_wait;
                if !fl_will_commit || timed_out {
                    if cell.cas(FREE, AGAVE_CLAIMED) {
                        finish_wait(waited);
                        board.agave_executed.fetch_add(1, Ordering::SeqCst);
                        if timed_out {
                            STATS.follow_timeouts.fetch_add(1, Ordering::Relaxed);
                        }
                        if board.is_bound() {
                            STATS.agave_bound.fetch_add(1, Ordering::Relaxed);
                        } else {
                            STATS.agave_unbound.fetch_add(1, Ordering::Relaxed);
                            if bind_waited {
                                STATS.bind_wait_timeouts.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        return Follow::Execute;
                    }
                    continue;
                }
                waited = true;
                wait_step(start);
            }
            // Agave's own cell (a task never runs twice): execute as usual.
            _ => return Follow::Execute,
        }
    }
}

/// Replay's handler executed and committed task `index` (after [`Follow::Execute`]).
pub fn agave_done(bank: &Bank, index: usize) {
    let Some(board) = board_for(bank) else {
        return;
    };
    let Some(cell) = board.cell(index) else {
        return;
    };
    if cell.cas(AGAVE_CLAIMED, AGAVE_DONE) {
        if let Some(hooks) = hooks() {
            hooks.on_agave_done(board.slot, board.bank_id, index);
        }
    }
}

/// Whether `tx` is in `bank`'s verification sample (agave executes it and compares the
/// fast lane's result before committing its own).
pub fn is_sample(bank: &Bank, index: usize, tx: &impl TransactionWithMeta) -> bool {
    board_for(bank).is_some_and(|board| board.is_sample(index, tx.signature()))
}

/// Replay executed sampled transaction `index` (`result`, not committed yet): compare with
/// the fast lane's result. A difference poisons the fast lane and, if it committed anything
/// into this bank, requires the slot to be replayed without it.
pub(crate) fn verify_sample(bank: &Bank, index: usize, result: &TransactionProcessingResult) {
    let Some(board) = board_for(bank) else {
        return;
    };
    STATS.samples.fetch_add(1, Ordering::Relaxed);
    let wait = Duration::from_micros(SAMPLE_WAIT_US.load(Ordering::Relaxed));
    let start = Instant::now();
    let deposit = loop {
        if let Some(deposit) = board.deposits.lock().ok().and_then(|mut d| d.remove(&index)) {
            break Some(deposit);
        }
        if !fl_live() || !board.is_bound() || start.elapsed() >= wait {
            break None;
        }
        wait_step(start);
    };
    let Some(deposit) = deposit else {
        STATS.sample_timeouts.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let mut fields = Vec::new();
    diff_results(&deposit.result, None, deposit.recorded, result, None, &mut fields);
    if fields.is_empty() {
        if deposit.recorded {
            if let Ok(mut checks) = board.pending_balance_checks.lock() {
                checks.insert(index, deposit.balances);
            }
        }
        return;
    }
    sample_mismatch(&board, index, &fields);
}

/// Balances of a verified sample (with a `TransactionStatusSender`).
pub(crate) fn verify_sample_balances(bank: &Bank, index: usize, balances: &TxBalances) {
    let Some(board) = board_for(bank) else {
        return;
    };
    let Some(expected) = board
        .pending_balance_checks
        .lock()
        .ok()
        .and_then(|mut checks| checks.remove(&index))
    else {
        return;
    };
    let mut fields = Vec::new();
    diff_balances(expected.as_ref(), Some(balances), &mut fields);
    if !fields.is_empty() {
        sample_mismatch(&board, index, &fields);
    }
}

fn sample_mismatch(board: &Board, index: usize, fields: &[&'static str]) {
    STATS.sample_mismatches.fetch_add(1, Ordering::Relaxed);
    error!(
        "fast lane commit: slot {} index {index}: sampled transaction differs in {fields:?}",
        board.slot
    );
    if board.fl_committed() > 0
        && fields
            .iter()
            .any(|f| !matches!(*f, "logs" | "inner_ix" | "return_data" | "bal_native" | "bal_token"))
    {
        board.require_replay("sample_mismatch");
    }
    poison("a sampled transaction's fast-lane result differs from agave's");
}

/// Replay finished executing `bank` (every task done, verification passed) and is about to
/// freeze it: close its board and decide. `Ok(n)`: keep the bank (`n` transactions were
/// committed by the fast lane). `Err(reason)`: the fast lane committed something that must not
/// be kept; clear the bank and replay the slot without the fast lane.
pub fn close_bank(bank: &Bank) -> Result<u32, &'static str> {
    let Some(board) = remove_board(bank.bank_id()) else {
        return Ok(0);
    };
    board.abandon();
    board.close();
    if let Some(reason) = board.replay_required() {
        return Err(reason);
    }
    let (committed, verified) = (board.fl_committed(), board.verified());
    if committed != verified {
        STATS.unverified_commits.fetch_add(1, Ordering::Relaxed);
        error!(
            "fast lane commit: slot {}: {committed} fast-lane commits but replay verified \
             {verified}",
            board.slot
        );
        board.require_replay("unverified_commit");
        poison("the fast lane committed transactions replay does not have");
        return Err("unverified_commit");
    }
    Ok(committed)
}

/// Replay of `bank` failed (it would be marked dead): close its board. Returns whether the
/// fast lane committed into it, in which case the caller replays the slot without the fast
/// lane instead (the failure may come from a bad fast-lane commit; a genuinely invalid
/// block fails again).
pub fn abandon_bank(bank: &Bank) -> bool {
    let Some(board) = remove_board(bank.bank_id()) else {
        return false;
    };
    board.abandon();
    board.close();
    board.fl_committed() > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claims_are_exclusive() {
        // Many threads race to claim every cell, as the fast lane or as agave: each cell ends
        // with exactly one owner.
        let board = Arc::new(Board::new_for_tests(5, 1, 0));
        let n = 20_000;
        let fl_wins = Arc::new(AtomicU64::new(0));
        let agave_wins = Arc::new(AtomicU64::new(0));
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let board = board.clone();
                let fl_wins = fl_wins.clone();
                let agave_wins = agave_wins.clone();
                std::thread::spawn(move || {
                    for i in 0..n {
                        let index = if t % 2 == 0 { i } else { n - 1 - i };
                        if t < 4 {
                            if let Some(guard) = board.begin_commit(index) {
                                guard.done(&Ok(()), &Hash::default());
                                fl_wins.fetch_add(1, Ordering::Relaxed);
                            }
                        } else if board.cell(index).unwrap().cas(FREE, AGAVE_CLAIMED) {
                            agave_wins.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let fl = fl_wins.load(Ordering::Relaxed);
        let agave = agave_wins.load(Ordering::Relaxed);
        assert_eq!(fl + agave, n as u64);
        assert_eq!(u64::from(board.fl_committed()), fl);
        for i in 0..n {
            assert!(matches!(board.state_of(i), FL_DONE | AGAVE_CLAIMED));
        }
        assert_eq!(board.inflight.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_close_waits_and_refuses() {
        let board = Board::new_for_tests(5, 1, 0);
        let guard = board.begin_commit(0).unwrap();
        std::thread::scope(|s| {
            let closer = s.spawn(|| board.close());
            std::thread::sleep(Duration::from_millis(20));
            assert!(!closer.is_finished(), "close waits for the in-flight commit");
            guard.done(&Ok(()), &Hash::default());
            closer.join().unwrap();
        });
        assert!(board.begin_commit(1).is_none(), "no commit after close");
        assert_eq!(board.state_of(1), FREE);
    }

    #[test]
    fn test_unwinding_commit_fails_cell() {
        let board = Board::new_for_tests(5, 1, 0);
        {
            let _guard = board.begin_commit(3).unwrap();
            // dropped without done/decline: as if the commit panicked
        }
        assert_eq!(board.state_of(3), FL_FAILED);
        assert_eq!(board.replay_required(), Some("fl_commit_failed"));
        let guard = board.begin_commit(4).unwrap();
        guard.decline();
        assert_eq!(board.state_of(4), FL_DECLINED);
        assert!(board.decline(5));
        assert!(!board.decline(5));
        assert!(board.begin_commit(5).is_none());
    }

    #[test]
    fn test_sampling_is_deterministic() {
        let sig = Signature::from([7u8; 64]);
        assert!(!sampled(&sig, 0));
        assert!(sampled(&sig, 1_000_000));
        let hits = (0..10_000u32)
            .filter(|i| {
                let mut bytes = [0u8; 64];
                bytes[..4].copy_from_slice(&i.wrapping_mul(2_654_435_761).to_le_bytes());
                bytes[4..8].copy_from_slice(&i.to_le_bytes());
                sampled(&Signature::from(bytes), 100_000)
            })
            .count();
        assert!((500..1500).contains(&hits), "{hits}");
    }
}
