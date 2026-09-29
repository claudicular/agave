//! Milestone-1 shadow check ("commit = shadow"): FL's full processing result of each
//! transaction against agave's own, captured by `solana_runtime::fast_lane_commit` just
//! before agave commits it, field by field.
//!
//! The compared fields are everything agave's commit consumes (`Bank::commit_transactions`
//! and the rest of `execute_batch`): the variant (executed / fees-only / no-op / error),
//! status, loaded (post-execution) accounts, touched flags, fee details, rollback accounts,
//! loaded data size, the remaining loaded-transaction state (compute budget), logs, inner
//! instructions, return data, executed units, account-size deltas, programs modified, and
//! the native and token balances sent to the `TransactionStatusSender`. If every field is
//! equal for every transaction, committing FL's result yields exactly agave's state.

use {
    crate::{
        mem,
        mv::accounts_equal,
        run::FlProcessed,
    },
    solana_account::ReadableAccount,
    solana_clock::Slot,
    solana_runtime::fast_lane_commit::{AgaveProcessed, TxBalances, tx_balances},
    solana_signature::Signature,
    solana_svm::transaction_processing_result::{ProcessedTransaction, TransactionProcessingResult},
    std::{
        collections::HashMap,
        fmt::Write as _,
        time::{Duration, Instant},
    },
};

/// Every field the comparison reports, in reporting order.
pub const FIELDS: [&str; 18] = solana_runtime::fast_lane_commit::DIFF_FIELDS;

/// FL's side of one comparison: the processing result and its compiled balances.
pub struct FlFull {
    pub slot: Slot,
    pub parent_slot: Slot,
    pub ordinal: u32,
    pub signature: Signature,
    pub result: TransactionProcessingResult,
    pub balances: Option<TxBalances>,
    pub recorded: bool,
    pub t: Instant,
}

impl FlFull {
    pub fn new(
        slot: Slot,
        parent_slot: Slot,
        ordinal: u32,
        signature: Signature,
        processed: FlProcessed,
    ) -> Self {
        let FlProcessed {
            result,
            balances,
            recorded,
            ..
        } = processed;
        Self {
            slot,
            parent_slot,
            ordinal,
            signature,
            result,
            balances: balances.and_then(|b| tx_balances(b).into_iter().next()),
            recorded,
            t: Instant::now(),
        }
    }

    pub fn bytes(&self) -> i64 {
        processing_result_bytes(&self.result) + balances_bytes(self.balances.as_ref()) + 128
    }
}

/// Approximate heap bytes of a processing result (accounts, logs, inner instructions).
pub fn processing_result_bytes(result: &TransactionProcessingResult) -> i64 {
    match result {
        Ok(ProcessedTransaction::Executed(executed)) => {
            let loaded = &executed.loaded_transaction;
            let details = &executed.execution_details;
            let accounts: i64 = loaded
                .accounts
                .iter()
                .map(|(_, a)| mem::account_bytes(a))
                .sum();
            let rollback: i64 = loaded
                .rollback_accounts
                .iter()
                .map(|(_, a)| mem::account_bytes(a))
                .sum();
            let logs: i64 = details
                .log_messages
                .as_ref()
                .map(|logs| logs.iter().map(|l| l.len() as i64 + 24).sum())
                .unwrap_or(0);
            let inner: i64 = details
                .inner_instructions
                .as_ref()
                .map(|list| {
                    list.iter()
                        .flatten()
                        .map(|ix| 48 + ix.instruction.data.len() as i64 + ix.instruction.accounts.len() as i64)
                        .sum()
                })
                .unwrap_or(0);
            let ret = details
                .return_data
                .as_ref()
                .map(|r| r.data.len() as i64 + 32)
                .unwrap_or(0);
            accounts + rollback + logs + inner + ret + 256
        }
        Ok(ProcessedTransaction::FeesOnly(fees_only)) => {
            fees_only
                .rollback_accounts
                .iter()
                .map(|(_, a)| mem::account_bytes(a))
                .sum::<i64>()
                + 128
        }
        Ok(ProcessedTransaction::NoOp(_)) | Err(_) => 128,
    }
}

fn balances_bytes(balances: Option<&TxBalances>) -> i64 {
    balances
        .map(|b| {
            8 * (b.pre.len() + b.post.len()) as i64
                + 160 * (b.token_pre.len() + b.token_post.len()) as i64
        })
        .unwrap_or(0)
}

pub fn agave_bytes(processed: &AgaveProcessed) -> i64 {
    processing_result_bytes(&processed.result) + balances_bytes(processed.balances.as_ref()) + 192
}

/// Compare FL's result with agave's (`solana_runtime::fast_lane_commit::diff_results`, the
/// same comparison agave's handlers run on commit-mode samples); pushes the name of every
/// differing field. Recording fields are compared only when `recorded`.
pub fn diff(
    fl: &TransactionProcessingResult,
    fl_balances: Option<&TxBalances>,
    recorded: bool,
    agave: &TransactionProcessingResult,
    agave_balances: Option<&TxBalances>,
    out: &mut Vec<&'static str>,
) {
    solana_runtime::fast_lane_commit::diff_results(
        fl,
        fl_balances,
        recorded,
        agave,
        agave_balances,
        out,
    )
}

/// Per-interval counters of the full comparison.
#[derive(Default)]
pub struct FullInterval {
    pub compared: u64,
    pub matched: u64,
    pub mismatched: u64,
    pub fields: HashMap<&'static str, u64>,
    pub unjoined_fl: u64,
    pub unjoined_agave: u64,
    /// FL executed without agave's recording configuration (recording fields skipped).
    pub unrecorded: u64,
}

impl FullInterval {
    pub fn fields_sorted(&self) -> Vec<(&'static str, u64)> {
        FIELDS
            .iter()
            .filter_map(|f| self.fields.get(f).map(|n| (*f, *n)))
            .collect()
    }
}

/// Join state: FL results and agave captures waiting for the other side.
#[derive(Default)]
pub struct FullCompare {
    fl: HashMap<(Slot, Signature), FlFull>,
    agave: HashMap<(Slot, Signature), Box<AgaveProcessed>>,
    pub interval: FullInterval,
    pub total_compared: u64,
    pub total_mismatched: u64,
}

/// A mismatch to sample (written by the comparator, rate-limited).
pub struct FullMismatch {
    pub json: String,
}

impl FullCompare {
    pub fn fl_len(&self) -> usize {
        self.fl.len()
    }

    pub fn agave_len(&self) -> usize {
        self.agave.len()
    }

    pub fn on_fl(&mut self, fl: FlFull) -> Option<FullMismatch> {
        let key = (fl.slot, fl.signature);
        if let Some(agave) = self.agave.remove(&key) {
            mem::FULL_BYTES.sub(agave_bytes(&agave));
            self.join(fl, &agave)
        } else {
            mem::FULL_BYTES.add(fl.bytes());
            if let Some(old) = self.fl.insert(key, fl) {
                mem::FULL_BYTES.sub(old.bytes());
            }
            None
        }
    }

    pub fn on_agave(&mut self, agave: Box<AgaveProcessed>) -> Option<FullMismatch> {
        let key = (agave.slot, agave.signature);
        if let Some(fl) = self.fl.remove(&key) {
            mem::FULL_BYTES.sub(fl.bytes());
            self.join(fl, &agave)
        } else {
            mem::FULL_BYTES.add(agave_bytes(&agave));
            if let Some(old) = self.agave.insert(key, agave) {
                mem::FULL_BYTES.sub(agave_bytes(&old));
            }
            None
        }
    }

    fn join(&mut self, fl: FlFull, agave: &AgaveProcessed) -> Option<FullMismatch> {
        let mut fields = Vec::new();
        let recorded = fl.recorded || agave.balances.is_none();
        if !fl.recorded && agave.balances.is_some() {
            self.interval.unrecorded += 1;
        }
        diff(
            &fl.result,
            fl.balances.as_ref(),
            fl.recorded && recorded,
            &agave.result,
            agave.balances.as_ref(),
            &mut fields,
        );
        if fl.parent_slot != agave.parent_slot {
            fields.push("parent");
        }
        self.interval.compared += 1;
        self.total_compared += 1;
        if fields.is_empty() {
            self.interval.matched += 1;
            return None;
        }
        self.interval.mismatched += 1;
        self.total_mismatched += 1;
        for field in &fields {
            *self.interval.fields.entry(field).or_default() += 1;
        }
        Some(FullMismatch {
            json: mismatch_json(&fl, agave, &fields),
        })
    }

    /// Entries older than `horizon` without a partner are counted and dropped.
    pub fn gc(&mut self, horizon: Duration) {
        let now = Instant::now();
        let stale_fl: Vec<_> = self
            .fl
            .iter()
            .filter(|(_, f)| now.saturating_duration_since(f.t) > horizon)
            .map(|(k, _)| *k)
            .collect();
        for key in stale_fl {
            if let Some(fl) = self.fl.remove(&key) {
                mem::FULL_BYTES.sub(fl.bytes());
                self.interval.unjoined_fl += 1;
            }
        }
        let stale_agave: Vec<_> = self
            .agave
            .iter()
            .filter(|(_, a)| now.saturating_duration_since(a.t) > horizon)
            .map(|(k, _)| *k)
            .collect();
        for key in stale_agave {
            if let Some(agave) = self.agave.remove(&key) {
                mem::FULL_BYTES.sub(agave_bytes(&agave));
                self.interval.unjoined_agave += 1;
            }
        }
    }

    /// Drop everything held (the fast lane is off or the mode changed).
    pub fn release(&mut self) {
        let held: i64 = self.fl.values().map(FlFull::bytes).sum::<i64>()
            + self.agave.values().map(|a| agave_bytes(a)).sum::<i64>();
        mem::FULL_BYTES.sub(held);
        self.fl = HashMap::new();
        self.agave = HashMap::new();
    }

    pub fn take_interval(&mut self) -> FullInterval {
        std::mem::take(&mut self.interval)
    }
}

fn result_summary(out: &mut String, result: &TransactionProcessingResult) {
    match result {
        Ok(ProcessedTransaction::Executed(e)) => {
            let d = &e.execution_details;
            let l = &e.loaded_transaction;
            let _ = write!(
                out,
                "{{\"kind\":\"executed\",\"status\":\"{:?}\",\"cu\":{},\"loaded_size\":{},\
                 \"fee\":\"{:?}\",\"accounts\":{},\"logs\":{},\"inner\":{},\"ret\":{},\
                 \"deltas\":\"{:?}\",\"programs\":{}}}",
                d.status,
                d.executed_units,
                l.loaded_accounts_data_size,
                l.fee_details,
                l.accounts.len(),
                d.log_messages.as_ref().map(|v| v.len()).unwrap_or(0),
                d.inner_instructions
                    .as_ref()
                    .map(|v| v.iter().map(|i| i.len()).sum::<usize>())
                    .unwrap_or(0),
                d.return_data.as_ref().map(|r| r.data.len()).unwrap_or(0),
                d.accounts_deltas,
                e.programs_modified_by_tx.len(),
            );
        }
        Ok(ProcessedTransaction::FeesOnly(f)) => {
            let _ = write!(
                out,
                "{{\"kind\":\"fees_only\",\"status\":\"{:?}\",\"fee\":\"{:?}\",\"loaded_size\":{}}}",
                f.load_error, f.fee_details, f.loaded_accounts_data_size
            );
        }
        Ok(ProcessedTransaction::NoOp(n)) => {
            let _ = write!(out, "{{\"kind\":\"noop\",\"status\":\"{:?}\"}}", n.validation_error);
        }
        Err(err) => {
            let _ = write!(out, "{{\"kind\":\"error\",\"status\":\"{err:?}\"}}");
        }
    }
}

fn first_log_diff(a: &TransactionProcessingResult, b: &TransactionProcessingResult) -> String {
    let logs = |r: &TransactionProcessingResult| match r {
        Ok(ProcessedTransaction::Executed(e)) => e.execution_details.log_messages.clone(),
        _ => None,
    };
    let (Some(a), Some(b)) = (logs(a), logs(b)) else {
        return String::new();
    };
    let i = a.iter().zip(&b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
    let esc = |s: Option<&String>| {
        s.map(|s| s.chars().take(200).collect::<String>())
            .unwrap_or_default()
            .replace('\\', "\\\\")
            .replace('"', "'")
    };
    format!(
        ",\"log_diff\":{{\"at\":{i},\"fl\":\"{}\",\"agave\":\"{}\"}}",
        esc(a.get(i)),
        esc(b.get(i))
    )
}

fn first_account_diff(a: &TransactionProcessingResult, b: &TransactionProcessingResult) -> String {
    let accounts = |r: &TransactionProcessingResult| match r {
        Ok(ProcessedTransaction::Executed(e)) => Some(e.loaded_transaction.accounts.clone()),
        _ => None,
    };
    let (Some(a), Some(b)) = (accounts(a), accounts(b)) else {
        return String::new();
    };
    for (i, ((ka, aa), (kb, ab))) in a.iter().zip(&b).enumerate() {
        if ka != kb || !accounts_equal(aa, ab) {
            return format!(
                ",\"account_diff\":{{\"at\":{i},\"fl\":\"{ka} {} {} {}\",\"agave\":\"{kb} {} {} {}\"}}",
                aa.lamports(),
                aa.owner(),
                aa.data().len(),
                ab.lamports(),
                ab.owner(),
                ab.data().len()
            );
        }
    }
    String::new()
}

fn mismatch_json(fl: &FlFull, agave: &AgaveProcessed, fields: &[&'static str]) -> String {
    let mut s = String::with_capacity(1024);
    let _ = write!(
        s,
        "{{\"full\":true,\"slot\":{},\"parent\":{},\"agave_parent\":{},\"ordinal\":{},\
         \"agave_index\":{},\"signature\":\"{}\",\"fields\":{:?},\"fl\":",
        fl.slot, fl.parent_slot, agave.parent_slot, fl.ordinal, agave.index, fl.signature, fields,
    );
    result_summary(&mut s, &fl.result);
    s.push_str(",\"agave\":");
    result_summary(&mut s, &agave.result);
    s.push_str(&first_log_diff(&fl.result, &agave.result));
    s.push_str(&first_account_diff(&fl.result, &agave.result));
    if fields.contains(&"bal_native") || fields.contains(&"bal_token") {
        let _ = write!(
            s,
            ",\"balances\":{{\"fl\":\"{:?}\",\"agave\":\"{:?}\"}}",
            fl.balances.as_ref().map(|b| (&b.pre, &b.post)),
            agave.balances.as_ref().map(|b| (&b.pre, &b.post))
        );
    }
    s.push('}');
    s
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_account::AccountSharedData,
        solana_fee_structure::FeeDetails,
        solana_pubkey::Pubkey,
        solana_svm::{
            account_loader::{FeesOnlyTransaction, LoadedTransaction},
            rollback_accounts::RollbackAccounts,
            transaction_execution_result::{ExecutedTransaction, TransactionExecutionDetails},
        },
        solana_transaction_error::TransactionError,
    };

    fn executed(lamports: u64, logs: Vec<String>, cu: u64) -> TransactionProcessingResult {
        let payer = Pubkey::new_from_array([1; 32]);
        let account = AccountSharedData::new(lamports, 0, &Pubkey::default());
        Ok(ProcessedTransaction::Executed(Box::new(ExecutedTransaction {
            loaded_transaction: LoadedTransaction {
                accounts: vec![(payer, account)],
                touched_flags: vec![true].into_boxed_slice(),
                fee_details: FeeDetails::new(5000, 0),
                rollback_accounts: RollbackAccounts::FeePayerOnly {
                    fee_payer: (payer, AccountSharedData::new(10, 0, &Pubkey::default())),
                },
                loaded_accounts_data_size: 100,
                ..LoadedTransaction::default()
            },
            execution_details: TransactionExecutionDetails {
                status: Ok(()),
                log_messages: Some(logs),
                inner_instructions: Some(vec![]),
                return_data: None,
                executed_units: cu,
                accounts_deltas: None,
            },
            programs_modified_by_tx: HashMap::new(),
        })))
    }

    fn fields(a: &TransactionProcessingResult, b: &TransactionProcessingResult) -> Vec<&'static str> {
        let mut out = Vec::new();
        diff(a, None, true, b, None, &mut out);
        out
    }

    #[test]
    fn test_diff_fields() {
        let base = executed(10, vec!["a".into()], 300);
        assert!(fields(&base, &executed(10, vec!["a".into()], 300)).is_empty());
        assert_eq!(fields(&base, &executed(11, vec!["a".into()], 300)), vec!["accounts"]);
        assert_eq!(fields(&base, &executed(10, vec!["b".into()], 300)), vec!["logs"]);
        assert_eq!(fields(&base, &executed(10, vec!["a".into()], 301)), vec!["cu"]);
        // Recording fields are skipped when FL did not record.
        let mut out = Vec::new();
        diff(&base, None, false, &executed(10, vec!["b".into()], 300), None, &mut out);
        assert!(out.is_empty());
        let err: TransactionProcessingResult = Err(TransactionError::AccountNotFound);
        assert_eq!(fields(&base, &err), vec!["kind"]);
        assert_eq!(fields(&err, &Err(TransactionError::AccountInUse)), vec!["status"]);
        let fees_only = |fee: u64| -> TransactionProcessingResult {
            Ok(ProcessedTransaction::FeesOnly(Box::new(FeesOnlyTransaction {
                load_error: TransactionError::ProgramAccountNotFound,
                rollback_accounts: RollbackAccounts::default(),
                fee_details: FeeDetails::new(fee, 0),
                loaded_accounts_data_size: 0,
            })))
        };
        assert!(fields(&fees_only(5000), &fees_only(5000)).is_empty());
        assert_eq!(fields(&fees_only(5000), &fees_only(10000)), vec!["fee"]);
        // Balances.
        let a = TxBalances {
            pre: vec![1],
            post: vec![2],
            ..TxBalances::default()
        };
        let b = TxBalances {
            pre: vec![1],
            post: vec![3],
            ..TxBalances::default()
        };
        let mut out = Vec::new();
        diff(&base, Some(&a), true, &base, Some(&b), &mut out);
        assert_eq!(out, vec!["bal_native"]);
    }

    #[test]
    fn test_join_and_gc() {
        let mut full = FullCompare::default();
        let sig = Signature::from([3; 64]);
        let fl = |r| FlFull {
            slot: 5,
            parent_slot: 4,
            ordinal: 0,
            signature: sig,
            result: r,
            balances: None,
            recorded: true,
            t: Instant::now(),
        };
        let agave = |r| {
            Box::new(AgaveProcessed {
                slot: 5,
                bank_id: 9,
                parent_slot: 4,
                index: 0,
                signature: sig,
                message_hash: solana_hash::Hash::default(),
                result: r,
                balances: None,
                cost: None,
                t: Instant::now(),
            })
        };
        assert!(full.on_fl(fl(executed(1, vec![], 1))).is_none());
        assert!(full.on_agave(agave(executed(1, vec![], 1))).is_none());
        assert_eq!((full.interval.compared, full.interval.matched), (1, 1));
        assert!(full.on_agave(agave(executed(1, vec![], 1))).is_none());
        let m = full.on_fl(fl(executed(2, vec![], 1))).unwrap();
        assert!(m.json.contains("accounts"), "{}", m.json);
        assert_eq!(full.interval.fields.get("accounts"), Some(&1));
        full.on_fl(fl(executed(2, vec![], 1)));
        full.gc(Duration::ZERO);
        assert_eq!(full.interval.unjoined_fl, 1);
        assert_eq!(full.fl_len() + full.agave_len(), 0);
    }
}
