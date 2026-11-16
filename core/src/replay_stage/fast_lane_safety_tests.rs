//! End-to-end test of the fast lane's safety net (`agave_fast_lane::cluster_check`) through
//! replay stage's own code: a wrong account write committed into slot 1's bank (what a wrong
//! fast-lane result would do under "execute once") gives slots 1 and 2 wrong bank hashes; the
//! cluster's duplicate confirmation of the correct hashes is detected, the fast lane is
//! poisoned, `dump_then_repair_correct_slots` dumps both slots, the same shreds are repaired, and
//! the re-replay (fast lane off, so no wrong write) reproduces the cluster's hashes and state.
//!
//! Alpenglow (bottom of the file): none of those TowerBFT signals exists; the reference is the
//! leader's block-footer bank hash, checked at freeze. A mismatch on a bank the fast lane
//! committed into is replayed once without the fast lane instead of marking the slot dead, and
//! that replay's footer check attributes it.

use {
    super::*,
    crate::consensus::heaviest_subtree_fork_choice::HeaviestSubtreeForkChoice,
    agave_fast_lane::{
        cluster_check::{self, EventKind},
        control,
    },
    agave_votor_messages::{
        certificate::{CertSignature, GenesisCert},
        consensus_message::Block,
    },
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_bls_signatures::{BLS_SIGNATURE_AFFINE_SIZE, Signature as BLSSignature},
    solana_entry::{
        block_component::{BlockComponent, BlockFooterV1, BlockHeaderV1, VersionedBlockMarker},
        entry::{self, Entry},
    },
    solana_ledger::{
        genesis_utils::{GenesisConfigInfo, create_genesis_config},
        shred::{ProcessShredsStats, ReedSolomonCache, Shred, Shredder},
    },
    solana_runtime::fast_lane_commit,
    solana_sha256_hasher::hash,
    solana_system_transaction as system_transaction,
    solana_unified_scheduler_pool::DefaultSchedulerPool,
    tempfile::TempDir,
};

static TEST_PUBKEY: Pubkey = Pubkey::new_from_array([11; 32]);

/// The fast lane's poison and commit mode are process-global: these tests run one at a time.
static FL_E2E: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    let _serial = FL_E2E.lock().unwrap_or_else(|e| e.into_inner());
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
    // Agave's side of the execute-once gate (`solana_runtime::fast_lane_commit::mode`, which the
    // commit path consults) follows the fast lane's commit mode while the fast lane is active.
    control::set_commit_mode(control::COMMIT_SHADOW);
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
    assert_eq!(fast_lane_commit::mode(), fast_lane_commit::MODE_OFF);
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
    // stand-in commit is skipped (as the execute-once path must when agave's side of the gate,
    // `fast_lane_commit::mode()`, is off).
    node.blockstore.insert_shreds(shreds1, false).unwrap();
    node.blockstore.insert_shreds(shreds2, false).unwrap();
    let gated_fl_commit = |bank: &Bank| {
        if control::is_active() || fast_lane_commit::mode() != fast_lane_commit::MODE_OFF {
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
    control::set_commit_mode(control::COMMIT_SHADOW);
    assert_eq!(fast_lane_commit::mode(), fast_lane_commit::MODE_OFF);
}

/// The same scenario through the real execute-once commit path: with `commit = on`, slot 1's
/// first transaction is committed by `commit_external` with a wrong fast-lane result (the
/// mint's post-state one lamport too high), agave's replay follows (verifies the committed
/// transaction, executes the others). The cluster's confirmation disables FL provisionally;
/// the dump and the re-replay of the same block without FL give the cluster's hashes, so the
/// disagreement is attributed to FL and the poison becomes sticky.
#[test]
#[ignore = "needs the process-global fast lane active for its whole run, which other core tests \
            poison through the cluster-check hooks: run alone with \
            `cargo test -p solana-core --lib -- --ignored fast_lane_real_commit`"]
fn test_fast_lane_real_commit_wrong_result_is_attributed_to_fl() {
    use {
        solana_runtime::transaction_execution::{CommitServices, ExternalCommit, commit_external},
        solana_svm::{
            transaction_processing_result::ProcessedTransaction,
            transaction_processor::TransactionProcessingConfig,
        },
        solana_svm_timings::ExecuteTimings,
    };
    let _serial = FL_E2E.lock().unwrap_or_else(|e| e.into_inner());
    agave_logger::setup();
    control::unpoison_for_tests();
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(100 * solana_native_token::LAMPORTS_PER_SOL);
    let genesis_hash = genesis_config.hash();
    let payers: Vec<Keypair> = (0..3u8)
        .map(|i| Keypair::new_from_array([i + 41; 32]))
        .collect();
    let transfer = |from: &Keypair, to: &Pubkey, lamports| {
        system_transaction::transfer(from, to, lamports, genesis_hash)
    };
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
    let mut cluster = Node::new(&genesis_config);
    let shred_keypair = Keypair::new_from_array([9; 32]);
    let entries1 = slot_entries(
        &genesis_config,
        cluster.bank_forks.read().unwrap().get(0).unwrap().last_blockhash(),
        1,
        0,
        slot1_txs.clone(),
    );
    let entries2 = slot_entries(&genesis_config, entries1.last().unwrap().hash, 2, 1, slot2_txs);
    let shreds1 = slot_shreds(&shred_keypair, 1, 0, &entries1, Hash::default());
    let shreds2 = slot_shreds(
        &shred_keypair,
        2,
        1,
        &entries2,
        shreds1.last().unwrap().merkle_root().unwrap(),
    );
    cluster.blockstore.insert_shreds(shreds1.clone(), false).unwrap();
    cluster.blockstore.insert_shreds(shreds2.clone(), false).unwrap();
    let mut frozen = cluster.replay_until_idle(&|_| {});
    frozen.sort_unstable();
    assert_eq!(frozen, vec![1, 2]);
    let cluster_hash1 = cluster.hash(1).unwrap();
    let cluster_hash2 = cluster.hash(2).unwrap();

    // Our node in commit mode. The fast lane is simulated by committing slot 1's first
    // transaction through the real commit path, with a wrong result.
    assert!(control::set_active(true));
    control::set_commit_mode(control::COMMIT_ON);
    control::set_follow_wait_ms(0);
    assert_eq!(fast_lane_commit::mode(), fast_lane_commit::MODE_ON);
    let mint = mint_keypair.pubkey();
    let mut node = Node::new(&genesis_config);
    node.blockstore.insert_shreds(shreds1.clone(), false).unwrap();
    node.blockstore.insert_shreds(shreds2.clone(), false).unwrap();
    let node_forks = node.bank_forks.clone();
    let fl_commit = |bank: &Bank| {
        if bank.slot() != 1 || fast_lane_commit::mode() != fast_lane_commit::MODE_ON {
            return;
        }
        let bank = node_forks.read().unwrap().get(1).unwrap();
        let board = fast_lane_commit::board_of(bank.bank_id()).expect("commit board");
        assert!(board.bind());
        let rtx = solana_runtime_transaction::runtime_transaction::ReplayTransaction::from(
            slot1_txs[0].clone(),
        );
        let batch = bank.prepare_unlocked_batch_from_single_tx(&rtx);
        let check = bank.check_transactions_before_execution::<
            solana_runtime_transaction::runtime_transaction::ReplayTransaction,
        >(
            batch.sanitized_transactions(),
            batch.lock_results(),
            bank.max_processing_age(),
            &mut Default::default(),
        );
        let mut output = bank.load_and_execute_transactions(
            &batch,
            bank.max_processing_age(),
            &mut ExecuteTimings::default(),
            &mut Default::default(),
            TransactionProcessingConfig::default(),
        );
        drop(batch);
        let Ok(ProcessedTransaction::Executed(executed)) = &mut output.processing_results[0] else {
            panic!("executed");
        };
        let (key, account) = &mut executed.loaded_transaction.accounts[0];
        assert_eq!(*key, mint);
        account.set_lamports(account.lamports() + 1);
        let result = commit_external(
            &bank,
            &board,
            0,
            &rtx,
            &check[0],
            output.processing_results.pop().unwrap(),
            output.balance_collector,
            CommitServices::default(),
            &mut ExecuteTimings::default(),
        );
        assert_eq!(result, ExternalCommit::Committed(Ok(())));
    };
    let mut frozen = node.replay_until_idle(&fl_commit);
    frozen.sort_unstable();
    assert_eq!(frozen, vec![1, 2]);
    let wrong_hash1 = node.hash(1).unwrap();
    assert_ne!(wrong_hash1, cluster_hash1);
    assert_ne!(node.hash(2).unwrap(), cluster_hash2);

    // Detection: FL off at once (provisionally), agave's gate off.
    node.cluster_confirms(1, cluster_hash1);
    assert!(control::is_poisoned());
    assert!(!control::is_poisoned_sticky(), "not attributed yet");
    assert_eq!(fast_lane_commit::mode(), fast_lane_commit::MODE_OFF);
    assert!(!fast_lane_commit::fl_live());
    node.cluster_confirms(2, cluster_hash2);
    let mut dumped = node.dump_then_repair();
    dumped.sort_unstable();
    assert_eq!(dumped, vec![(1, cluster_hash1), (2, cluster_hash2)]);

    // The same block, repaired and replayed without FL: the cluster's hashes. FL was wrong.
    node.blockstore.insert_shreds(shreds1, false).unwrap();
    node.blockstore.insert_shreds(shreds2, false).unwrap();
    let mut frozen = node.replay_until_idle(&fl_commit);
    frozen.sort_unstable();
    assert_eq!(frozen, vec![1, 2]);
    assert_eq!(node.hash(1), Some(cluster_hash1));
    assert_eq!(node.hash(2), Some(cluster_hash2));
    assert!(
        cluster_check::recent_events().iter().any(|e| e.slot == 1
            && e.kind == EventKind::Attribution { verdict: "fl_wrong" }),
        "attributed to the fast lane"
    );
    assert!(control::is_poisoned_sticky());
    assert!(!control::set_active(true));
    assert_eq!(fast_lane_commit::mode(), fast_lane_commit::MODE_OFF);
    control::set_follow_wait_ms(100);
}

// ---------------------------------------------------------------------------------------------
// Alpenglow: the reference hash is the leader's block footer, checked by replay at freeze.

/// The migration status of a cluster running Alpenglow from genesis (slot 0), as in replay
/// stage's own tests: every slot above 0 is an Alpenglow block.
fn alpenglow_migration_status() -> MigrationStatus {
    let migration_status = MigrationStatus::default();
    migration_status.record_feature_activation(0);
    let genesis_block = Block::new_unique(0);
    migration_status.set_genesis_block(genesis_block);
    migration_status.set_genesis_certificate(Arc::new(GenesisCert {
        block: genesis_block,
        signature: CertSignature {
            signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: vec![],
        },
    }));
    migration_status.enable_alpenglow_during_startup();
    migration_status
}

/// The transactions entry of an Alpenglow block (`num_hashes == 1`, low-power PoH) and its
/// alpentick (the single tick, last).
fn alpenglow_entries(start_hash: Hash, transactions: Vec<Transaction>) -> Vec<Entry> {
    let tx_entry = entry::next_entry(&start_hash, 1, transactions);
    let alpentick = entry::next_entry(&tx_entry.hash, 1, vec![]);
    vec![tx_entry, alpentick]
}

fn alpenglow_footer(bank_hash: Hash, block_producer_time_nanos: u64) -> BlockFooterV1 {
    BlockFooterV1 {
        bank_hash,
        block_producer_time_nanos,
        block_user_agent: vec![],
        block_final_cert: None,
        skip_reward_cert: None,
        notar_reward_cert: None,
    }
}

/// Merkle root of the last FEC set of `shreds` and the next shred index.
fn fec_end(shreds: &[Shred]) -> (Hash, u32) {
    let last = shreds
        .iter()
        .filter(|shred| shred.is_data())
        .max_by_key(|shred| shred.index())
        .unwrap();
    (last.merkle_root().unwrap(), last.index() + 1)
}

/// An Alpenglow block as its leader shreds it: header, the entries (the last one is the
/// alpentick), footer, each its own data set, FEC sets chained.
#[allow(clippy::too_many_arguments)]
fn alpenglow_shreds(
    keypair: &Keypair,
    slot: Slot,
    parent: Slot,
    parent_block_id: Hash,
    chained_merkle_root: Hash,
    entries: &[Entry],
    footer: BlockFooterV1,
) -> Vec<Shred> {
    let shredder = Shredder::new(slot, parent, 0, 0).unwrap();
    let cache = ReedSolomonCache::default();
    let mut stats = ProcessShredsStats::default();
    let header =
        BlockComponent::new_block_marker(VersionedBlockMarker::from_block_header(BlockHeaderV1 {
            parent_slot: parent,
            parent_block_id,
        }));
    let footer = BlockComponent::new_block_marker(VersionedBlockMarker::from_block_footer(footer));
    let (transactions, alpentick) = entries.split_at(entries.len() - 1);
    let (mut chained, mut next) = (chained_merkle_root, 0);
    let mut shreds = vec![];
    for piece in 0..4 {
        let fec_sets = match piece {
            0 => shredder.make_merkle_shreds_from_component(
                keypair, &header, false, chained, next, next, &cache, &mut stats,
            ),
            1 => shredder.make_merkle_shreds_from_entries(
                keypair,
                transactions,
                false,
                chained,
                next,
                next,
                &cache,
                &mut stats,
            ),
            2 => shredder.make_merkle_shreds_from_component(
                keypair, &footer, false, chained, next, next, &cache, &mut stats,
            ),
            _ => shredder.make_merkle_shreds_from_entries(
                keypair, alpentick, true, chained, next, next, &cache, &mut stats,
            ),
        };
        (chained, next) = fec_end(&fec_sets);
        shreds.extend(fec_sets);
    }
    shreds
}

impl Node {
    fn new_alpenglow(genesis_config: &solana_genesis_config::GenesisConfig) -> Self {
        let mut node = Self::new(genesis_config);
        node.ctx.migration_status = Arc::new(alpenglow_migration_status());
        node
    }

    fn leader(&self, slot: Slot) -> Pubkey {
        let bank0 = self.bank_forks.read().unwrap().get(0).unwrap();
        self.leader_schedule_cache
            .slot_leader_at(slot, Some(&bank0))
            .unwrap()
            .id
    }

    fn block_id(&self, slot: Slot) -> Hash {
        self.blockstore
            .get_block_id(slot, &self.ctx.migration_status)
            .unwrap()
            .expect("full Alpenglow block")
    }

    /// Replay-loop steps of the Alpenglow branch (no TowerBFT structures) as `my_pubkey`,
    /// until nothing changes. `before_replay` runs on every newly created bank before it
    /// replays.
    fn replay_alpenglow_until_idle(
        &mut self,
        my_pubkey: &Pubkey,
        before_replay: &dyn Fn(&Arc<Bank>),
    ) -> Vec<Slot> {
        let mut frozen = vec![];
        for _ in 0..16 {
            let before: HashSet<BankId> = self
                .bank_forks
                .read()
                .unwrap()
                .banks()
                .values()
                .map(|bank| bank.bank_id())
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
                    my_pubkey,
                },
                &mut self.progress,
                &mut replay_timing,
            );
            let created: Vec<Arc<Bank>> = self
                .bank_forks
                .read()
                .unwrap()
                .banks()
                .values()
                .filter(|bank| !before.contains(&bank.bank_id()))
                .map(|bank| bank.clone_without_scheduler())
                .collect();
            for bank in &created {
                before_replay(bank);
            }
            let newly_frozen = ReplayStage::process_active_banks(
                0,
                &self.ctx,
                &mut self.progress,
                &mut vec![],
                &mut LatestValidatorVotesForFrozenBanks::default(),
                &mut self.duplicate_slots_to_repair,
                &mut self.purge_repair_slot_counter,
                None,
                my_pubkey,
                my_pubkey,
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

    fn is_dead(&self, slot: Slot) -> bool {
        self.blockstore.is_dead(slot)
            || self
                .progress
                .get(&slot)
                .is_some_and(|progress| progress.dead_reason.is_some())
    }
}

/// Slots 1 and 2 of an Alpenglow cluster, slot 2 spending what slot 1 transferred.
struct AlpenglowCase {
    genesis_config: solana_genesis_config::GenesisConfig,
    mint: Pubkey,
    payers: Vec<Pubkey>,
    slot1_txs: Vec<Transaction>,
    slot2_txs: Vec<Transaction>,
    /// The leaders' footer timestamps of slots 1 and 2 (within the nanosecond-clock bounds).
    footer_nanos: [u64; 2],
}

impl AlpenglowCase {
    fn new(seed: u8) -> Self {
        let GenesisConfigInfo {
            mut genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(100 * solana_native_token::LAMPORTS_PER_SOL);
        solana_runtime::genesis_utils::activate_feature(
            &mut genesis_config,
            agave_feature_set::alpenglow::id(),
        );
        let genesis_hash = genesis_config.hash();
        let payers: Vec<Keypair> = (0..3u8)
            .map(|i| Keypair::new_from_array([seed + i; 32]))
            .collect();
        let slot1_txs = payers
            .iter()
            .map(|payer| {
                system_transaction::transfer(
                    &mint_keypair,
                    &payer.pubkey(),
                    solana_native_token::LAMPORTS_PER_SOL,
                    genesis_hash,
                )
            })
            .collect();
        let slot2_txs = vec![system_transaction::transfer(
            &payers[0],
            &payers[1].pubkey(),
            solana_native_token::LAMPORTS_PER_SOL / 2,
            genesis_hash,
        )];
        // The footer's timestamp must lie just above its parent's clock: slot 1's parent clock
        // is the clock replay gives bank 1 at creation (an isolated bank shows it).
        let (probe_parent, _probe_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        let probe = Bank::new_from_parent(probe_parent, SlotLeader::default(), 1);
        let slot1_nanos =
            u64::try_from(probe.clock().unix_timestamp).unwrap() * 1_000_000_000 + 1_000_000;
        Self {
            genesis_config,
            mint: mint_keypair.pubkey(),
            payers: payers.iter().map(|p| p.pubkey()).collect(),
            slot1_txs,
            slot2_txs,
            footer_nanos: [slot1_nanos, slot1_nanos + 1_000_000],
        }
    }
}

/// The fast lane's execute-once commit of `tx` as transaction 0 of `bank`, through the real
/// commit path (`commit_external`) on a board bound to the bank; `wrong`: with the fee payer's
/// post-state one lamport too high.
fn fl_commit_first_tx(bank: &Arc<Bank>, tx: &Transaction, wrong: bool) {
    use {
        solana_runtime::transaction_execution::{CommitServices, ExternalCommit, commit_external},
        solana_runtime_transaction::runtime_transaction::ReplayTransaction,
        solana_svm::{
            transaction_processing_result::ProcessedTransaction,
            transaction_processor::TransactionProcessingConfig,
        },
        solana_svm_timings::ExecuteTimings,
    };
    // A board whatever the process-global commit mode (other tests in this binary may poison
    // the fast lane at any time).
    let board = fast_lane_commit::insert_board_for_tests(bank);
    assert!(board.bind());
    let rtx = ReplayTransaction::from(tx.clone());
    let batch = bank.prepare_unlocked_batch_from_single_tx(&rtx);
    let check = bank.check_transactions_before_execution::<ReplayTransaction>(
        batch.sanitized_transactions(),
        batch.lock_results(),
        bank.max_processing_age(),
        &mut Default::default(),
    );
    let mut output = bank.load_and_execute_transactions(
        &batch,
        bank.max_processing_age(),
        &mut ExecuteTimings::default(),
        &mut Default::default(),
        TransactionProcessingConfig::default(),
    );
    drop(batch);
    let Ok(ProcessedTransaction::Executed(executed)) = &mut output.processing_results[0] else {
        panic!("executed");
    };
    if wrong {
        let (_, account) = &mut executed.loaded_transaction.accounts[0];
        account.set_lamports(account.lamports() + 1);
    }
    let result = commit_external(
        bank,
        &board,
        0,
        &rtx,
        &check[0],
        output.processing_results.pop().unwrap(),
        output.balance_collector,
        CommitServices::default(),
        &mut ExecuteTimings::default(),
    );
    assert_eq!(result, ExternalCommit::Committed(Ok(())));
}

fn footer_events(slot: Slot, footer_hash: Hash) -> Vec<cluster_check::Event> {
    cluster_check::recent_events()
        .into_iter()
        .filter(|e| e.slot == slot && e.cluster_hash == footer_hash)
        .collect()
}

fn footer_pending(footer_hash: Hash) -> bool {
    control::open_provisional_reasons()
        .iter()
        .any(|reason| str::contains(reason, footer_hash.to_string().as_str()))
}

/// What a newly created bank of slot 1 found before it replayed.
#[derive(Debug, Default, Clone, PartialEq)]
struct Creation {
    agave_only: bool,
    board: bool,
    footer_pending: bool,
    dead: bool,
}

impl Creation {
    fn of(bank: &Bank, blockstore: &Blockstore, footer_hash: Hash) -> Self {
        Self {
            agave_only: fast_lane_commit::is_agave_only_slot(bank.slot()),
            board: fast_lane_commit::board_of(bank.bank_id()).is_some(),
            footer_pending: footer_pending(footer_hash),
            dead: blockstore.is_dead(bank.slot()),
        }
    }
}

/// Alpenglow, a wrong fast-lane result: slot 1's first transaction is committed through the
/// real execute-once path with the mint's post-state one lamport too high, agave's replay
/// follows it (and executes the others), and the frozen hash differs from the leader's footer.
/// Replay does not mark the slot dead: the fast lane is disabled provisionally, the bank is
/// cleared and the slot replayed once without it. The same block then matches the footer, so
/// the disagreement is the fast lane's (`fl_wrong`, sticky); the slot is alive with the
/// cluster's state, and its child replays on top.
#[test]
fn test_alpenglow_footer_mismatch_replays_without_fl_and_attributes_fl_wrong() {
    let _serial = FL_E2E.lock().unwrap_or_else(|e| e.into_inner());
    agave_logger::setup();
    control::unpoison_for_tests();
    let case = AlpenglowCase::new(61);
    let shred_keypair = Keypair::new_from_array([13; 32]);

    // The cluster: the leader's own replay of the same blocks (no footer check, no fast lane)
    // gives the bank hashes the footers carry.
    let mut cluster = Node::new_alpenglow(&case.genesis_config);
    let leader = cluster.leader(1);
    assert_eq!(cluster.leader(2), leader);
    let entries1 = alpenglow_entries(
        cluster
            .bank_forks
            .read()
            .unwrap()
            .get(0)
            .unwrap()
            .last_blockhash(),
        case.slot1_txs.clone(),
    );
    let entries2 = alpenglow_entries(entries1.last().unwrap().hash, case.slot2_txs.clone());
    let unknown = alpenglow_footer(Hash::default(), case.footer_nanos[0]);
    let shreds1 = alpenglow_shreds(
        &shred_keypair,
        1,
        0,
        Hash::default(),
        Hash::default(),
        &entries1,
        unknown,
    );
    cluster
        .blockstore
        .insert_shreds(shreds1.clone(), true)
        .unwrap();
    let shreds2 = alpenglow_shreds(
        &shred_keypair,
        2,
        1,
        cluster.block_id(1),
        fec_end(&shreds1).0,
        &entries2,
        alpenglow_footer(Hash::default(), case.footer_nanos[1]),
    );
    cluster.blockstore.insert_shreds(shreds2, true).unwrap();
    let mut frozen = cluster.replay_alpenglow_until_idle(&leader, &|_| {});
    frozen.sort_unstable();
    assert_eq!(frozen, vec![1, 2]);
    let (footer1, footer2) = (cluster.hash(1).unwrap(), cluster.hash(2).unwrap());

    // Our node (not the leader) receives the blocks with those footers.
    let mut node = Node::new_alpenglow(&case.genesis_config);
    let ours1 = alpenglow_shreds(
        &shred_keypair,
        1,
        0,
        Hash::default(),
        Hash::default(),
        &entries1,
        alpenglow_footer(footer1, case.footer_nanos[0]),
    );
    node.blockstore.insert_shreds(ours1.clone(), true).unwrap();
    let ours2 = alpenglow_shreds(
        &shred_keypair,
        2,
        1,
        node.block_id(1),
        fec_end(&ours1).0,
        &entries2,
        alpenglow_footer(footer2, case.footer_nanos[1]),
    );
    node.blockstore.insert_shreds(ours2, true).unwrap();

    let creations = std::cell::RefCell::new(Vec::<Creation>::new());
    let blockstore = node.blockstore.clone();
    let fl = |bank: &Arc<Bank>| {
        if bank.slot() != 1 {
            return;
        }
        let mut creations = creations.borrow_mut();
        creations.push(Creation::of(bank, &blockstore, footer1));
        if creations.len() == 1 {
            fl_commit_first_tx(bank, &case.slot1_txs[0], true);
        }
    };
    let mut frozen = node.replay_alpenglow_until_idle(&TEST_PUBKEY, &fl);
    frozen.sort_unstable();
    assert_eq!(
        frozen,
        vec![1, 2],
        "slot 1 is not dead; its child replays on top"
    );

    // Slot 1 was replayed twice: with the fast lane, then (the bank cleared, the slot
    // agave-only, no board, the fast lane provisionally off pending attribution) without it.
    let creations = creations.into_inner();
    assert_eq!(creations.len(), 2, "{creations:?}");
    assert!(!creations[0].footer_pending && !creations[0].dead);
    assert_eq!(
        creations[1],
        Creation {
            agave_only: true,
            board: false,
            footer_pending: true,
            dead: false,
        }
    );
    let mismatch = footer_events(1, footer1)
        .into_iter()
        .find(|e| {
            e.kind
                == EventKind::Mismatch {
                    source: "block_footer",
                }
        })
        .expect("the footer mismatch was recorded");
    let wrong_hash = mismatch.our_hash.unwrap();
    assert_ne!(wrong_hash, footer1);

    // The verdict: the same block without the fast lane matches the footer.
    let events = footer_events(1, footer1);
    assert!(
        events.iter().any(|e| e.our_hash == Some(wrong_hash)
            && e.kind
                == EventKind::Attribution {
                    verdict: "fl_wrong"
                }),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == EventKind::Match { recovered: true })
    );
    assert!(control::is_poisoned_sticky());
    assert!(!footer_pending(footer1));
    assert!(!control::set_active(true), "enable refused until restart");
    assert_eq!(fast_lane_commit::mode(), fast_lane_commit::MODE_OFF);
    assert!(cluster_check::last_matched_slot() >= 2);

    // Not stuck: both slots frozen with the cluster's hashes and state, nothing dead.
    assert_eq!(node.hash(1), Some(footer1));
    assert_eq!(node.hash(2), Some(footer2));
    assert!(!node.is_dead(1) && !node.is_dead(2));
    for pubkey in std::iter::once(case.mint).chain(case.payers.iter().copied()) {
        assert_eq!(
            node.account(2, &pubkey),
            cluster.account(2, &pubkey),
            "{pubkey}"
        );
    }
}

/// Our node replays slot 1 of `case` whose footer claims `footer_hash`; `fl_commit`: the fast
/// lane commits its first transaction on the first replay, correctly or (`Some(true)`) with the
/// fee payer one lamport too high. Returns the frozen slots and what each new bank of slot 1
/// found before replaying.
fn replay_alpenglow_slot1(
    node: &mut Node,
    case: &AlpenglowCase,
    footer_hash: Hash,
    fl_commit: Option<bool>,
) -> (Vec<Slot>, Vec<Creation>) {
    let shred_keypair = Keypair::new_from_array([17; 32]);
    let entries1 = alpenglow_entries(
        node.bank_forks
            .read()
            .unwrap()
            .get(0)
            .unwrap()
            .last_blockhash(),
        case.slot1_txs.clone(),
    );
    let shreds = alpenglow_shreds(
        &shred_keypair,
        1,
        0,
        Hash::default(),
        Hash::default(),
        &entries1,
        alpenglow_footer(footer_hash, case.footer_nanos[0]),
    );
    node.blockstore.insert_shreds(shreds, true).unwrap();
    let creations = std::cell::RefCell::new(Vec::<Creation>::new());
    let blockstore = node.blockstore.clone();
    let fl = |bank: &Arc<Bank>| {
        if bank.slot() != 1 {
            return;
        }
        let mut creations = creations.borrow_mut();
        creations.push(Creation::of(bank, &blockstore, footer_hash));
        if let Some(wrong) = fl_commit.filter(|_| creations.len() == 1) {
            fl_commit_first_tx(bank, &case.slot1_txs[0], wrong);
        }
    };
    let frozen = node.replay_alpenglow_until_idle(&TEST_PUBKEY, &fl);
    (frozen, creations.into_inner())
}

fn footer_mismatches(slot: Slot, footer_hash: Hash) -> Vec<cluster_check::Event> {
    footer_events(slot, footer_hash)
        .into_iter()
        .filter(|e| {
            e.kind
                == EventKind::Mismatch {
                    source: "block_footer",
                }
        })
        .collect()
}

fn verdicts(slot: Slot, footer_hash: Hash) -> Vec<&'static str> {
    footer_events(slot, footer_hash)
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::Attribution { verdict } => Some(verdict),
            _ => None,
        })
        .collect()
}

/// Alpenglow, a wrong footer: the fast lane committed slot 1's first transaction correctly, yet
/// the frozen hash differs from the leader's footer. The replay without the fast lane differs
/// from it too, with the same hash: not the fast lane's fault (`leader_footer`). Its provisional
/// poison is lifted and the slot is marked dead, as upstream does on a footer mismatch.
#[test]
fn test_alpenglow_footer_mismatch_without_fl_too_lifts_and_marks_dead() {
    let _serial = FL_E2E.lock().unwrap_or_else(|e| e.into_inner());
    agave_logger::setup();
    control::unpoison_for_tests();
    let case = AlpenglowCase::new(71);
    let mut node = Node::new_alpenglow(&case.genesis_config);
    let bad_footer = Hash::new_unique();
    let (frozen, creations) = replay_alpenglow_slot1(&mut node, &case, bad_footer, Some(false));
    assert!(frozen.is_empty());
    assert_eq!(
        creations.len(),
        2,
        "replayed once more without the fast lane"
    );
    assert_eq!(
        creations[1],
        Creation {
            agave_only: true,
            board: false,
            footer_pending: true,
            dead: false,
        }
    );
    let mismatches = footer_mismatches(1, bad_footer);
    assert_eq!(mismatches.len(), 2, "{mismatches:?}");
    assert_eq!(
        mismatches[0].our_hash, mismatches[1].our_hash,
        "the fast lane's commit was right"
    );
    assert_eq!(verdicts(1, bad_footer), vec!["leader_footer"]);
    assert!(!footer_pending(bad_footer), "provisional poison lifted");
    if !control::is_poisoned_sticky() && control::open_provisional_reasons().is_empty() {
        assert!(
            !control::is_poisoned(),
            "nothing else holds the fast lane off"
        );
    }
    assert!(node.is_dead(1), "marked dead as upstream");
}

/// Alpenglow, a footer mismatch on a bank the fast lane committed nothing into: upstream
/// behaviour exactly (the slot is marked dead at the first freeze, no second replay), and the
/// fast lane is not disabled for it (`not_fl`; before, its version was unknown at this point and
/// the disagreement poisoned the fast lane for good).
#[test]
fn test_alpenglow_footer_mismatch_without_fl_commits_is_upstream() {
    let _serial = FL_E2E.lock().unwrap_or_else(|e| e.into_inner());
    agave_logger::setup();
    control::unpoison_for_tests();
    let case = AlpenglowCase::new(81);
    let mut node = Node::new_alpenglow(&case.genesis_config);
    let bad_footer = Hash::new_unique();
    let (frozen, creations) = replay_alpenglow_slot1(&mut node, &case, bad_footer, None);
    assert!(frozen.is_empty());
    assert_eq!(creations.len(), 1, "no replay without the fast lane");
    assert!(node.is_dead(1));
    assert_eq!(footer_mismatches(1, bad_footer).len(), 1);
    assert_eq!(verdicts(1, bad_footer), vec!["not_fl"]);
    assert!(!footer_pending(bad_footer));
    assert!(
        control::poison_reason()
            .is_none_or(|r| !str::contains(&r, bad_footer.to_string().as_str())),
        "{:?}",
        control::poison_reason()
    );
}

/// Alpenglow, the fast lane and the footer both wrong: the replay without the fast lane differs
/// from the footer too, but with another hash than the fast lane's version: the fast lane changed
/// the bank (`fl_wrong`, sticky) even though the leader's claim is also wrong; the slot is dead.
#[test]
fn test_alpenglow_footer_mismatch_with_another_hash_without_fl_is_fl_wrong() {
    let _serial = FL_E2E.lock().unwrap_or_else(|e| e.into_inner());
    agave_logger::setup();
    control::unpoison_for_tests();
    let case = AlpenglowCase::new(91);
    let mut node = Node::new_alpenglow(&case.genesis_config);
    let bad_footer = Hash::new_unique();
    let (frozen, creations) = replay_alpenglow_slot1(&mut node, &case, bad_footer, Some(true));
    assert!(frozen.is_empty());
    assert_eq!(
        creations.len(),
        2,
        "replayed once more without the fast lane"
    );
    let mismatches = footer_mismatches(1, bad_footer);
    assert_eq!(mismatches.len(), 2, "{mismatches:?}");
    assert_ne!(mismatches[0].our_hash, mismatches[1].our_hash);
    assert_eq!(verdicts(1, bad_footer), vec!["fl_wrong"]);
    assert!(control::is_poisoned_sticky());
    assert!(!footer_pending(bad_footer));
    assert!(
        node.is_dead(1),
        "the footer is wrong as well: marked dead as upstream"
    );
}
