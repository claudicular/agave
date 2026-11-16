//! Read-only accessors on a **frozen parent bank** for the speculative fast lane
//! (`agave-fast-lane`).
//!
//! The fast lane executes the transactions of a child slot N over the frozen parent P and
//! a private multi-version overlay, without ever creating a `Bank` for N (a throwaway child
//! bank would write sysvars into the shared, slot-keyed accounts write cache and purge that
//! slot on drop). Everything the child's execution environment needs is computed here as a
//! pure function of P and N, mirroring `_new_from_parent` / `prepare_for_block_execution` /
//! `load_and_execute_transactions` for the supported case: P frozen, N in P's epoch, N not
//! the last slot of the epoch, no reward distribution, no hard-fork restart slot change, and
//! either a TowerBFT P (alpenglow feature inactive) or an Alpenglow P whose child cannot
//! switch parents mid-slot (no fast leader handover window start). Every other child is
//! refused with [`FastLaneUnsupported`].
//!
//! Nothing in this module mutates any bank, accounts-db or program-cache state, and account
//! reads use `LoadHint::Unspecified` (the fast lane runs off the replay thread, so the root
//! may move during a load).

use {
    super::{Bank, EpochRewardStatus},
    agave_feature_set::FeatureSet,
    crate::{
        leader_schedule_utils::leader_slot_index,
        sysvar_account::{create_account, from_account},
    },
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::{accounts::Accounts, blockhash_queue::BlockhashQueue},
    solana_address_lookup_table_interface::error::AddressLookupError,
    solana_builtins::BUILTINS,
    solana_clock::{BankId, Clock, Epoch, Slot},
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_hash::Hash,
    solana_message::{
        AddressLoader, VersionedMessage,
        v0::{LoadedAddresses, MessageAddressTableLookup},
    },
    solana_nonce::state::DurableNonce,
    solana_packet::PACKET_DATA_SIZE,
    solana_program_runtime::{
        execution_budget::{
            SVMTransactionExecutionAndFeeBudgetLimits, SVMTransactionExecutionCost,
        },
        loaded_programs::{ProgramRuntimeEnvironment, ProgramRuntimeEnvironments},
        program_cache_entry::{DELAY_VISIBILITY_SLOT_OFFSET, ProgramCacheEntry},
    },
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_runtime_transaction::{
        runtime_transaction::RuntimeTransaction, transaction_with_meta::TransactionWithMeta,
    },
    solana_sdk_ids::sysvar,
    solana_slot_hashes::SlotHashes,
    solana_svm::{
        account_loader::{CheckedTransactionDetails, TransactionCheckResult},
        transaction_processor::TransactionProcessingEnvironment,
    },
    solana_svm_transaction::message_address_table_lookup::SVMMessageAddressTableLookup,
    solana_sysvar::last_restart_slot::LastRestartSlot,
    solana_transaction::{
        sanitized::{MessageHash, SanitizedTransaction},
        versioned::{TransactionVersion, VersionedTransaction},
    },
    solana_transaction_error::{AddressLoaderError, TransactionError, TransactionResult},
    std::{
        collections::{HashMap, HashSet},
        sync::Arc,
    },
};

/// Why the fast lane cannot execute a given child of this bank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastLaneUnsupported {
    /// The parent bank has not been frozen yet.
    ParentNotFrozen,
    /// The unfrozen parent's fee collector is unknown.
    UnknownFeeCollector,
    /// The child slot is not greater than the parent slot.
    ChildNotAfterParent,
    /// The alpenglow feature is active but the parent is not an Alpenglow bank: the
    /// migration is in flight (TowerBFT or vote-only banks), or the child is the first
    /// Alpenglow block (its GenesisCertificate marker turns it Alpenglow mid-slot, over a
    /// TowerBFT start Clock).
    AlpenglowMigration,
    /// Alpenglow fast leader handover is active and the child is the first slot of its
    /// leader window: its leader may abandon the slot's prefix with an UpdateParent marker
    /// and rebuild it on another parent.
    FastLeaderHandover,
    /// The child's unfrozen parent is (or may become) an Alpenglow bank: the child's Clock
    /// and the parent's footer writes (nanosecond clock, vote rewards) come from the
    /// parent's block footer, which chained contexts do not model. The child must wait for
    /// the parent's freeze.
    ChainedAlpenglow,
    /// The child starts a new epoch (feature activation, rewards, stake history ...).
    EpochBoundary,
    /// The child is the last slot of its epoch: its deployment environment may be the next
    /// epoch's.
    LastSlotOfEpoch,
    /// Partitioned epoch rewards are being calculated or distributed.
    RewardDistribution,
    /// A hard fork makes the child write a new `LastRestartSlot` sysvar.
    LastRestartSlotChange,
    /// A sysvar the child derives from could not be read or decoded.
    BadSysvar,
    /// A builtin id registered in the parent's processor has no prototype.
    UnknownBuiltin,
}

/// The child slot's execution environment, computed from its frozen parent.
///
/// For a supported child this is identical to what `Bank::new_from_parent(parent, _, child)`
/// would build: only the Clock and SlotHashes sysvars differ from the parent (see the
/// `fast_lane` tests, which compare against a real child bank byte for byte).
pub struct FastLaneChildContext {
    pub parent_slot: Slot,
    pub parent_bank_id: BankId,
    pub parent_hash: Hash,
    pub child_slot: Slot,
    pub epoch: Epoch,
    /// The child's Clock sysvar value and account.
    pub clock: Clock,
    pub clock_account: AccountSharedData,
    /// The child's SlotHashes sysvar value and account. For a chained context (parent not
    /// frozen yet) the parent's entry carries a placeholder hash: see
    /// [`Bank::fast_lane_resolve_slot_hashes`].
    pub slot_hashes: Arc<SlotHashes>,
    pub slot_hashes_account: AccountSharedData,
    /// Chained contexts only: the parent's own SlotHashes (value and account), from which
    /// the child's is derived once the parent's bank hash is known.
    parent_slot_hashes: Option<(Arc<SlotHashes>, AccountSharedData)>,
    /// Chained contexts only: the child's RecentBlockhashes account (the parent registers
    /// its last blockhash at its last tick, after FL finished it but before agave freezes).
    recent_blockhashes_account: Option<AccountSharedData>,
    /// The parent's blockhash queue (the child's until its last tick).
    pub(crate) blockhash_queue: BlockhashQueue,
    next_durable_nonce: DurableNonce,
    pub blockhash: Hash,
    pub blockhash_lamports_per_signature: u64,
    pub max_processing_age: usize,
    pub feature_set: Arc<FeatureSet>,
    pub transaction_account_lock_limit: usize,
    epoch_total_stake: u64,
    execution_environment: ProgramRuntimeEnvironment,
    deployment_environment: ProgramRuntimeEnvironment,
    rent: Rent,
    /// The child bank's `is_alpenglow()` (inherited from the parent).
    alpenglow_migration_succeeded: bool,
}

impl FastLaneChildContext {
    /// The `TransactionProcessingEnvironment` the child bank would pass to the SVM.
    pub fn processing_environment(&self) -> TransactionProcessingEnvironment {
        TransactionProcessingEnvironment {
            blockhash: self.blockhash,
            blockhash_lamports_per_signature: self.blockhash_lamports_per_signature,
            // Agave passes the child bank's `is_alpenglow()`: legacy vote instructions fail.
            alpenglow_migration_succeeded: self.alpenglow_migration_succeeded,
            epoch_total_stake: self.epoch_total_stake,
            feature_set: self.feature_set.runtime_features(),
            program_runtime_environments: ProgramRuntimeEnvironments::new(
                self.execution_environment.clone(),
                self.deployment_environment.clone(),
            ),
            rent: self.rent.clone(),
        }
    }

    /// The execution program-runtime environment (pointer identity matters for the
    /// program cache).
    pub fn execution_environment(&self) -> &ProgramRuntimeEnvironment {
        &self.execution_environment
    }

    /// The child's value of a sysvar that differs from the parent's, if `key` is one.
    pub fn sysvar_override(&self, key: &Pubkey) -> Option<&AccountSharedData> {
        if *key == sysvar::clock::id() {
            Some(&self.clock_account)
        } else if *key == sysvar::slot_hashes::id() {
            Some(&self.slot_hashes_account)
        } else {
            #[allow(deprecated)]
            let recent_blockhashes = sysvar::recent_blockhashes::id();
            if *key == recent_blockhashes {
                self.recent_blockhashes_account.as_ref()
            } else {
                None
            }
        }
    }

    /// Whether this context was built for a child of an unfrozen parent (its SlotHashes
    /// holds a placeholder for the parent's bank hash).
    pub fn is_chained(&self) -> bool {
        self.parent_slot_hashes.is_some()
    }

    /// `durable nonce` the child's nonce advances would store (from the parent's last
    /// blockhash, like `BlockhashQueue::next_durable_nonce`).
    pub fn next_durable_nonce(&self) -> &DurableNonce {
        &self.next_durable_nonce
    }
}

/// Result of the state-independent part of `check_transactions` for one transaction.
#[derive(Debug, Clone)]
pub enum FastLaneStaticCheck {
    /// The recent blockhash is valid for age: ready to execute.
    Ready(CheckedTransactionDetails),
    /// The recent blockhash is not in the queue: the durable-nonce path must be checked
    /// against the nonce account's value at the transaction's position in the slot.
    NeedsNonce(SVMTransactionExecutionAndFeeBudgetLimits),
    /// The transaction is unprocessable (agave would fail the block).
    Err(TransactionError),
}

/// What the fast lane needs to build its private `TransactionBatchProcessor` so that it
/// behaves exactly like this bank's.
pub struct FastLaneProgramSetup {
    pub slot: Slot,
    pub epoch: Epoch,
    /// The execution environment Arc (identity-compared by the program cache).
    pub program_runtime_environment: ProgramRuntimeEnvironment,
    pub execution_cost: SVMTransactionExecutionCost,
    /// Builtins registered in this bank's processor, rebuilt from their prototypes.
    pub builtins: Vec<(Pubkey, ProgramCacheEntry)>,
}

/// What the fast lane knows about an **unfrozen** parent P when it has executed all of P's
/// transactions itself, for building P's child's context on top of FL's own results.
pub struct FastLaneChainedParent {
    /// P's own context (built over P's frozen parent, which is the bank the chained context
    /// is computed on).
    pub ctx: Arc<FastLaneChildContext>,
    /// The hash of P's last tick entry: the blockhash P registers at its block boundary.
    pub last_blockhash: Hash,
    /// P's `fee_rate_governor.lamports_per_signature` (from agave's unfrozen bank P).
    pub lamports_per_signature: u64,
    /// Final values of the vote-program accounts P's transactions wrote.
    pub vote_accounts: Vec<(Pubkey, AccountSharedData)>,
}

/// How a written vote-program account changes the stakes cache's vote accounts (mirrors
/// `StakesCache::check_and_store`): `None` = no change, `Some(None)` = removed,
/// `Some(Some(ts))` = present with last timestamp `ts`.
fn vote_timestamp_after(account: &AccountSharedData) -> Option<Option<(Slot, i64)>> {
    if !solana_vote_program::check_id(account.owner()) {
        return None;
    }
    if account.lamports() == 0 {
        return Some(None);
    }
    if !solana_vote_interface::state::VoteStateVersions::is_correct_size_and_initialized(
        account.data(),
    ) {
        return Some(None);
    }
    match solana_vote::vote_account::VoteAccount::try_from(account.clone()) {
        Ok(vote_account) => {
            let ts = vote_account.vote_state_view().last_timestamp();
            Some(Some((ts.slot, ts.timestamp)))
        }
        Err(_) => Some(None),
    }
}

/// An `AddressLoader` resolving a child slot's lookups against the frozen parent's state,
/// with `current_slot = child` and the child's SlotHashes (see
/// `Accounts::lookup_table_addresses_into`). Table changes made inside the child slot can
/// never affect a lookup in that slot (extensions are hidden until the next slot, a
/// deactivating table stays usable, a closable table is already unusable, a new table is
/// empty), so this equals the child bank's own resolution at any point of its execution.
#[derive(Clone, Copy)]
pub struct FastLaneAddressLoader<'a> {
    load_table: &'a dyn Fn(&Pubkey) -> Option<AccountSharedData>,
    ctx: &'a FastLaneChildContext,
}

impl AddressLoader for FastLaneAddressLoader<'_> {
    fn load_addresses(
        self,
        lookups: &[MessageAddressTableLookup],
    ) -> Result<LoadedAddresses, AddressLoaderError> {
        Bank::fast_lane_load_addresses_with(
            self.ctx,
            lookups.iter().map(SVMMessageAddressTableLookup::from),
            self.load_table,
        )
        .map(|(loaded_addresses, _deactivation_slot)| loaded_addresses)
    }
}

fn into_address_loader_error(err: AddressLookupError) -> AddressLoaderError {
    match err {
        AddressLookupError::LookupTableAccountNotFound => {
            AddressLoaderError::LookupTableAccountNotFound
        }
        AddressLookupError::InvalidAccountOwner => AddressLoaderError::InvalidAccountOwner,
        AddressLookupError::InvalidAccountData => AddressLoaderError::InvalidAccountData,
        AddressLookupError::InvalidLookupIndex => AddressLoaderError::InvalidLookupIndex,
    }
}

impl Bank {
    /// Whether the fast lane can execute `child_slot` over this (frozen) bank.
    pub fn fast_lane_support(&self, child_slot: Slot) -> Result<(), FastLaneUnsupported> {
        if !self.is_frozen() {
            return Err(FastLaneUnsupported::ParentNotFrozen);
        }
        if child_slot <= self.slot() {
            return Err(FastLaneUnsupported::ChildNotAfterParent);
        }
        let features = self.feature_set.snapshot();
        // An Alpenglow parent's child is an Alpenglow bank from its creation
        // (`is_alpenglow` is inherited): footer Clock and markers, no TowerBFT votes.
        if features.alpenglow && !self.is_alpenglow() {
            return Err(FastLaneUnsupported::AlpenglowMigration);
        }
        let epoch_schedule = self.epoch_schedule();
        let child_epoch = epoch_schedule.get_epoch(child_slot);
        if child_epoch != self.epoch() {
            return Err(FastLaneUnsupported::EpochBoundary);
        }
        if epoch_schedule.get_epoch(child_slot.saturating_add(DELAY_VISIBILITY_SLOT_OFFSET))
            != child_epoch
        {
            return Err(FastLaneUnsupported::LastSlotOfEpoch);
        }
        // Replay accepts an UpdateParent marker only with fast leader handover active
        // (`BlockComponentProcessor::on_marker`, checked on the child's feature set, which
        // equals this bank's within an epoch) and only in the first slot of a leader window
        // (`leader_slot_index(slot) == 0`, enforced by blockstore and replay).
        if self.is_alpenglow()
            && features.alpenglow_fast_leader_handover
            && leader_slot_index(child_slot) == 0
        {
            return Err(FastLaneUnsupported::FastLeaderHandover);
        }
        if matches!(self.epoch_reward_status, EpochRewardStatus::Active(_)) {
            return Err(FastLaneUnsupported::RewardDistribution);
        }
        // Mirror `update_last_restart_slot` for the child: it writes only if the value
        // derived from the hard forks at `child_slot` differs from the stored one.
        let current_last_restart_slot = self
            .get_account(&sysvar::last_restart_slot::id())
            .and_then(|account| from_account::<LastRestartSlot>(&account))
            .map(|lrs| lrs.last_restart_slot);
        let child_last_restart_slot = {
            let hard_forks = self.hard_forks.read().unwrap();
            hard_forks
                .iter()
                .rev()
                .find(|(hard_fork, _)| *hard_fork <= child_slot)
                .map(|(slot, _)| *slot)
                .unwrap_or(0)
        };
        if current_last_restart_slot != Some(child_last_restart_slot) {
            return Err(FastLaneUnsupported::LastRestartSlotChange);
        }
        Ok(())
    }

    /// Whether a grandchild of this (frozen) bank can be built on top of FL's own run of its
    /// unfrozen child ([`Self::fast_lane_chained_child_context`]): never once the alpenglow
    /// feature is active (the unfrozen child is, or may become, an Alpenglow bank whose
    /// block footer sets the grandchild's Clock).
    pub fn fast_lane_chaining_supported(&self) -> bool {
        !self.is_alpenglow() && !self.feature_set.snapshot().alpenglow
    }

    /// The child's Clock, exactly as `prepare_for_block_execution` writes it in a same-epoch
    /// child created from this frozen bank: `update_clock_slot_for_alpenglow` for an
    /// Alpenglow parent (this bank's Clock with the child's slot fields: the timestamps are
    /// the ones this bank's block footer wrote), else the TowerBFT
    /// `update_clock(Some(parent_epoch))` vote-timestamp estimate.
    fn fast_lane_child_clock(
        &self,
        child_slot: Slot,
    ) -> Result<(Clock, AccountSharedData), FastLaneUnsupported> {
        // The child reads the parent's clock account (it has not written its own yet).
        let old_account = self.get_account(&sysvar::clock::id());
        let parent_clock: Clock = old_account
            .as_ref()
            .map(|account| from_account::<Clock>(account).ok_or(FastLaneUnsupported::BadSysvar))
            .transpose()?
            .unwrap_or_default();

        let parent_epoch = self.epoch();
        let epoch_schedule = self.epoch_schedule();
        let clock = if self.is_alpenglow() {
            // `update_clock_slot_for_alpenglow` in the child, where `self.clock()` is this
            // bank's Clock and `self.parent()` is this bank. Its `(0, _)` arm (genesis)
            // cannot apply to a child.
            let child_epoch = epoch_schedule.get_epoch(child_slot);
            let epoch_start_timestamp = if parent_epoch != child_epoch {
                parent_clock.unix_timestamp
            } else {
                parent_clock.epoch_start_timestamp
            };
            Clock {
                slot: child_slot,
                epoch: child_epoch,
                leader_schedule_epoch: epoch_schedule.get_leader_schedule_epoch(child_slot),
                epoch_start_timestamp,
                unix_timestamp: parent_clock.unix_timestamp,
            }
        } else {
            let first_slot_in_epoch = epoch_schedule.get_first_slot_in_epoch(parent_epoch);
            let epoch_start_timestamp =
                Some((first_slot_in_epoch, parent_clock.epoch_start_timestamp));
            let max_allowable_drift = super::MaxAllowableDrift {
                fast: super::MAX_ALLOWABLE_DRIFT_PERCENTAGE_FAST,
                slow: super::MAX_ALLOWABLE_DRIFT_PERCENTAGE_SLOW_V2,
            };
            let ancestor_timestamp = parent_clock.unix_timestamp;
            let mut unix_timestamp = parent_clock.unix_timestamp;
            if let Some(timestamp_estimate) = self.get_timestamp_estimate_for_slot(
                child_slot,
                max_allowable_drift,
                epoch_start_timestamp,
            ) {
                unix_timestamp = timestamp_estimate;
                if timestamp_estimate < ancestor_timestamp {
                    unix_timestamp = ancestor_timestamp;
                }
            }
            // Same epoch as the parent (checked by `fast_lane_support`): no epoch start
            // update.
            Clock {
                slot: child_slot,
                epoch_start_timestamp: parent_clock.epoch_start_timestamp,
                epoch: epoch_schedule.get_epoch(child_slot),
                leader_schedule_epoch: epoch_schedule.get_leader_schedule_epoch(child_slot),
                unix_timestamp,
            }
        };
        // `update_sysvar_account`: fields inherited from the old account, then rent-adjusted.
        let mut account = create_account(
            &clock,
            self.inherit_specially_retained_account_fields(&old_account),
        );
        self.adjust_sysvar_balance_for_rent(&mut account);
        Ok((clock, account))
    }

    /// The Clock sysvar account this frozen bank's transactions read. An Alpenglow bank's
    /// block footer rewrites the Clock after the slot's last transaction
    /// (`update_clock_from_footer`: the footer's time, and on an epoch's first slot also the
    /// epoch start); this rebuilds the value `update_clock_slot_for_alpenglow` wrote at the
    /// bank's creation: the frozen Clock's slot fields with the parent's timestamps (the
    /// footer keeps the account's lamports and rent epoch). `None` when the parent is no
    /// longer linked (squashed into a root). Not exact for the first Alpenglow block, which
    /// starts with a TowerBFT Clock (the fast lane never runs it).
    pub fn fast_lane_execution_clock_account(&self) -> Option<AccountSharedData> {
        let account = self.get_account(&sysvar::clock::id())?;
        if !self.is_alpenglow() {
            return Some(account);
        }
        let frozen: Clock = from_account(&account)?;
        let parent = self.parent()?;
        let parent_clock = parent.clock();
        let epoch_start_timestamp = if parent.epoch() != self.epoch() {
            parent_clock.unix_timestamp
        } else {
            frozen.epoch_start_timestamp
        };
        let clock = Clock {
            epoch_start_timestamp,
            unix_timestamp: parent_clock.unix_timestamp,
            ..frozen
        };
        let mut start = create_account(
            &clock,
            self.inherit_specially_retained_account_fields(&Some(account)),
        );
        self.adjust_sysvar_balance_for_rent(&mut start);
        Some(start)
    }

    /// The accounts an Alpenglow bank's block footer writes after the slot's last
    /// transaction besides vote accounts (`update_bank_with_footer_fields`): the Clock
    /// sysvar and the nanosecond clock account.
    pub fn fast_lane_alpenglow_footer_accounts() -> [Pubkey; 2] {
        [sysvar::clock::id(), *super::NANOSECOND_CLOCK_ACCOUNT]
    }

    /// The child's SlotHashes, exactly as `update_slot_hashes` computes it in a child of
    /// this frozen bank.
    fn fast_lane_child_slot_hashes(
        &self,
    ) -> Result<(SlotHashes, AccountSharedData), FastLaneUnsupported> {
        let old_account = self.get_account(&sysvar::slot_hashes::id());
        let mut slot_hashes = match old_account.as_ref() {
            Some(account) => wincode::deserialize::<SlotHashes>(account.data())
                .map_err(|_| FastLaneUnsupported::BadSysvar)?,
            None => SlotHashes::default(),
        };
        slot_hashes.add(self.slot(), self.hash());
        let mut account = create_account(
            &slot_hashes,
            self.inherit_specially_retained_account_fields(&old_account),
        );
        self.adjust_sysvar_balance_for_rent(&mut account);
        Ok((slot_hashes, account))
    }

    /// Compute the child slot's execution environment from this frozen bank.
    pub fn fast_lane_child_context(
        &self,
        child_slot: Slot,
    ) -> Result<FastLaneChildContext, FastLaneUnsupported> {
        self.fast_lane_support(child_slot)?;
        let (clock, clock_account) = self.fast_lane_child_clock(child_slot)?;
        let (slot_hashes, slot_hashes_account) = self.fast_lane_child_slot_hashes()?;
        let blockhash_queue = self.blockhash_queue.read().unwrap().clone();
        let next_durable_nonce = blockhash_queue.next_durable_nonce();
        let (blockhash, blockhash_lamports_per_signature) =
            self.last_blockhash_and_lamports_per_signature();
        let effective_epoch_of_deployments = self
            .epoch_schedule()
            .get_epoch(child_slot.saturating_add(DELAY_VISIBILITY_SLOT_OFFSET));
        let execution_environment = self
            .transaction_processor
            .program_runtime_environment
            .clone();
        let deployment_environment = self
            .transaction_processor
            .program_runtime_environment_for_epoch(effective_epoch_of_deployments);
        Ok(FastLaneChildContext {
            parent_slot: self.slot(),
            parent_bank_id: self.bank_id(),
            parent_hash: self.hash(),
            child_slot,
            epoch: self.epoch(),
            clock,
            clock_account,
            slot_hashes: Arc::new(slot_hashes),
            slot_hashes_account,
            parent_slot_hashes: None,
            recent_blockhashes_account: None,
            blockhash_queue,
            next_durable_nonce,
            blockhash,
            blockhash_lamports_per_signature,
            max_processing_age: self.max_processing_age(),
            feature_set: Arc::clone(&self.feature_set),
            transaction_account_lock_limit: self.get_transaction_account_lock_limit(),
            epoch_total_stake: self.get_current_epoch_total_stake(),
            execution_environment,
            deployment_environment,
            rent: self.rent_collector.rent.clone(),
            alpenglow_migration_succeeded: self.is_alpenglow(),
        })
    }

    /// The context of `child_slot` whose parent P is **not frozen yet**, on top of FL's own
    /// execution of all of P. `self` is P's frozen parent (the bank P's own context was built
    /// on). Identical to what `Bank::new_from_parent(P, _, child_slot)` would build, except
    /// that SlotHashes carries `Hash::default()` for P (P's bank hash is unknown until agave
    /// freezes it); readers of SlotHashes must be treated as provisional and validated with
    /// [`Self::fast_lane_resolve_slot_hashes`].
    ///
    /// P's freeze-time writes (fee deposits, SlotHistory, the incinerator) are
    /// not applied here either; the caller treats reads of those accounts as provisional.
    pub fn fast_lane_chained_child_context(
        &self,
        parent: &FastLaneChainedParent,
        child_slot: Slot,
    ) -> Result<FastLaneChildContext, FastLaneUnsupported> {
        let p = &parent.ctx;
        if !self.is_frozen() {
            return Err(FastLaneUnsupported::ParentNotFrozen);
        }
        if p.parent_slot != self.slot() || child_slot <= p.child_slot {
            return Err(FastLaneUnsupported::ChildNotAfterParent);
        }
        // An Alpenglow child's Clock is P's footer time and P's footer rewrites the
        // nanosecond clock and vote accounts after P's transactions: none of it is known
        // here. The child runs over the frozen P instead.
        if !self.fast_lane_chaining_supported() || p.alpenglow_migration_succeeded {
            return Err(FastLaneUnsupported::ChainedAlpenglow);
        }
        // The child must itself be supported relative to P: same epoch, not the epoch's last
        // slot, no rewards (P's reward status equals self's within an epoch), no
        // LastRestartSlot change.
        self.fast_lane_support(child_slot)?;

        let overrides: HashMap<Pubkey, Option<(Slot, i64)>> = parent
            .vote_accounts
            .iter()
            .filter_map(|(key, account)| vote_timestamp_after(account).map(|ts| (*key, ts)))
            .collect();
        let parent_clock = &p.clock;
        let epoch_schedule = self.epoch_schedule();
        let epoch_start_timestamp = Some((
            epoch_schedule.get_first_slot_in_epoch(self.epoch()),
            parent_clock.epoch_start_timestamp,
        ));
        let max_allowable_drift = super::MaxAllowableDrift {
            fast: super::MAX_ALLOWABLE_DRIFT_PERCENTAGE_FAST,
            slow: super::MAX_ALLOWABLE_DRIFT_PERCENTAGE_SLOW_V2,
        };
        let ancestor_timestamp = parent_clock.unix_timestamp;
        let mut unix_timestamp = parent_clock.unix_timestamp;
        if let Some(estimate) = self.get_timestamp_estimate_for_slot_with(
            child_slot,
            max_allowable_drift,
            epoch_start_timestamp,
            &overrides,
        ) {
            unix_timestamp = estimate;
            if estimate < ancestor_timestamp {
                unix_timestamp = ancestor_timestamp;
            }
        }
        let clock = Clock {
            slot: child_slot,
            epoch_start_timestamp: parent_clock.epoch_start_timestamp,
            epoch: epoch_schedule.get_epoch(child_slot),
            leader_schedule_epoch: epoch_schedule.get_leader_schedule_epoch(child_slot),
            unix_timestamp,
        };
        let mut clock_account = create_account(
            &clock,
            self.inherit_specially_retained_account_fields(&Some(p.clock_account.clone())),
        );
        self.adjust_sysvar_balance_for_rent(&mut clock_account);

        let (slot_hashes, slot_hashes_account) =
            self.child_slot_hashes_of(&p.slot_hashes, &p.slot_hashes_account, p.child_slot, Hash::default());

        let mut blockhash_queue = p.blockhash_queue.clone();
        blockhash_queue.register_hash(&parent.last_blockhash, parent.lamports_per_signature);
        #[allow(deprecated)]
        let recent_blockhashes_id = sysvar::recent_blockhashes::id();
        let old_recent_blockhashes = p
            .recent_blockhashes_account
            .clone()
            .or_else(|| self.get_account(&recent_blockhashes_id));
        #[allow(deprecated)]
        let recent_blockhash_iter = blockhash_queue.get_recent_blockhashes();
        let mut recent_blockhashes_account =
            super::recent_blockhashes_account::create_account_with_data_and_fields(
                recent_blockhash_iter,
                self.inherit_specially_retained_account_fields(&old_recent_blockhashes),
            );
        self.adjust_sysvar_balance_for_rent(&mut recent_blockhashes_account);
        let next_durable_nonce = blockhash_queue.next_durable_nonce();
        let effective_epoch_of_deployments = epoch_schedule
            .get_epoch(child_slot.saturating_add(DELAY_VISIBILITY_SLOT_OFFSET));
        Ok(FastLaneChildContext {
            parent_slot: p.child_slot,
            parent_bank_id: 0,
            parent_hash: Hash::default(),
            child_slot,
            epoch: self.epoch(),
            clock,
            clock_account,
            slot_hashes: Arc::new(slot_hashes),
            slot_hashes_account,
            parent_slot_hashes: Some((Arc::clone(&p.slot_hashes), p.slot_hashes_account.clone())),
            recent_blockhashes_account: Some(recent_blockhashes_account),
            blockhash_queue,
            next_durable_nonce,
            blockhash: parent.last_blockhash,
            blockhash_lamports_per_signature: parent.lamports_per_signature,
            max_processing_age: p.max_processing_age,
            feature_set: Arc::clone(&p.feature_set),
            transaction_account_lock_limit: p.transaction_account_lock_limit,
            epoch_total_stake: p.epoch_total_stake,
            execution_environment: p.execution_environment.clone(),
            deployment_environment: self
                .transaction_processor
                .program_runtime_environment_for_epoch(effective_epoch_of_deployments),
            rent: p.rent.clone(),
            alpenglow_migration_succeeded: p.alpenglow_migration_succeeded,
        })
    }

    /// `parent_slot_hashes` plus `(parent_slot, parent_hash)`, as `update_slot_hashes` builds
    /// it in the child (account fields inherited from `parent_account`).
    fn child_slot_hashes_of(
        &self,
        parent_slot_hashes: &SlotHashes,
        parent_account: &AccountSharedData,
        parent_slot: Slot,
        parent_hash: Hash,
    ) -> (SlotHashes, AccountSharedData) {
        let mut slot_hashes = SlotHashes::new(parent_slot_hashes.slot_hashes());
        slot_hashes.add(parent_slot, parent_hash);
        let mut account = create_account(
            &slot_hashes,
            self.inherit_specially_retained_account_fields(&Some(parent_account.clone())),
        );
        self.adjust_sysvar_balance_for_rent(&mut account);
        (slot_hashes, account)
    }

    /// The true SlotHashes of a chained context's child once its parent's bank hash is known.
    pub fn fast_lane_resolve_slot_hashes(
        &self,
        ctx: &FastLaneChildContext,
        parent_hash: Hash,
    ) -> Option<(Arc<SlotHashes>, AccountSharedData)> {
        let (parent_slot_hashes, parent_account) = ctx.parent_slot_hashes.as_ref()?;
        let (slot_hashes, account) =
            self.child_slot_hashes_of(parent_slot_hashes, parent_account, ctx.parent_slot, parent_hash);
        Some((Arc::new(slot_hashes), account))
    }

    /// `fee_rate_governor.lamports_per_signature` of this (possibly unfrozen) bank.
    pub fn fast_lane_lamports_per_signature(&self) -> u64 {
        self.fee_rate_governor.lamports_per_signature
    }

    /// The reserved account keys the child uses for sanitization (the parent's; they only
    /// change at an epoch boundary).
    pub fn fast_lane_reserved_account_keys(&self) -> &HashSet<Pubkey> {
        self.get_reserved_account_keys()
    }

    /// Resolve address table lookups for the child slot of `ctx` against this bank's state.
    pub fn fast_lane_load_addresses<'a>(
        &self,
        ctx: &FastLaneChildContext,
        address_table_lookups: impl Iterator<Item = SVMMessageAddressTableLookup<'a>>,
    ) -> Result<(LoadedAddresses, Slot), AddressLoaderError> {
        Self::fast_lane_load_addresses_with(ctx, address_table_lookups, &|key: &Pubkey| {
            self.get_account_modified_slot(key)
                .map(|(account, _slot)| account)
        })
    }

    /// [`Self::fast_lane_load_addresses`] with table accounts read by `load_table` (a chained
    /// child reads tables as FL's execution of its unfrozen parent left them).
    pub fn fast_lane_load_addresses_with<'a>(
        ctx: &FastLaneChildContext,
        address_table_lookups: impl Iterator<Item = SVMMessageAddressTableLookup<'a>>,
        load_table: &dyn Fn(&Pubkey) -> Option<AccountSharedData>,
    ) -> Result<(LoadedAddresses, Slot), AddressLoaderError> {
        let mut deactivation_slot = u64::MAX;
        let mut loaded_addresses = LoadedAddresses::default();
        for address_table_lookup in address_table_lookups {
            let table_account = load_table(address_table_lookup.account_key)
                .ok_or(AddressLoaderError::LookupTableAccountNotFound)?;
            deactivation_slot = deactivation_slot.min(
                Accounts::lookup_table_addresses_into(
                    &table_account,
                    ctx.child_slot,
                    address_table_lookup,
                    &ctx.slot_hashes,
                    &mut loaded_addresses,
                )
                .map_err(into_address_loader_error)?,
            );
        }
        Ok((loaded_addresses, deactivation_slot))
    }

    /// Sanitize and hash a transaction of the child slot exactly as replay does
    /// (`verify_transaction_with_serialized_message` in `HashOnly` mode), resolving address
    /// lookups for the child slot. Signatures are not verified (replay verifies them
    /// asynchronously and marks the slot dead on failure).
    pub fn fast_lane_verify_transaction(
        &self,
        ctx: &FastLaneChildContext,
        tx: VersionedTransaction,
        serialized_message: &[u8],
    ) -> TransactionResult<RuntimeTransaction<SanitizedTransaction>> {
        self.fast_lane_verify_transaction_with(ctx, tx, serialized_message, &|key: &Pubkey| {
            self.get_account_modified_slot(key)
                .map(|(account, _slot)| account)
        })
    }

    /// [`Self::fast_lane_verify_transaction`] with lookup-table accounts read by
    /// `load_table`.
    pub fn fast_lane_verify_transaction_with(
        &self,
        ctx: &FastLaneChildContext,
        tx: VersionedTransaction,
        serialized_message: &[u8],
        load_table: &dyn Fn(&Pubkey) -> Option<AccountSharedData>,
    ) -> TransactionResult<RuntimeTransaction<SanitizedTransaction>> {
        // Mirrors `Bank::verify_transaction` (v1 is always enabled since v4.4, #15270).
        let max_transaction_size = match tx.version() {
            TransactionVersion::Number(1) => solana_message::v1::MAX_TRANSACTION_SIZE,
            _ => PACKET_DATA_SIZE,
        } as u64;
        let size = wincode::serialized_size(&tx).map_err(|_| TransactionError::SanitizeFailure)?;
        if size > max_transaction_size {
            return Err(TransactionError::SanitizeFailure);
        }
        if tx.message.instructions().len()
            > solana_transaction_context::MAX_INSTRUCTION_TRACE_LENGTH
        {
            return Err(TransactionError::SanitizeFailure);
        }
        let message_hash = VersionedMessage::hash_raw_message(serialized_message);
        RuntimeTransaction::try_create(
            tx,
            MessageHash::Precomputed(message_hash),
            None,
            FastLaneAddressLoader { load_table, ctx },
            self.get_reserved_account_keys(),
        )
    }

    /// The state-independent part of `check_transactions` for the child slot: the compute
    /// budget and fee, and the blockhash-age check against the parent's
    /// blockhash queue. The status-cache check is skipped: a valid block never contains an
    /// already-processed transaction, and replay marks an invalid one dead.
    pub fn fast_lane_check_static(
        &self,
        ctx: &FastLaneChildContext,
        tx: &impl TransactionWithMeta,
    ) -> FastLaneStaticCheck {
        let feature_snapshot = ctx.feature_set.snapshot();
        let compute_budget_and_limits = match self.compute_budget_and_limits(
            tx,
            &ctx.feature_set,
            self.fee_features(),
            feature_snapshot.raise_cpi_nesting_limit_to_8,
        ) {
            Ok(limits) => limits,
            Err(err) => return FastLaneStaticCheck::Err(err),
        };
        if ctx
            .blockhash_queue
            .get_hash_info_if_valid(tx.recent_blockhash(), ctx.max_processing_age)
            .is_some()
        {
            FastLaneStaticCheck::Ready(CheckedTransactionDetails::new(
                None,
                compute_budget_and_limits,
            ))
        } else {
            FastLaneStaticCheck::NeedsNonce(compute_budget_and_limits)
        }
    }

    /// The durable-nonce part of `check_transaction_age` for the child slot, with the nonce
    /// account supplied by `load_nonce_account` (the fast lane's overlay value at the
    /// transaction's position). Replay uses non-strict size and authority checks.
    pub fn fast_lane_check_nonce(
        ctx: &FastLaneChildContext,
        tx: &impl TransactionWithMeta,
        compute_budget_and_limits: SVMTransactionExecutionAndFeeBudgetLimits,
        load_nonce_account: impl FnOnce(&Pubkey) -> Option<AccountSharedData>,
    ) -> TransactionCheckResult {
        match Self::durable_nonce_if_advanceable(tx, &ctx.next_durable_nonce).filter(
            |nonce_address| {
                // strict_nonce_checks: never in replay
                Self::check_nonce_account_with(tx, *nonce_address, false, load_nonce_account)
                    .is_some()
            },
        ) {
            Some(nonce_address) => Ok(CheckedTransactionDetails::new(
                Some(nonce_address),
                compute_budget_and_limits,
            )),
            None => Err(TransactionError::BlockhashNotFound),
        }
    }

    /// What the fast lane needs to configure a private processor identical to this bank's.
    pub fn fast_lane_program_setup(&self) -> Result<FastLaneProgramSetup, FastLaneUnsupported> {
        let processor = &self.transaction_processor;
        let builtin_ids: Vec<Pubkey> = processor
            .builtin_program_ids
            .read()
            .unwrap()
            .iter()
            .copied()
            .collect();
        let mut builtins = Vec::with_capacity(builtin_ids.len());
        for program_id in builtin_ids {
            // Mirrors `add_active_builtin_programs`.
            let prototype = BUILTINS
                .iter()
                .find(|builtin| builtin.program_id == program_id)
                .ok_or(FastLaneUnsupported::UnknownBuiltin)?;
            builtins.push((
                program_id,
                ProgramCacheEntry::new_builtin(prototype.register_fn),
            ));
        }
        Ok(FastLaneProgramSetup {
            slot: self.slot(),
            epoch: self.epoch(),
            program_runtime_environment: processor.program_runtime_environment.clone(),
            execution_cost: processor.execution_cost(),
            builtins,
        })
    }

    /// Loaded (compiled) program-cache entries deployed at or below `root` and compiled for
    /// `environment`, for seeding the fast lane's private cache without any JIT. Takes a read
    /// lock on this bank's global program cache for the duration of the copy.
    pub fn fast_lane_rooted_program_entries(
        &self,
        root: Slot,
        environment: &ProgramRuntimeEnvironment,
    ) -> Vec<(Pubkey, Arc<ProgramCacheEntry>)> {
        self.transaction_processor
            .global_program_cache
            .read()
            .unwrap()
            .get_flattened_entries()
            .into_iter()
            .filter(|(_key, entry)| {
                entry.deployment_slot <= root
                    && entry
                        .program
                        .get_environment()
                        .map(|env| env == environment)
                        .unwrap_or(false)
            })
            .collect()
    }

    /// The execution program-runtime environment Arc of this bank's processor.
    pub fn fast_lane_execution_environment(&self) -> ProgramRuntimeEnvironment {
        self.transaction_processor
            .program_runtime_environment
            .clone()
    }

    /// The accounts this bank deposits the slot's fees into at freeze: the validator-fee
    /// collector (the leader's identity, or with SIMD-0232 its vote account's block-revenue
    /// collector) and, with SIMD-0123 block revenue sharing, the leader's vote account.
    /// `None` if agave could not determine the collector either.
    pub fn fast_lane_fee_deposit_accounts(&self) -> Option<Vec<Pubkey>> {
        self.fee_deposit_accounts()
    }

    /// The compute budget configured for this bank, if any (test/diagnostic accessor).
    pub fn fast_lane_compute_budget(&self) -> Option<ComputeBudget> {
        self.compute_budget
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bank::{SlotLeader, test_utils::update_vote_account_timestamp},
            bank_forks::BankForks,
            genesis_utils::{
                GenesisConfigInfo, activate_all_features_alpenglow,
                create_genesis_config_with_leader, deactivate_features,
            },
        },
        agave_votor_messages::migration::GENESIS_CERTIFICATE_ACCOUNT,
        solana_signer::Signer,
        solana_vote_program::vote_state::BlockTimestamp,
        std::sync::{Arc, RwLock},
        test_case::test_case,
    };

    fn assert_same_account(a: &AccountSharedData, b: &AccountSharedData, what: &str) {
        assert_eq!(a.lamports(), b.lamports(), "{what}: lamports");
        assert_eq!(a.owner(), b.owner(), "{what}: owner");
        assert_eq!(a.executable(), b.executable(), "{what}: executable");
        assert_eq!(a.rent_epoch(), b.rent_epoch(), "{what}: rent_epoch");
        assert_eq!(a.data(), b.data(), "{what}: data");
    }

    /// A frozen parent at slot 4 whose vote account carries `timestamp` (if any) relative
    /// to the genesis-derived clock, so the stake-weighted estimate, the drift clamps and
    /// the monotonic clamp are all exercised.
    fn frozen_parent(
        timestamp_offset: Option<(Slot, i64)>,
    ) -> (Arc<Bank>, Arc<std::sync::RwLock<crate::bank_forks::BankForks>>) {
        let leader = solana_pubkey::new_rand();
        let GenesisConfigInfo {
            genesis_config,
            voting_keypair,
            ..
        } = create_genesis_config_with_leader(5_000_000_000, &leader, 1_000_000_000);
        let (bank0, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        let mut parent = bank0;
        for slot in 1..=4 {
            parent = Bank::new_from_parent_with_bank_forks(
                &bank_forks,
                parent,
                SlotLeader::default(),
                slot,
            );
        }
        if let Some((ts_slot, offset)) = timestamp_offset {
            update_vote_account_timestamp(
                BlockTimestamp {
                    slot: ts_slot,
                    timestamp: parent.unix_timestamp_from_genesis().saturating_add(offset),
                },
                &parent,
                &voting_keypair.pubkey(),
            );
        }
        parent.freeze();
        (parent, bank_forks)
    }

    #[test_case(None; "no timestamps")]
    #[test_case(Some((4, -10_000)); "timestamp behind -> slow clamp or monotonic clamp")]
    #[test_case(Some((4, 1_000_000)); "timestamp far ahead -> fast drift clamp")]
    #[test_case(Some((2, 3)); "older vote slot, small offset")]
    fn test_child_context_matches_new_from_parent(timestamp: Option<(Slot, i64)>) {
        let (parent, _bank_forks) = frozen_parent(timestamp);
        for child_slot in [parent.slot() + 1, parent.slot() + 3] {
            let ctx = parent.fast_lane_child_context(child_slot).unwrap();
            let child = Bank::new_from_parent(Arc::clone(&parent), SlotLeader::default(), child_slot);
            let child_clock = child.get_account(&sysvar::clock::id()).unwrap();
            assert_same_account(&ctx.clock_account, &child_clock, "clock");
            assert_eq!(ctx.clock, child.clock());
            let child_slot_hashes = child.get_account(&sysvar::slot_hashes::id()).unwrap();
            assert_same_account(&ctx.slot_hashes_account, &child_slot_hashes, "slot_hashes");

            let env = ctx.processing_environment();
            assert!(!child.is_alpenglow());
            assert!(!env.alpenglow_migration_succeeded);
            let (blockhash, lps) = child.last_blockhash_and_lamports_per_signature();
            assert_eq!(env.blockhash, blockhash);
            assert_eq!(env.blockhash_lamports_per_signature, lps);
            assert_eq!(env.epoch_total_stake, child.get_current_epoch_total_stake());
            assert_eq!(env.rent, child.rent_collector.rent);
            assert_eq!(
                env.program_runtime_environments.get_env_for_execution(),
                &child.transaction_processor.program_runtime_environment
            );
            assert_eq!(
                *ctx.next_durable_nonce(),
                child.blockhash_queue.read().unwrap().next_durable_nonce()
            );
            assert_eq!(ctx.max_processing_age, child.max_processing_age());
            assert_eq!(
                ctx.transaction_account_lock_limit,
                child.get_transaction_account_lock_limit()
            );

            // Every other sysvar the child's sysvar cache holds equals the parent's.
            #[allow(deprecated)]
            let others = [
                sysvar::rent::id(),
                sysvar::epoch_schedule::id(),
                sysvar::epoch_rewards::id(),
                sysvar::stake_history::id(),
                sysvar::last_restart_slot::id(),
                sysvar::fees::id(),
                sysvar::recent_blockhashes::id(),
            ];
            for key in others {
                assert!(ctx.sysvar_override(&key).is_none());
                assert_eq!(
                    parent.get_account(&key),
                    child.get_account(&key),
                    "sysvar {key}"
                );
            }
            // The child's sysvar cache holds exactly these values.
            let cache = child.transaction_processor.sysvar_cache();
            assert_eq!(
                cache.sysvar_id_to_buffer(&sysvar::clock::id()).as_deref(),
                Some(ctx.clock_account.data())
            );
            assert_eq!(
                cache.sysvar_id_to_buffer(&sysvar::slot_hashes::id()).as_deref(),
                Some(ctx.slot_hashes_account.data())
            );
        }
    }

    /// A child of an unfrozen parent P, built from FL's knowledge of P (P's own context over
    /// its frozen parent, P's last tick hash, P's lamports per signature and the vote
    /// accounts P wrote), equals the real child once agave finishes and freezes P.
    #[test_case(None; "no vote update in P")]
    #[test_case(Some(1_000_000); "vote timestamp far ahead updated in P")]
    #[test_case(Some(-3); "vote timestamp slightly behind updated in P")]
    fn test_chained_child_context_matches_new_from_parent(ts_offset_in_p: Option<i64>) {
        let leader = solana_pubkey::new_rand();
        let GenesisConfigInfo {
            genesis_config,
            voting_keypair,
            ..
        } = create_genesis_config_with_leader(5_000_000_000, &leader, 1_000_000_000);
        let (bank0, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        let mut grandparent = bank0;
        for slot in 1..=3 {
            grandparent = Bank::new_from_parent_with_bank_forks(
                &bank_forks,
                grandparent,
                SlotLeader::default(),
                slot,
            );
        }
        grandparent.freeze();
        // P = slot 4, not frozen; FL computes P's own context from its frozen parent.
        let p_ctx = Arc::new(grandparent.fast_lane_child_context(4).unwrap());
        let parent = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            Arc::clone(&grandparent),
            SlotLeader::default(),
            4,
        );
        let mut vote_accounts = Vec::new();
        if let Some(offset) = ts_offset_in_p {
            let vote_pubkey = voting_keypair.pubkey();
            update_vote_account_timestamp(
                BlockTimestamp {
                    slot: 4,
                    timestamp: parent.unix_timestamp_from_genesis().saturating_add(offset),
                },
                &parent,
                &vote_pubkey,
            );
            vote_accounts.push((vote_pubkey, parent.get_account(&vote_pubkey).unwrap()));
        }
        // P reaches its last tick (registers its blockhash), but is not frozen.
        let mut tick_hash = parent.last_blockhash();
        loop {
            tick_hash = solana_sha256_hasher::hashv(&[tick_hash.as_ref(), &[7]]);
            parent.register_tick_for_test(&tick_hash);
            if parent.last_blockhash() == tick_hash {
                break;
            }
        }
        let chained = FastLaneChainedParent {
            ctx: Arc::clone(&p_ctx),
            last_blockhash: tick_hash,
            lamports_per_signature: parent.fast_lane_lamports_per_signature(),
            vote_accounts,
        };
        for child_slot in [5, 7] {
            let ctx = grandparent
                .fast_lane_chained_child_context(&chained, child_slot)
                .unwrap();
            assert!(ctx.is_chained());
            parent.freeze();
            let child =
                Bank::new_from_parent(Arc::clone(&parent), SlotLeader::default(), child_slot);
            assert_same_account(
                &ctx.clock_account,
                &child.get_account(&sysvar::clock::id()).unwrap(),
                "clock",
            );
            #[allow(deprecated)]
            let rb_id = sysvar::recent_blockhashes::id();
            assert_same_account(
                ctx.sysvar_override(&rb_id).unwrap(),
                &child.get_account(&rb_id).unwrap(),
                "recent blockhashes",
            );
            let (_, resolved) = grandparent
                .fast_lane_resolve_slot_hashes(&ctx, parent.hash())
                .unwrap();
            assert_same_account(
                &resolved,
                &child.get_account(&sysvar::slot_hashes::id()).unwrap(),
                "resolved slot hashes",
            );
            let env = ctx.processing_environment();
            let (blockhash, lps) = child.last_blockhash_and_lamports_per_signature();
            assert_eq!(env.blockhash, blockhash);
            assert_eq!(env.blockhash_lamports_per_signature, lps);
            assert_eq!(
                *ctx.next_durable_nonce(),
                child.blockhash_queue.read().unwrap().next_durable_nonce()
            );
            // Age checks see the same queue.
            assert!(
                ctx.blockhash_queue
                    .get_hash_info_if_valid(&tick_hash, ctx.max_processing_age)
                    .is_some()
            );
        }
    }

    #[test]
    fn test_timestamp_estimate_for_slot_identity() {
        for timestamp in [None, Some((4, -5)), Some((3, 1_000))] {
            let (parent, _bank_forks) = frozen_parent(timestamp);
            let child =
                Bank::new_from_parent(Arc::clone(&parent), SlotLeader::default(), parent.slot() + 1);
            let drift = super::super::MaxAllowableDrift {
                fast: super::super::MAX_ALLOWABLE_DRIFT_PERCENTAGE_FAST,
                slow: super::super::MAX_ALLOWABLE_DRIFT_PERCENTAGE_SLOW_V2,
            };
            let start = Some((0, child.clock().epoch_start_timestamp));
            assert_eq!(
                child.get_timestamp_estimate(drift, start),
                child.get_timestamp_estimate_for_slot(child.slot(), drift, start),
            );
            // The parent computes the child's estimate identically.
            assert_eq!(
                parent.get_timestamp_estimate_for_slot(child.slot(), drift, start),
                child.get_timestamp_estimate_for_slot(child.slot(), drift, start),
            );
        }
    }

    #[test]
    fn test_support_refusals() {
        let (parent, _bank_forks) = frozen_parent(None);
        let unfrozen = Bank::new_from_parent(Arc::clone(&parent), SlotLeader::default(), 5);
        assert_eq!(
            unfrozen.fast_lane_support(6),
            Err(FastLaneUnsupported::ParentNotFrozen)
        );
        assert_eq!(
            parent.fast_lane_support(4),
            Err(FastLaneUnsupported::ChildNotAfterParent)
        );
        let schedule = parent.epoch_schedule().clone();
        let epoch = parent.epoch();
        let last_slot = schedule.get_last_slot_in_epoch(epoch);
        assert_eq!(
            parent.fast_lane_support(last_slot),
            Err(FastLaneUnsupported::LastSlotOfEpoch)
        );
        assert_eq!(
            parent.fast_lane_support(last_slot + 1),
            Err(FastLaneUnsupported::EpochBoundary)
        );
        assert_eq!(parent.fast_lane_support(5), Ok(()));
        // TowerBFT: fast leader handover (active in this genesis) does not apply, and
        // children of unfrozen children can be chained.
        assert!(parent.feature_set.snapshot().alpenglow_fast_leader_handover);
        assert_eq!(parent.fast_lane_support(8), Ok(()));
        assert!(parent.fast_lane_chaining_supported());
        // The execution Clock of a TowerBFT bank is its frozen Clock.
        assert_eq!(
            parent.fast_lane_execution_clock_account(),
            parent.get_account(&sysvar::clock::id())
        );
    }

    #[test]
    fn test_program_setup_matches_bank() {
        let (parent, _bank_forks) = frozen_parent(None);
        let setup = parent.fast_lane_program_setup().unwrap();
        let ids = parent
            .transaction_processor
            .builtin_program_ids
            .read()
            .unwrap()
            .clone();
        assert_eq!(setup.builtins.len(), ids.len());
        for (id, _entry) in &setup.builtins {
            assert!(ids.contains(id));
        }
        assert_eq!(
            setup.execution_cost,
            parent.transaction_processor.execution_cost()
        );
    }

    const VALIDATOR_STAKE: u64 = 1_000_000_000;

    /// An Alpenglow genesis (every feature, a genesis certificate), optionally without fast
    /// leader handover or without the certificate (the alpenglow feature active but the
    /// banks not yet Alpenglow, as during the migration).
    fn alpenglow_genesis(
        handover: bool,
        certificate: bool,
    ) -> (
        solana_genesis_config::GenesisConfig,
        solana_keypair::Keypair,
    ) {
        let leader = solana_pubkey::new_rand();
        let GenesisConfigInfo {
            mut genesis_config,
            voting_keypair,
            ..
        } = create_genesis_config_with_leader(5_000_000_000, &leader, VALIDATOR_STAKE);
        activate_all_features_alpenglow(&mut genesis_config);
        if !handover {
            deactivate_features(
                &mut genesis_config,
                &vec![agave_feature_set::alpenglow_fast_leader_handover::id()],
            );
        }
        if !certificate {
            genesis_config.accounts.remove(&GENESIS_CERTIFICATE_ACCOUNT);
        }
        (genesis_config, voting_keypair)
    }

    /// A frozen Alpenglow parent at `parent_slot`. Its block footer set its Clock to
    /// `footer_offset` seconds from the genesis-derived time (plus a sub-second remainder),
    /// and its vote account carries a timestamp far ahead, so a TowerBFT estimate for its
    /// children differs from the Alpenglow Clock.
    fn frozen_alpenglow_parent(
        parent_slot: Slot,
        footer_offset: i64,
        handover: bool,
    ) -> (Arc<Bank>, Arc<RwLock<BankForks>>) {
        let (genesis_config, voting_keypair) = alpenglow_genesis(handover, true);
        let (bank0, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        assert!(bank0.is_alpenglow());
        let mut parent = bank0;
        for slot in 1..=parent_slot {
            parent = Bank::new_from_parent_with_bank_forks(
                &bank_forks,
                parent,
                SlotLeader::default(),
                slot,
            );
        }
        assert!(parent.is_alpenglow());
        update_vote_account_timestamp(
            BlockTimestamp {
                slot: parent_slot,
                timestamp: parent
                    .unix_timestamp_from_genesis()
                    .saturating_add(1_000_000),
            },
            &parent,
            &voting_keypair.pubkey(),
        );
        let footer_s = parent
            .unix_timestamp_from_genesis()
            .saturating_add(footer_offset);
        parent.update_clock_from_footer(
            footer_s
                .saturating_mul(1_000_000_000)
                .saturating_add(123_456_789),
        );
        assert_eq!(parent.clock().unix_timestamp, footer_s);
        parent.freeze();
        (parent, bank_forks)
    }

    /// For an Alpenglow parent the child's Clock is the parent's footer Clock with the
    /// child's slot fields (no vote-timestamp estimate), and the child executes with
    /// `alpenglow_migration_succeeded`: FL's context equals `Bank::new_from_parent`'s.
    #[test_case(5, -100; "footer behind the PoH estimate")]
    #[test_case(5, 0; "footer at the PoH estimate")]
    #[test_case(9, 1_000; "footer far ahead")]
    fn test_alpenglow_child_context_matches_new_from_parent(parent_slot: Slot, footer_offset: i64) {
        let (parent, _bank_forks) = frozen_alpenglow_parent(parent_slot, footer_offset, true);
        let parent_clock = parent.clock();
        // Children in the parent's epoch, none the first slot of a leader window.
        let children: Vec<Slot> = (parent_slot + 1..parent_slot + 6)
            .filter(|slot| leader_slot_index(*slot) != 0)
            .collect();
        assert!(children.len() >= 3);
        for child_slot in children {
            let ctx = parent.fast_lane_child_context(child_slot).unwrap();
            let child =
                Bank::new_from_parent(Arc::clone(&parent), SlotLeader::default(), child_slot);
            assert!(child.is_alpenglow());
            let child_clock = child.get_account(&sysvar::clock::id()).unwrap();
            assert_same_account(&ctx.clock_account, &child_clock, "clock");
            assert_eq!(ctx.clock, child.clock());
            assert_eq!(ctx.clock.slot, child_slot);
            assert_eq!(ctx.clock.unix_timestamp, parent_clock.unix_timestamp);
            assert_eq!(
                ctx.clock.epoch_start_timestamp,
                parent_clock.epoch_start_timestamp
            );
            // The TowerBFT estimate (what FL used to compute) would differ for most of
            // these parents: the test exercises the Alpenglow branch, not a coincidence.
            if footer_offset < 0 {
                let drift = super::super::MaxAllowableDrift {
                    fast: super::super::MAX_ALLOWABLE_DRIFT_PERCENTAGE_FAST,
                    slow: super::super::MAX_ALLOWABLE_DRIFT_PERCENTAGE_SLOW_V2,
                };
                let start = Some((
                    parent
                        .epoch_schedule()
                        .get_first_slot_in_epoch(parent.epoch()),
                    parent_clock.epoch_start_timestamp,
                ));
                let tower = parent
                    .get_timestamp_estimate_for_slot(child_slot, drift, start)
                    .map(|estimate| estimate.max(parent_clock.unix_timestamp))
                    .unwrap_or(parent_clock.unix_timestamp);
                assert_ne!(tower, ctx.clock.unix_timestamp, "child {child_slot}");
            }
            let child_slot_hashes = child.get_account(&sysvar::slot_hashes::id()).unwrap();
            assert_same_account(&ctx.slot_hashes_account, &child_slot_hashes, "slot_hashes");

            let env = ctx.processing_environment();
            assert!(env.alpenglow_migration_succeeded);
            let (blockhash, lps) = child.last_blockhash_and_lamports_per_signature();
            assert_eq!(env.blockhash, blockhash);
            assert_eq!(env.blockhash_lamports_per_signature, lps);
            assert_eq!(env.epoch_total_stake, child.get_current_epoch_total_stake());
            assert_eq!(env.rent, child.rent_collector.rent);
            assert_eq!(
                env.program_runtime_environments.get_env_for_execution(),
                &child.transaction_processor.program_runtime_environment
            );
            assert_eq!(
                *ctx.next_durable_nonce(),
                child.blockhash_queue.read().unwrap().next_durable_nonce()
            );
            assert_eq!(ctx.max_processing_age, child.max_processing_age());

            // Every other sysvar the child's sysvar cache holds equals the parent's.
            #[allow(deprecated)]
            let others = [
                sysvar::rent::id(),
                sysvar::epoch_schedule::id(),
                sysvar::epoch_rewards::id(),
                sysvar::stake_history::id(),
                sysvar::last_restart_slot::id(),
                sysvar::fees::id(),
                sysvar::recent_blockhashes::id(),
            ];
            for key in others {
                assert!(ctx.sysvar_override(&key).is_none());
                assert_eq!(
                    parent.get_account(&key),
                    child.get_account(&key),
                    "sysvar {key}"
                );
            }
            let cache = child.transaction_processor.sysvar_cache();
            assert_eq!(
                cache.sysvar_id_to_buffer(&sysvar::clock::id()).as_deref(),
                Some(ctx.clock_account.data())
            );
            assert_eq!(
                cache
                    .sysvar_id_to_buffer(&sysvar::slot_hashes::id())
                    .as_deref(),
                Some(ctx.slot_hashes_account.data())
            );
            drop(cache);

            // The child's own footer rewrites its Clock after its last transaction; the
            // comparator's execution Clock still rebuilds the value its transactions read.
            assert_eq!(
                child.fast_lane_execution_clock_account().as_ref(),
                Some(&child_clock)
            );
            let footer_s = parent_clock.unix_timestamp.saturating_add(7);
            child.update_clock_from_footer(footer_s.saturating_mul(1_000_000_000));
            child.freeze();
            assert_eq!(child.clock().unix_timestamp, footer_s);
            assert_ne!(
                child.get_account(&sysvar::clock::id()).as_ref(),
                Some(&child_clock)
            );
            let execution_clock = child.fast_lane_execution_clock_account().unwrap();
            assert_same_account(&execution_clock, &ctx.clock_account, "execution clock");
        }
    }

    #[test]
    fn test_alpenglow_support_refusals() {
        // Fast leader handover active: no first slot of a leader window.
        let (parent, _bank_forks) = frozen_alpenglow_parent(5, 0, true);
        assert_eq!(parent.fast_lane_support(6), Ok(()));
        assert_eq!(parent.fast_lane_support(7), Ok(()));
        assert_eq!(
            parent.fast_lane_support(8),
            Err(FastLaneUnsupported::FastLeaderHandover)
        );
        assert_eq!(parent.fast_lane_support(9), Ok(()));
        assert!(matches!(
            parent.fast_lane_child_context(8),
            Err(FastLaneUnsupported::FastLeaderHandover)
        ));
        let schedule = parent.epoch_schedule().clone();
        let last_slot = schedule.get_last_slot_in_epoch(parent.epoch());
        assert_eq!(
            parent.fast_lane_support(last_slot),
            Err(FastLaneUnsupported::LastSlotOfEpoch)
        );
        assert_eq!(
            parent.fast_lane_support(last_slot + 1),
            Err(FastLaneUnsupported::EpochBoundary)
        );
        assert!(!parent.fast_lane_chaining_supported());

        // Without fast leader handover every slot of the window is supported.
        let (parent, _bank_forks) = frozen_alpenglow_parent(5, 0, false);
        assert!(!parent.feature_set.snapshot().alpenglow_fast_leader_handover);
        for child_slot in 6..=9 {
            assert_eq!(parent.fast_lane_support(child_slot), Ok(()), "{child_slot}");
        }

        // The alpenglow feature active but the bank not an Alpenglow bank (migration in
        // flight, vote-only banks, the first Alpenglow block's parent): refused.
        let (genesis_config, _) = alpenglow_genesis(true, false);
        let (bank0, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        assert!(!bank0.is_alpenglow());
        assert!(bank0.feature_set.snapshot().alpenglow);
        let parent =
            Bank::new_from_parent_with_bank_forks(&bank_forks, bank0, SlotLeader::default(), 1);
        parent.freeze();
        assert_eq!(
            parent.fast_lane_support(2),
            Err(FastLaneUnsupported::AlpenglowMigration)
        );
        assert!(!parent.fast_lane_chaining_supported());
    }

    /// A child of an unfrozen Alpenglow parent cannot be chained (its Clock is the parent's
    /// footer time): it must wait for the parent's freeze.
    #[test]
    fn test_alpenglow_chained_child_context_refused() {
        let (grandparent, bank_forks) = frozen_alpenglow_parent(5, 0, true);
        let p_ctx = Arc::new(grandparent.fast_lane_child_context(6).unwrap());
        let parent = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            Arc::clone(&grandparent),
            SlotLeader::default(),
            6,
        );
        let chained = FastLaneChainedParent {
            ctx: p_ctx,
            last_blockhash: Hash::new_unique(),
            lamports_per_signature: parent.fast_lane_lamports_per_signature(),
            vote_accounts: Vec::new(),
        };
        assert!(matches!(
            grandparent.fast_lane_chained_child_context(&chained, 7),
            Err(FastLaneUnsupported::ChainedAlpenglow)
        ));
        // Once agave froze the parent, the same child runs over it.
        parent.freeze();
        let ctx = parent.fast_lane_child_context(7).unwrap();
        let child = Bank::new_from_parent(Arc::clone(&parent), SlotLeader::default(), 7);
        assert_same_account(
            &ctx.clock_account,
            &child.get_account(&sysvar::clock::id()).unwrap(),
            "clock",
        );
        assert!(ctx.processing_environment().alpenglow_migration_succeeded);
    }

    #[test]
    fn test_alpenglow_footer_accounts() {
        let [clock, nanosecond_clock] = Bank::fast_lane_alpenglow_footer_accounts();
        assert_eq!(clock, sysvar::clock::id());
        assert_eq!(nanosecond_clock, *super::super::NANOSECOND_CLOCK_ACCOUNT);
        // The footer writes exactly these two besides vote accounts.
        let (parent, _bank_forks) = frozen_alpenglow_parent(5, 0, true);
        let child = Bank::new_from_parent(Arc::clone(&parent), SlotLeader::default(), 6);
        let before: Vec<_> = [clock, nanosecond_clock]
            .iter()
            .map(|key| child.get_account(key))
            .collect();
        child.update_clock_from_footer(
            parent
                .clock()
                .unix_timestamp
                .saturating_add(3)
                .saturating_mul(1_000_000_000),
        );
        for (key, before) in [clock, nanosecond_clock].iter().zip(before) {
            assert_ne!(child.get_account(key), before, "{key}");
        }
        assert!(child.get_nanosecond_clock().is_some());
    }
}
