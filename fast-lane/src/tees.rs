//! Tees on agave's geyser notifiers. Each forwards every call to the inner notifier
//! **first**, unchanged, then (only while the fast lane is active) makes a cheap copy for FL
//! with a non-blocking `try_send`. They run on agave threads (replay handlers, the replay
//! stage, the RPC completed-slots service) and never block or panic on FL's behalf.

use {
    crate::{
        control::{self, TapStats},
        tap::unix_ns,
        Shared,
    },
    solana_account::AccountSharedData,
    solana_accounts_db::accounts_update_notifier_interface::{
        AccountForGeyser, AccountsUpdateNotifier, AccountsUpdateNotifierInterface,
    },
    solana_clock::{BankId, Slot, UnixTimestamp},
    solana_geyser_plugin_manager::block_metadata_notifier_interface::{
        BlockMetadataNotifier, BlockMetadataNotifierArc,
    },
    solana_pubkey::Pubkey,
    solana_rpc::slot_status_notifier::{SlotStatusNotifier, SlotStatusNotifierInterface},
    solana_runtime::bank::KeyedRewardsAndNumPartitions,
    solana_signature::Signature,
    solana_transaction::sanitized::SanitizedTransaction,
    std::{
        fmt,
        sync::{Arc, RwLock},
        time::Instant,
    },
};

/// Agave's grouped per-transaction notification, as observed by the tee.
pub struct AgaveFrame {
    pub slot: Slot,
    pub bank_id: BankId,
    pub signature: Signature,
    pub accounts: Vec<(Pubkey, AccountSharedData)>,
    pub t: Instant,
    pub t_unix_ns: u64,
}

/// Slot lifecycle events from agave.
#[derive(Debug, Clone)]
pub enum AgaveEvent {
    /// Bank `slot` (`bank_id`) was frozen by replay.
    Frozen {
        slot: Slot,
        bank_id: BankId,
        parent_slot: Slot,
        t: Instant,
    },
    /// Replay created bank `slot` with `bank_id` over `parent`.
    Created {
        slot: Slot,
        parent: Slot,
        bank_id: BankId,
    },
    /// Replay marked `slot` dead.
    Dead { slot: Slot },
    /// `slot` was rooted.
    Rooted { slot: Slot },
}

pub struct TeeAccountsUpdateNotifier {
    pub(crate) inner: AccountsUpdateNotifier,
    shared: Arc<Shared>,
}

impl TeeAccountsUpdateNotifier {
    pub fn new(inner: AccountsUpdateNotifier, shared: Arc<Shared>) -> Self {
        Self { inner, shared }
    }
}

impl fmt::Debug for TeeAccountsUpdateNotifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TeeAccountsUpdateNotifier")
            .field("inner", &self.inner)
            .finish()
    }
}

impl AccountsUpdateNotifierInterface for TeeAccountsUpdateNotifier {
    fn snapshot_notifications_enabled(&self) -> bool {
        self.inner.snapshot_notifications_enabled()
    }

    fn notify_account_update(
        &self,
        slot: Slot,
        bank_id: BankId,
        account: &AccountSharedData,
        txn: &Option<&SanitizedTransaction>,
        pubkey: &Pubkey,
        write_version: u64,
    ) {
        self.inner
            .notify_account_update(slot, bank_id, account, txn, pubkey, write_version)
    }

    fn notify_account_restore_from_snapshot(
        &self,
        slot: Slot,
        write_version: u64,
        account: &AccountForGeyser<'_>,
    ) {
        self.inner
            .notify_account_restore_from_snapshot(slot, write_version, account)
    }

    fn notify_end_of_restore_from_snapshot(&self) {
        self.inner.notify_end_of_restore_from_snapshot()
    }

    fn notify_transaction_accounts(
        &self,
        slot: Slot,
        bank_id: BankId,
        signature: &Signature,
        transaction_index: usize,
        accounts: &[(&Pubkey, &AccountSharedData)],
        write_version_start: u64,
    ) {
        self.inner.notify_transaction_accounts(
            slot,
            bank_id,
            signature,
            transaction_index,
            accounts,
            write_version_start,
        );
        // Only frames that can meet an FL result: not the catch-up backlog or slots FL did
        // not run, and not in commit mode (agave's notification then comes from FL's commit).
        if !control::is_active()
            || !self.shared.comparator_enabled
            || control::commit_mode() == control::COMMIT_ON
            || !control::wants_agave_slot(slot)
        {
            return;
        }
        let frame = AgaveFrame {
            slot,
            bank_id,
            signature: *signature,
            accounts: accounts
                .iter()
                .map(|(key, account)| (**key, (*account).clone()))
                .collect(),
            t: Instant::now(),
            t_unix_ns: unix_ns(),
        };
        TapStats::inc(&self.shared.tap_stats.agave_frames);
        let bytes = crate::compare::agave_frame_bytes(&frame);
        crate::mem::FRAME_QUEUE_BYTES.add(bytes);
        if self.shared.frame_tx.try_send(frame).is_err() {
            crate::mem::FRAME_QUEUE_BYTES.sub(bytes);
            TapStats::inc(&self.shared.tap_stats.agave_frame_drops);
        }
    }

    fn transaction_accounts_notifications_enabled(&self) -> bool {
        self.inner.transaction_accounts_notifications_enabled()
    }

    fn transaction_accounts_include_readonly_owners(&self) -> Vec<Pubkey> {
        self.inner.transaction_accounts_include_readonly_owners()
    }
}

fn send_event(shared: &Shared, event: AgaveEvent) {
    if !control::is_active() {
        return;
    }
    TapStats::inc(&shared.tap_stats.events);
    if shared.event_tx.try_send(event).is_err() {
        TapStats::inc(&shared.tap_stats.event_drops);
    }
}

pub struct TeeBlockMetadataNotifier {
    inner: Option<BlockMetadataNotifierArc>,
    shared: Arc<Shared>,
}

impl TeeBlockMetadataNotifier {
    pub fn new(inner: Option<BlockMetadataNotifierArc>, shared: Arc<Shared>) -> Self {
        Self { inner, shared }
    }
}

impl BlockMetadataNotifier for TeeBlockMetadataNotifier {
    fn notify_block_metadata(
        &self,
        parent_slot: u64,
        parent_blockhash: &str,
        slot: u64,
        bank_id: BankId,
        blockhash: &str,
        rewards: &KeyedRewardsAndNumPartitions,
        block_time: Option<UnixTimestamp>,
        block_height: Option<u64>,
        executed_transaction_count: u64,
        entry_count: u64,
        commission_rate_in_basis_points: bool,
    ) {
        if let Some(inner) = &self.inner {
            inner.notify_block_metadata(
                parent_slot,
                parent_blockhash,
                slot,
                bank_id,
                blockhash,
                rewards,
                block_time,
                block_height,
                executed_transaction_count,
                entry_count,
                commission_rate_in_basis_points,
            );
        }
        send_event(
            &self.shared,
            AgaveEvent::Frozen {
                slot,
                bank_id,
                parent_slot,
                t: Instant::now(),
            },
        );
    }
}

pub struct TeeSlotStatusNotifier {
    inner: Option<SlotStatusNotifier>,
    shared: Arc<Shared>,
}

impl TeeSlotStatusNotifier {
    pub fn new(inner: Option<SlotStatusNotifier>, shared: Arc<Shared>) -> Self {
        Self { inner, shared }
    }

    fn with_inner(&self, f: impl FnOnce(&(dyn SlotStatusNotifierInterface + Sync + Send))) {
        if let Some(inner) = &self.inner {
            f(&*inner.read().unwrap());
        }
    }
}

impl SlotStatusNotifierInterface for TeeSlotStatusNotifier {
    fn notify_slot_confirmed(&self, slot: Slot, parent: Option<Slot>, bank_id: BankId) {
        self.with_inner(|inner| inner.notify_slot_confirmed(slot, parent, bank_id));
    }

    fn notify_slot_processed(&self, slot: Slot, parent: Option<Slot>, bank_id: BankId) {
        self.with_inner(|inner| inner.notify_slot_processed(slot, parent, bank_id));
    }

    fn notify_slot_rooted(&self, slot: Slot, parent: Option<Slot>, bank_id: BankId) {
        self.with_inner(|inner| inner.notify_slot_rooted(slot, parent, bank_id));
        send_event(&self.shared, AgaveEvent::Rooted { slot });
    }

    fn notify_first_shred_received(&self, slot: Slot) {
        self.with_inner(|inner| inner.notify_first_shred_received(slot));
    }

    fn notify_completed(&self, slot: Slot) {
        self.with_inner(|inner| inner.notify_completed(slot));
    }

    fn notify_created_bank(&self, slot: Slot, parent: Slot, bank_id: BankId) {
        self.with_inner(|inner| inner.notify_created_bank(slot, parent, bank_id));
        send_event(
            &self.shared,
            AgaveEvent::Created {
                slot,
                parent,
                bank_id,
            },
        );
    }

    fn notify_slot_dead(&self, slot: Slot, parent: Slot, error: String) {
        self.with_inner(|inner| inner.notify_slot_dead(slot, parent, error));
        send_event(&self.shared, AgaveEvent::Dead { slot });
    }
}

/// Wrap `inner` so FL observes grouped transaction notifications. Without an inner
/// notifier nothing is installed: an accounts-update notifier makes accounts-db notify
/// every store, which FL must not impose on agave.
pub fn tee_accounts_update(
    inner: Option<AccountsUpdateNotifier>,
    shared: &Arc<Shared>,
) -> Option<AccountsUpdateNotifier> {
    let inner = inner?;
    let _ = shared
        .readonly_owners
        .set(inner.transaction_accounts_include_readonly_owners());
    Some(Arc::new(TeeAccountsUpdateNotifier::new(inner, Arc::clone(shared))))
}

/// Wrap (or install) a block-metadata notifier so FL learns about frozen banks.
pub fn tee_block_metadata(
    inner: Option<BlockMetadataNotifierArc>,
    shared: &Arc<Shared>,
) -> Option<BlockMetadataNotifierArc> {
    Some(Arc::new(TeeBlockMetadataNotifier::new(inner, Arc::clone(shared))))
}

/// Wrap (or install) a slot-status notifier so FL learns about dead and rooted slots.
pub fn tee_slot_status(
    inner: Option<SlotStatusNotifier>,
    shared: &Arc<Shared>,
) -> Option<SlotStatusNotifier> {
    Some(Arc::new(RwLock::new(TeeSlotStatusNotifier::new(
        inner,
        Arc::clone(shared),
    ))))
}
