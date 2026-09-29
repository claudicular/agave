//! End-to-end test of the fast lane's safety net (`agave_fast_lane::cluster_check`) through
//! replay stage's own code: a wrong account write committed into slot 1's bank (what a wrong
//! fast-lane result would do under "execute once") gives slots 1 and 2 wrong bank hashes; the
//! cluster's duplicate confirmation of the correct hashes is detected, the fast lane is
//! poisoned, `dump_then_repair_correct_slots` dumps both slots, the same shreds are repaired, and
//! the re-replay (fast lane off, so no wrong write) reproduces the cluster's hashes and state.

use {
    super::*,
    crate::consensus::heaviest_subtree_fork_choice::HeaviestSubtreeForkChoice,
    agave_fast_lane::{
        cluster_check::{self, EventKind},
        control,
    },
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_entry::entry::{self, Entry},
    solana_ledger::{
        genesis_utils::{GenesisConfigInfo, create_genesis_config},
        shred::{ProcessShredsStats, ReedSolomonCache, Shred, Shredder},
    },
    solana_sha256_hasher::hash,
    solana_system_transaction as system_transaction,
    solana_unified_scheduler_pool::DefaultSchedulerPool,
    tempfile::TempDir,
};

static TEST_PUBKEY: Pubkey = Pubkey::new_from_array([11; 32]);

/// A full slot: one transaction entry, then the ticks to the end of `slot` from `parent`.
fn slot_entries(
    genesis_config: &solana_genesis_config::GenesisConfig,
    start_hash: Hash,
    slot: Slot,
    parent: Slot,
    transactions: Vec<Transaction>,
) -> Vec<Entry> {
    let hashes_per_tick = genesis_config.poh_config.hashes_per_tick.unwrap_or(1);
    let num_ticks = (slot - parent) * genesis_config.ticks_per_slot;
    let tx_entry = entry::next_entry(
        &start_hash,
        hashes_per_tick.saturating_sub(1).max(1),
        transactions,
    );
    let first_tick = entry::next_entry(&tx_entry.hash, 1, vec![]);
    let prev_hash = first_tick.hash;
    let mut entries = vec![tx_entry, first_tick];
    entries.extend(entry::create_ticks(
        num_ticks - 1,
        hashes_per_tick,
        prev_hash,
    ));
    entries
}

fn slot_shreds(
    keypair: &Keypair,
    slot: Slot,
    parent: Slot,
    entries: &[Entry],
    chained_merkle_root: Hash,
) -> Vec<Shred> {
    Shredder::new(slot, parent, 0, 0)
        .unwrap()
        .make_merkle_shreds_from_entries(
            keypair,
            entries,
            true,
            chained_merkle_root,
            0,
            0,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        )
        .collect()
}

/// One node's replay state: bank forks, blockstore and the TowerBFT structures that the
/// cluster-agreement checks run on.
struct Node {
    bank_forks: Arc<RwLock<BankForks>>,
    blockstore: Arc<Blockstore>,
    _ledger_path: TempDir,
    leader_schedule_cache: LeaderScheduleCache,
    progress: ProgressMap,
    ctx: ProcessActiveBanksContext,
    tbft: TowerBFTStructures,
    duplicate_slots_to_repair: DuplicateSlotsToRepair,
    purge_repair_slot_counter: PurgeRepairSlotCounter,
    finalization_cert_sender: Sender<SmallVec<[Certificate; 2]>>,
    _replay_vote_receiver: Receiver<ReplayVoteMessage>,
}

impl Node {
    fn new(genesis_config: &solana_genesis_config::GenesisConfig) -> Self {
        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(genesis_config);
        // Deterministic ticks so that two nodes produce identical bank hashes.
        let mut tick_hash = Hash::default();
        while bank0.tick_height() < bank0.max_tick_height() {
            tick_hash = hash(tick_hash.as_ref());
            bank0.register_tick_for_test(&tick_hash);
        }
        bank0.freeze();
        bank_forks.write().unwrap().install_scheduler_pool(
            DefaultSchedulerPool::new_for_verification(None, None, None, None, None),
        );
        let ledger_path = tempfile::tempdir().unwrap();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let leader_schedule_cache = LeaderScheduleCache::new_from_bank(&bank0);
        let mut progress = ProgressMap::default();
        progress.insert(
            0,
            ForkProgress::new(bank0.last_blockhash(), None, None, 0, 0, None),
        );
        let (replay_vote_sender, replay_vote_receiver) = crossbeam_channel::unbounded();
        let (cluster_slots_update_sender, _) = crossbeam_channel::bounded(1024);
        let (cost_update_sender, _) = crossbeam_channel::bounded(1024);
        let (ancestor_hashes_replay_update_sender, _) = crossbeam_channel::bounded(1024);
        let (votor_event_sender, _) = crossbeam_channel::bounded(1024);
        let ctx = ProcessActiveBanksContext {
            bank_forks: bank_forks.clone(),
            blockstore: blockstore.clone(),
            transaction_status_sender: None,
            entry_notification_sender: None,
            replay_vote_sender,
            bank_notification_sender: None,
            rpc_subscriptions: None,
            slot_status_notifier: None,
            cluster_slots_update_sender,
            cost_update_sender,
            ancestor_hashes_replay_update_sender,
            block_metadata_notifier: None,
            votor_event_sender,
            replay_mode: ForkReplayMode::Serial,
            replay_verification_worker_pool: ReplayVerificationWorkerPool::new(1),
            migration_status: Arc::new(MigrationStatus::default()),
            remember_chained_block_id_pass: false,
        };
        let tbft = TowerBFTStructures {
            heaviest_subtree_fork_choice: HeaviestSubtreeForkChoice::new_from_bank_forks(
                bank_forks.clone(),
            ),
            duplicate_slots_tracker: DuplicateSlotsTracker::default(),
            duplicate_confirmed_slots: DuplicateConfirmedSlots::default(),
            unfrozen_gossip_verified_vote_hashes: UnfrozenGossipVerifiedVoteHashes::default(),
            epoch_slots_frozen_slots: EpochSlotsFrozenSlots::default(),
        };
        let (finalization_cert_sender, _) = crossbeam_channel::unbounded();
        Self {
            bank_forks,
            blockstore,
            _ledger_path: ledger_path,
            leader_schedule_cache,
            progress,
            ctx,
            tbft,
            duplicate_slots_to_repair: DuplicateSlotsToRepair::default(),
            purge_repair_slot_counter: PurgeRepairSlotCounter::default(),
            finalization_cert_sender,
            _replay_vote_receiver: replay_vote_receiver,
        }
    }

    /// Replay-loop steps (stock order: create children of frozen banks, then replay) until
    /// nothing changes. `commit_fl` runs on every newly created bank before it replays: the
    /// stand-in for the execute-once commit of fast-lane results.
    fn replay_until_idle(&mut self, commit_fl: &dyn Fn(&Bank)) -> Vec<Slot> {
        let mut frozen = vec![];
        for _ in 0..16 {
            let before: HashSet<Slot> = self
                .bank_forks
                .read()
                .unwrap()
                .banks()
                .keys()
                .copied()
                .collect();
            let mut replay_timing = ReplayLoopTiming::default();
            ReplayStage::generate_new_bank_forks(
                NewBankForksContext {
                    blockstore: &self.blockstore,
                    bank_forks: &self.bank_forks,
                    leader_schedule_cache: &self.leader_schedule_cache,
                    rpc_subscriptions: None,
                    slot_status_notifier: &None,
                    migration_status: &self.ctx.migration_status,
                    my_pubkey: &TEST_PUBKEY,
                },
                &mut self.progress,
                &mut replay_timing,
            );
            let created: Vec<Arc<Bank>> = {
                let bank_forks = self.bank_forks.read().unwrap();
                bank_forks
                    .banks()
                    .iter()
                    .filter(|(slot, _)| !before.contains(slot))
                    .map(|(_, bank)| bank.clone_without_scheduler())
                    .collect()
            };
            for bank in &created {
                commit_fl(bank);
            }
            let newly_frozen = ReplayStage::process_active_banks(
                0,
                &self.ctx,
                &mut self.progress,
                &mut vec![],
                &mut LatestValidatorVotesForFrozenBanks::default(),
                &mut self.duplicate_slots_to_repair,
                &mut self.purge_repair_slot_counter,
                Some(&mut self.tbft),
                &TEST_PUBKEY,
                &TEST_PUBKEY,
                &mut replay_timing,
                &self.finalization_cert_sender,
            );
            if created.is_empty() && newly_frozen.is_empty() {
                return frozen;
            }
            frozen.extend(newly_frozen);
        }
        panic!("replay did not settle");
    }

    /// The cluster duplicate-confirms `slot` with `hash` (what the vote listener sends).
    fn cluster_confirms(&mut self, slot: Slot, hash: Hash) {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        sender.send(vec![(slot, hash)]).unwrap();
        ReplayStage::process_duplicate_confirmed_slots(
            &receiver,
            &self.blockstore,
            &mut self.tbft.duplicate_slots_tracker,
            &mut self.tbft.duplicate_confirmed_slots,
            &mut self.tbft.epoch_slots_frozen_slots,
            &self.bank_forks,
            &self.progress,
            &mut self.tbft.heaviest_subtree_fork_choice,
            &mut self.duplicate_slots_to_repair,
            &self.ctx.ancestor_hashes_replay_update_sender,
            &mut self.purge_repair_slot_counter,
        );
    }

    fn dump_then_repair(&mut self) -> Vec<(Slot, Hash)> {
        let (mut ancestors, mut descendants) = {
            let bank_forks = self.bank_forks.read().unwrap();
            (bank_forks.ancestors(), bank_forks.descendants())
        };
        let (dumped_slots_sender, dumped_slots_receiver) = crossbeam_channel::unbounded();
        ReplayStage::dump_then_repair_correct_slots(
            &mut self.duplicate_slots_to_repair,
            &mut ancestors,
            &mut descendants,
            &mut self.progress,
            &self.bank_forks,
            &self.blockstore,
            None,
            &mut self.purge_repair_slot_counter,
            &dumped_slots_sender,
            &TEST_PUBKEY,
            &self.leader_schedule_cache,
        );
        dumped_slots_receiver.try_iter().flatten().collect()
    }

    fn hash(&self, slot: Slot) -> Option<Hash> {
        self.bank_forks
            .read()
            .unwrap()
            .get(slot)
            .filter(|bank| bank.is_frozen())
            .map(|bank| bank.hash())
    }

    fn account(&self, slot: Slot, pubkey: &Pubkey) -> Option<AccountSharedData> {
        self.bank_forks
            .read()
            .unwrap()
            .get(slot)
            .unwrap()
            .get_account(pubkey)
    }
}

#[test]
fn test_fast_lane_cluster_mismatch_poisons_dumps_and_replays() {
    agave_logger::setup();
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(100 * solana_native_token::LAMPORTS_PER_SOL);
    let genesis_hash = genesis_config.hash();
    let payers: Vec<Keypair> = (0..3u8)
        .map(|i| Keypair::new_from_array([i + 21; 32]))
        .collect();
    let transfer = |from: &Keypair, to: &Pubkey, lamports| {
        system_transaction::transfer(from, to, lamports, genesis_hash)
    };
    // 0 <- 1 <- 2; slot 2 spends what slot 1 transferred.
    let slot1_txs: Vec<Transaction> = payers
        .iter()
        .map(|payer| {
            transfer(
                &mint_keypair,
                &payer.pubkey(),
                solana_native_token::LAMPORTS_PER_SOL,
            )
        })
        .collect();
    let slot2_txs = vec![transfer(
        &payers[0],
        &payers[1].pubkey(),
        solana_native_token::LAMPORTS_PER_SOL / 2,
    )];

    // The cluster: the same blocks replayed with no fast lane.
    let mut cluster = Node::new(&genesis_config);
    let shred_keypair = Keypair::new_from_array([7; 32]);
    let entries1 = slot_entries(
        &genesis_config,
        cluster
            .bank_forks
            .read()
            .unwrap()
            .get(0)
            .unwrap()
            .last_blockhash(),
        1,
        0,
        slot1_txs,
    );
    let entries2 = slot_entries(
        &genesis_config,
        entries1.last().unwrap().hash,
        2,
        1,
        slot2_txs,
    );
    let shreds1 = slot_shreds(&shred_keypair, 1, 0, &entries1, Hash::default());
    let shreds2 = slot_shreds(
        &shred_keypair,
        2,
        1,
        &entries2,
        shreds1.last().unwrap().merkle_root().unwrap(),
    );
    cluster
        .blockstore
        .insert_shreds(shreds1.clone(), false)
        .unwrap();
    cluster
        .blockstore
        .insert_shreds(shreds2.clone(), false)
        .unwrap();
    let mut frozen = cluster.replay_until_idle(&|_| {});
    frozen.sort_unstable();
    assert_eq!(frozen, vec![1, 2]);
    let cluster_hash1 = cluster.hash(1).unwrap();
    let cluster_hash2 = cluster.hash(2).unwrap();

    // Our node: the fast lane commits slot 1 with the mint's post-state one lamport too high.
    let mint = mint_keypair.pubkey();
    let fl_commit = |bank: &Bank| {
        if bank.slot() == 1 {
            let mut account = bank.get_account(&mint).unwrap();
            account.set_lamports(account.lamports() + 1);
            bank.store_account(&mint, &account);
        }
    };
    // If no other test in this process has poisoned the fast lane yet, it starts active and the
    // detection below must turn it off.
    let fl_started_active = control::set_active(true);
    let mut node = Node::new(&genesis_config);
    node.blockstore
        .insert_shreds(shreds1.clone(), false)
        .unwrap();
    node.blockstore
        .insert_shreds(shreds2.clone(), false)
        .unwrap();
    let mut frozen = node.replay_until_idle(&fl_commit);
    frozen.sort_unstable();
    assert_eq!(frozen, vec![1, 2]);
    let wrong_hash1 = node.hash(1).unwrap();
    let wrong_hash2 = node.hash(2).unwrap();
    assert_ne!(wrong_hash1, cluster_hash1);
    assert_ne!(
        wrong_hash2, cluster_hash2,
        "the error propagates to descendants"
    );
    assert_ne!(node.account(2, &mint), cluster.account(2, &mint));

    // Detection: the cluster duplicate-confirms its hashes (vote listener -> replay stage).
    node.cluster_confirms(1, cluster_hash1);
    assert!(control::is_poisoned());
    assert!(!control::is_active());
    assert!(
        !control::set_active(true),
        "the control file's enable must be refused"
    );
    assert!(control::poison_reason().is_some());
    if fl_started_active {
        assert!(
            control::poison_reason()
                .unwrap()
                .contains(&cluster_hash1.to_string())
        );
    }
    let events = cluster_check::recent_events();
    assert!(events.iter().any(|e| e.slot == 1
        && e.our_hash == Some(wrong_hash1)
        && e.cluster_hash == cluster_hash1
        && e.kind
            == EventKind::Mismatch {
                source: "duplicate_confirmed"
            }));
    node.cluster_confirms(2, cluster_hash2);
    assert_eq!(node.duplicate_slots_to_repair.get(&1), Some(&cluster_hash1));
    assert_eq!(node.duplicate_slots_to_repair.get(&2), Some(&cluster_hash2));
    assert!(
        !node
            .tbft
            .heaviest_subtree_fork_choice
            .is_candidate(&(1, wrong_hash1))
            .unwrap()
    );

    // Agave's dump: slots 1 and 2 leave bank forks and the blockstore; repair is notified.
    let mut dumped = node.dump_then_repair();
    dumped.sort_unstable();
    assert_eq!(dumped, vec![(1, cluster_hash1), (2, cluster_hash2)]);
    assert!(node.bank_forks.read().unwrap().get(1).is_none());
    assert!(node.bank_forks.read().unwrap().get(2).is_none());
    assert!(
        node.blockstore
            .meta(1)
            .unwrap()
            .is_none_or(|m| m.received == 0)
    );

    // Repair delivers the same block again; replay runs it with the fast lane off, so the
    // stand-in commit is skipped (as the execute-once path must when `is_active()` is false).
    node.blockstore.insert_shreds(shreds1, false).unwrap();
    node.blockstore.insert_shreds(shreds2, false).unwrap();
    let gated_fl_commit = |bank: &Bank| {
        if control::is_active() {
            fl_commit(bank)
        }
    };
    let mut frozen = node.replay_until_idle(&gated_fl_commit);
    frozen.sort_unstable();
    assert_eq!(frozen, vec![1, 2]);
    assert_eq!(node.hash(1), Some(cluster_hash1));
    assert_eq!(node.hash(2), Some(cluster_hash2));
    for pubkey in std::iter::once(mint).chain(payers.iter().map(|p| p.pubkey())) {
        assert_eq!(
            node.account(2, &pubkey),
            cluster.account(2, &pubkey),
            "{pubkey}"
        );
    }
    assert!(node.duplicate_slots_to_repair.is_empty());
    for (slot, hash) in [(1, cluster_hash1), (2, cluster_hash2)] {
        assert_eq!(
            node.tbft
                .heaviest_subtree_fork_choice
                .is_duplicate_confirmed(&(slot, hash)),
            Some(true)
        );
        assert!(
            cluster_check::recent_events().iter().any(|e| e.slot == slot
                && e.cluster_hash == hash
                && e.kind == EventKind::Match { recovered: true }),
            "slot {slot} recovered"
        );
    }
    // Still poisoned: nothing re-arms the fast lane before a restart.
    assert!(control::is_poisoned());
    assert!(!control::is_active());
}
