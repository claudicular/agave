//! Integration points between agave's replay and the fast lane (`agave-fast-lane`, which
//! depends on this crate and installs [`FastLaneHooks`] at startup). Everything here is inert
//! unless the fast lane installed its hooks and set a mode: the replay hot path then pays
//! one relaxed atomic load per transaction.
//!
//! Modes (process-wide, switchable at runtime):
//! - [`MODE_OFF`]: nothing.
//! - [`MODE_SHADOW`] (milestone 1): after agave executes and commits a replayed transaction,
//!   a copy of everything its commit consumed (the full `TransactionProcessingResult`: post
//!   accounts, fee details, rollback accounts, status, logs, inner instructions, return data,
//!   executed units, account-size deltas, loaded data size, modified programs) plus its
//!   balances and cost is handed to the fast lane, whose comparator checks it field by field
//!   against the fast lane's own result for the same transaction. Nothing else changes.

use {
    crate::{bank::Bank, transaction_balances::compile_collected_balances},
    solana_clock::{BankId, Slot},
    solana_hash::Hash,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_signature::Signature,
    solana_svm::{
        transaction_balances::BalanceCollector,
        transaction_processing_result::{ProcessedTransaction, TransactionProcessingResult},
    },
    solana_transaction_status::TransactionTokenBalance,
    std::{
        sync::{
            Arc, RwLock,
            atomic::{AtomicU8, Ordering},
        },
        time::Instant,
    },
};

pub const MODE_OFF: u8 = 0;
pub const MODE_SHADOW: u8 = 1;

static MODE: AtomicU8 = AtomicU8::new(MODE_OFF);
static HOOKS: RwLock<Option<Arc<dyn FastLaneHooks>>> = RwLock::new(None);

/// Implemented by the fast lane. Called on agave's replay threads: implementations must be
/// wait-free and must never panic (agave's panic hook exits the process).
pub trait FastLaneHooks: Send + Sync {
    /// Agave executed and committed transaction `index` of bank `slot` (shadow mode).
    fn on_agave_processed(&self, processed: AgaveProcessed);
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
    if let Ok(mut slot) = HOOKS.write() {
        *slot = None;
    }
}

pub fn set_mode(mode: u8) {
    MODE.store(mode.min(MODE_SHADOW), Ordering::SeqCst);
}

#[inline]
pub fn mode() -> u8 {
    MODE.load(Ordering::Relaxed)
}

fn hooks() -> Option<Arc<dyn FastLaneHooks>> {
    HOOKS.read().ok()?.clone()
}

/// Whether replay should capture its processing results for the fast lane (shadow mode).
#[inline]
pub fn capture_enabled() -> bool {
    MODE.load(Ordering::Relaxed) == MODE_SHADOW
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
