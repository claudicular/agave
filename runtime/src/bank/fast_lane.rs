//! Read-only accessors on a **frozen parent bank** for the speculative fast lane
//! (`agave-fast-lane`).
//!
//! The fast lane executes the transactions of a child slot N over the frozen parent P and
//! a private multi-version overlay, without ever creating a `Bank` for N (a throwaway child
//! bank would write sysvars into the shared, slot-keyed accounts write cache and purge that
//! slot on drop). Everything the child's execution environment needs is computed here as a
//! pure function of P and N, mirroring `_new_from_parent` / `prepare_for_block_execution` /
//! `load_and_execute_transactions` for the supported case: P frozen, N in P's epoch, N not
//! the last slot of the epoch, no reward distribution, not Alpenglow, and no hard-fork
//! restart slot change. Every other child is refused with [`FastLaneUnsupported`].
//!
//! Nothing in this module mutates any bank, accounts-db or program-cache state, and account
//! reads use `LoadHint::Unspecified` (the fast lane runs off the replay thread, so the root
//! may move during a load).

use {
    super::{Bank, EpochRewardStatus},
    agave_feature_set::FeatureSet,
    crate::sysvar_account::{create_account, from_account},
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
    std::{collections::HashSet, sync::Arc},
};

/// Why the fast lane cannot execute a given child of this bank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastLaneUnsupported {
    /// The parent bank has not been frozen yet.
    ParentNotFrozen,
    /// The child slot is not greater than the parent slot.
    ChildNotAfterParent,
    /// Alpenglow is active on the parent (footer clock, markers, migration): paused.
    Alpenglow,
    /// The alpenglow feature is active but the parent is not yet an alpenglow bank
    /// (migration blocks may be in flight): paused.
    AlpenglowMigration,
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
    /// The child's SlotHashes sysvar value and account.
    pub slot_hashes: Arc<SlotHashes>,
    pub slot_hashes_account: AccountSharedData,
    /// The parent's blockhash queue (the child's until its last tick).
    blockhash_queue: BlockhashQueue,
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
}

impl FastLaneChildContext {
    /// The `TransactionProcessingEnvironment` the child bank would pass to the SVM.
    pub fn processing_environment(&self) -> TransactionProcessingEnvironment {
        TransactionProcessingEnvironment {
            blockhash: self.blockhash,
            blockhash_lamports_per_signature: self.blockhash_lamports_per_signature,
            // `fast_lane_support` refuses alpenglow parents.
            alpenglow_migration_succeeded: false,
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
            None
        }
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

/// An `AddressLoader` resolving a child slot's lookups against the frozen parent's state,
/// with `current_slot = child` and the child's SlotHashes (see
/// `Accounts::lookup_table_addresses_into`). Table changes made inside the child slot can
/// never affect a lookup in that slot (extensions are hidden until the next slot, a
/// deactivating table stays usable, a closable table is already unusable, a new table is
/// empty), so this equals the child bank's own resolution at any point of its execution.
#[derive(Clone, Copy)]
pub struct FastLaneAddressLoader<'a> {
    parent: &'a Bank,
    ctx: &'a FastLaneChildContext,
}

impl AddressLoader for FastLaneAddressLoader<'_> {
    fn load_addresses(
        self,
        lookups: &[MessageAddressTableLookup],
    ) -> Result<LoadedAddresses, AddressLoaderError> {
        self.parent
            .fast_lane_load_addresses(
                self.ctx,
                lookups.iter().map(SVMMessageAddressTableLookup::from),
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
        if self.is_alpenglow() {
            return Err(FastLaneUnsupported::Alpenglow);
        }
        if self.feature_set.snapshot().alpenglow {
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

    /// The child's Clock, exactly as `update_clock(Some(parent_epoch))` computes it in a
    /// same-epoch TowerBFT child created from this frozen bank.
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
        let first_slot_in_epoch = epoch_schedule.get_first_slot_in_epoch(parent_epoch);
        let epoch_start_timestamp = Some((first_slot_in_epoch, parent_clock.epoch_start_timestamp));
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
        // Same epoch as the parent (checked by `fast_lane_support`): no epoch start update.
        let clock = Clock {
            slot: child_slot,
            epoch_start_timestamp: parent_clock.epoch_start_timestamp,
            epoch: epoch_schedule.get_epoch(child_slot),
            leader_schedule_epoch: epoch_schedule.get_leader_schedule_epoch(child_slot),
            unix_timestamp,
        };
        let mut account = create_account(
            &clock,
            self.inherit_specially_retained_account_fields(&old_account),
        );
        self.adjust_sysvar_balance_for_rent(&mut account);
        Ok((clock, account))
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
        })
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
        let mut deactivation_slot = u64::MAX;
        let mut loaded_addresses = LoadedAddresses::default();
        for address_table_lookup in address_table_lookups {
            let table_account = self
                .get_account_modified_slot(address_table_lookup.account_key)
                .map(|(account, _slot)| account)
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
        let enable_tx_v1 = ctx.feature_set.snapshot().enable_tx_v1;
        if !enable_tx_v1 && tx.version() == TransactionVersion::Number(1) {
            return Err(TransactionError::UnsupportedVersion);
        }
        let max_transaction_size = match tx.version() {
            TransactionVersion::Number(1) if enable_tx_v1 => {
                solana_message::v1::MAX_TRANSACTION_SIZE
            }
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
            FastLaneAddressLoader { parent: self, ctx },
            self.get_reserved_account_keys(),
        )
    }

    /// The state-independent part of `check_transactions` for the child slot: the v1
    /// filter, the compute budget and fee, and the blockhash-age check against the parent's
    /// blockhash queue. The status-cache check is skipped: a valid block never contains an
    /// already-processed transaction, and replay marks an invalid one dead.
    pub fn fast_lane_check_static(
        &self,
        ctx: &FastLaneChildContext,
        tx: &impl TransactionWithMeta,
    ) -> FastLaneStaticCheck {
        let feature_snapshot = ctx.feature_set.snapshot();
        if !feature_snapshot.enable_tx_v1 && tx.version() == TransactionVersion::Number(1) {
            return FastLaneStaticCheck::Err(TransactionError::UnsupportedVersion);
        }
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
        match Self::check_nonce_transaction_validity_with(
            tx,
            &ctx.next_durable_nonce,
            false, // strict_nonce_size_check: never in replay
            false, // strict_nonce_authority_check: never in replay
            load_nonce_account,
        ) {
            Some((nonce_address, _)) => Ok(CheckedTransactionDetails::new(
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
            let activation_slot = prototype
                .enable_feature_id
                .and_then(|feature_id| self.feature_set.activated_slot(&feature_id))
                .unwrap_or(0);
            builtins.push((
                program_id,
                ProgramCacheEntry::new_builtin(activation_slot, prototype.register_fn),
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
            .filter(|(_key, _slot, entry)| {
                entry.deployment_slot <= root
                    && entry
                        .program
                        .get_environment()
                        .map(|env| env == environment)
                        .unwrap_or(false)
            })
            .map(|(key, _slot, entry)| (key, entry))
            .collect()
    }

    /// The execution program-runtime environment Arc of this bank's processor.
    pub fn fast_lane_execution_environment(&self) -> ProgramRuntimeEnvironment {
        self.transaction_processor
            .program_runtime_environment
            .clone()
    }

    /// The fee collector (slot leader) this bank deposits fees to at freeze.
    pub fn fast_lane_collector_id(&self) -> Pubkey {
        *self.leader_id()
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
            genesis_utils::{GenesisConfigInfo, create_genesis_config_with_leader},
        },
        solana_signer::Signer,
        solana_vote_program::vote_state::BlockTimestamp,
        std::sync::Arc,
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
}
