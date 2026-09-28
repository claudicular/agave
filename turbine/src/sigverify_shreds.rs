use {
    crate::{
        cluster_nodes::{ClusterNodesCache, DATA_PLANE_FANOUT},
        receive_diet::{self, TraceStage, receive_diet},
        retransmit_stage::RetransmitStage,
    },
    agave_feature_set as feature_set,
    crossbeam_channel::{Receiver, RecvTimeoutError, SendError, Sender},
    itertools::{Either, Itertools},
    rayon::{ThreadPool, ThreadPoolBuilder, prelude::*},
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_keypair::Keypair,
    solana_ledger::{
        blockstore_meta::BlockLocation,
        leader_schedule_cache::LeaderScheduleCache,
        shred::{
            self,
            layout::{get_shred, resign_packet},
            wire::is_retransmitter_signed_variant,
        },
        sigverify_shreds::{LruCache, SlotPubkeys, verify_shreds, verify_shreds_serial},
    },
    solana_perf::{
        self,
        deduper::Deduper,
        packet::{PacketBatch, PacketRef, PacketRefMut},
    },
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_signer::Signer,
    solana_streamer::{evicting_sender::EvictingSender, streamer::ChannelSend},
    std::{
        num::NonZeroUsize,
        sync::{
            Arc, RwLock,
            atomic::{AtomicUsize, Ordering},
        },
        thread::{Builder, JoinHandle},
        time::{Duration, Instant},
    },
    thiserror::Error,
};

// 34MB where each cache entry is 136 bytes.
const SIGVERIFY_LRU_CACHE_CAPACITY: usize = 1 << 18;

const DEDUPER_FALSE_POSITIVE_RATE: f64 = 0.001;
const DEDUPER_NUM_BITS: u64 = 637_534_199; // 76MB
const DEDUPER_RESET_CYCLE: Duration = Duration::from_secs(5 * 60);

// Num epochs capacity should be at least 2 because near the epoch boundary we
// may receive shreds from the other side of the epoch boundary. Because of the
// TTL based eviction it is extremely unlikely that we will ever store > 2 epochs anyway
const CLUSTER_NODES_CACHE_NUM_EPOCH_CAP: usize = 2;
// Because for ClusterNodes::get_retransmit_parent only pubkeys of staked nodes
// are needed, we can use longer durations for cache TTL.
const CLUSTER_NODES_CACHE_TTL: Duration = Duration::from_secs(30);

/// Maximum number of packet batches to process in a single sigverify iteration.
const SIGVERIFY_SHRED_BATCH_SIZE: usize = 1024;

#[allow(clippy::enum_variant_names)]
enum ShredSigverifyError {
    RecvDisconnected,
    RecvTimeout,
    SendError,
}

#[derive(Debug, Error)]
enum ResignError {
    #[error("verification of retransmitter signature failed")]
    VerifyRetransmitterSignature,
    #[error(transparent)]
    Shred(#[from] shred::Error),
}

pub type RepairNonceLocationLookup = dyn Fn(shred::Nonce) -> Option<BlockLocation> + Send + Sync;

pub fn spawn_shred_sigverify(
    cluster_info: Arc<ClusterInfo>,
    bank_forks: Arc<RwLock<BankForks>>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    shred_fetch_receiver: Receiver<PacketBatch>,
    retransmit_sender: EvictingSender<Vec<shred::Payload>>,
    verified_sender: Sender<Vec<(shred::Payload, /*is_repaired:*/ bool, BlockLocation)>>,
    repair_nonce_location_lookup: Arc<RepairNonceLocationLookup>,
    num_sigverify_threads: NonZeroUsize,
) -> JoinHandle<()> {
    let mut stats = ShredSigVerifyStats::new(Instant::now());
    let cache = RwLock::new(LruCache::new(SIGVERIFY_LRU_CACHE_CAPACITY));
    let cluster_nodes_cache = ClusterNodesCache::<RetransmitStage>::new(
        CLUSTER_NODES_CACHE_NUM_EPOCH_CAP,
        CLUSTER_NODES_CACHE_TTL,
    );
    let thread_pool = ThreadPoolBuilder::new()
        .num_threads(num_sigverify_threads.get())
        .thread_name(|i| format!("solSvrfyShred{i:02}"))
        .build()
        .expect("new rayon threadpool");
    let run_shred_sigverify = move || {
        let mut rng = rand::rng();
        let deduper = Deduper::<2, [u8]>::new(&mut rng, DEDUPER_NUM_BITS);
        let mut shred_buffer = Vec::with_capacity(SIGVERIFY_SHRED_BATCH_SIZE);
        loop {
            if deduper.maybe_reset(&mut rng, DEDUPER_FALSE_POSITIVE_RATE, DEDUPER_RESET_CYCLE) {
                stats.num_deduper_saturations += 1;
            }
            // We can't store the keypair outside the loop
            // because the identity might be hot swapped.
            let keypair = cluster_info.keypair();
            // Receive diet switches may be flipped at runtime; read them once per iteration.
            let serial_max_packets = receive_diet().serial_sigverify_max_packets();
            let tracing = receive_diet::trace_enabled();
            match run_shred_sigverify(
                &thread_pool,
                &keypair,
                &cluster_info,
                &bank_forks,
                &leader_schedule_cache,
                &deduper,
                &shred_fetch_receiver,
                &retransmit_sender,
                &verified_sender,
                &cluster_nodes_cache,
                repair_nonce_location_lookup.as_ref(),
                &cache,
                &mut stats,
                &mut shred_buffer,
                serial_max_packets,
                tracing,
            ) {
                Ok(()) => (),
                Err(ShredSigverifyError::RecvTimeout) => (),
                Err(ShredSigverifyError::RecvDisconnected) => break,
                Err(ShredSigverifyError::SendError) => break,
            }
            stats.maybe_submit();
        }
    };
    Builder::new()
        .name("solShredVerifr".to_string())
        .spawn(run_shred_sigverify)
        .unwrap()
}

#[allow(clippy::too_many_arguments)]
fn run_shred_sigverify<const K: usize>(
    thread_pool: &ThreadPool,
    keypair: &Keypair,
    cluster_info: &ClusterInfo,
    bank_forks: &RwLock<BankForks>,
    leader_schedule_cache: &LeaderScheduleCache,
    deduper: &Deduper<K, [u8]>,
    shred_fetch_receiver: &Receiver<PacketBatch>,
    retransmit_sender: &EvictingSender<Vec<shred::Payload>>,
    verified_sender: &Sender<Vec<(shred::Payload, /*is_repaired:*/ bool, BlockLocation)>>,
    cluster_nodes_cache: &ClusterNodesCache<RetransmitStage>,
    repair_nonce_location_lookup: &RepairNonceLocationLookup,
    cache: &RwLock<LruCache>,
    stats: &mut ShredSigVerifyStats,
    shred_buffer: &mut Vec<PacketBatch>,
    // Receive diet `serial_sigverify` bound; 0 = off.
    serial_max_packets: usize,
    // Receive diet tracer on.
    tracing: bool,
) -> Result<(), ShredSigverifyError> {
    const RECV_TIMEOUT: Duration = Duration::from_secs(1);
    let packets = shred_fetch_receiver.recv_timeout(RECV_TIMEOUT)?;
    stats.num_packets += packets.len();
    shred_buffer.push(packets);
    for packets in shred_fetch_receiver
        .try_iter()
        .take(SIGVERIFY_SHRED_BATCH_SIZE - 1)
    {
        stats.num_packets += packets.len();
        shred_buffer.push(packets);
    }

    let now = Instant::now();
    if tracing {
        trace_packets(TraceStage::SigverifyDequeued, shred_buffer);
    }
    stats.num_iters += 1;
    stats.num_batches += shred_buffer.len();
    stats.num_discards_pre += count_discards(shred_buffer);
    // With the receive diet `serial_sigverify` switch, an iteration holding only a few packets
    // (the common case once the TVU receive coalesce is off) is processed on this thread: the
    // same dedup, verification and resigning, without three rayon hand-offs.
    let thread_pool = (serial_max_packets == 0
        || shred_buffer.iter().map(PacketBatch::len).sum::<usize>() > serial_max_packets)
        .then_some(thread_pool);
    // Repair shreds include a randomly generated u32 nonce, so it does not
    // make sense to deduplicate the entire packet payload (i.e. they are not
    // duplicate of any other packet.data(..)).
    // If the nonce is excluded from the deduper then false positives might
    // prevent us from repairing a block until the deduper is reset after
    // DEDUPER_RESET_CYCLE. A workaround is to also repair "coding" shreds to
    // add some redundancy but that is not implemented at the moment.
    // Because the repair nonce is already verified in shred-fetch-stage we can
    // exclude repair shreds from the deduper, but we still need to pass the
    // repair shred to the deduper to filter out duplicates from the turbine
    // path once a shred is repaired.
    // For backward compatibility we need to allow trailing bytes in the packet
    // after the shred payload, but have to exclude them here from the deduper.
    let is_duplicate = |packet: &PacketRefMut| {
        !packet.meta().discard()
            && shred::wire::get_shred(packet.as_ref())
                .map(|shred| deduper.dedup(shred))
                .unwrap_or(true)
            && !packet.meta().repair()
    };
    stats.num_duplicates += match thread_pool {
        Some(thread_pool) => thread_pool.install(|| {
            shred_buffer
                .par_iter_mut()
                .flatten()
                .filter(is_duplicate)
                .map(|mut packet| packet.meta_mut().set_discard(true))
                .count()
        }),
        None => shred_buffer
            .iter_mut()
            .flat_map(|batch| batch.iter_mut())
            .filter(is_duplicate)
            .map(|mut packet| packet.meta_mut().set_discard(true))
            .count(),
    };
    let (working_bank, root_bank) = {
        let bank_forks = bank_forks.read().unwrap();
        (bank_forks.working_bank(), bank_forks.root_bank())
    };
    verify_packets(
        thread_pool,
        &keypair.pubkey(),
        &working_bank,
        leader_schedule_cache,
        shred_buffer,
        cache,
    );
    stats.num_discards_post += count_discards(shred_buffer);
    // Verify retransmitter's signature, and resign shreds
    // Merkle root as the retransmitter node.
    let resign_start = Instant::now();
    let verify_and_resign = |mut packet: PacketRefMut| {
        if maybe_verify_and_resign_packet(
            &mut packet,
            &root_bank,
            &working_bank,
            cluster_info,
            leader_schedule_cache,
            cluster_nodes_cache,
            stats,
            keypair,
        )
        .is_err()
        {
            packet.meta_mut().set_discard(true);
        }
    };
    // The pass only resigns resigned-variant shreds (the last erasure batches of a slot) and
    // discards packets it cannot parse; with the diet switch on it is skipped when the iteration
    // holds neither.
    if serial_max_packets == 0 || needs_resign_pass(shred_buffer) {
        match thread_pool {
            Some(thread_pool) => thread_pool.install(|| {
                shred_buffer
                    .par_iter_mut()
                    .flatten()
                    .filter(|packet| !packet.meta().discard())
                    .for_each(verify_and_resign)
            }),
            None => shred_buffer
                .iter_mut()
                .flat_map(|batch| batch.iter_mut())
                .filter(|packet| !packet.meta().discard())
                .for_each(verify_and_resign),
        }
    }
    stats.resign_micros += resign_start.elapsed().as_micros() as u64;
    // Extract shred payload from packets, and separate out repaired shreds.
    let (shreds, repairs): (Vec<_>, Vec<_>) = shred_buffer
        .iter()
        .flat_map(|batch| batch.iter())
        .filter(|packet| !packet.meta().discard())
        .filter_map(|packet| {
            extract_shred_and_location(packet, repair_nonce_location_lookup, stats)
        })
        .partition_map(|(shred, location)| {
            if let Some(location) = location {
                // No need for Arc overhead here because repaired shreds are
                // not retranmitted.
                Either::Right((
                    shred::Payload::from(shred),
                    /* is_repaired */ true,
                    location,
                ))
            } else {
                // Share the payload between the retransmit-stage and the
                // window-service.
                Either::Left(shred::Payload::from(shred))
            }
        });

    // Repaired shreds are not retransmitted.
    stats.num_retransmit_shreds += shreds.len();
    if let Err(send_err) = retransmit_sender.try_send(shreds.clone()) {
        match send_err {
            crossbeam_channel::TrySendError::Full(v) => {
                stats.num_retransmit_stage_overflow_shreds += v.len();
            }
            _ => unreachable!("EvictingSender holds on to both ends of the channel"),
        }
    }
    // Send all shreds to window service to be inserted into blockstore.
    if tracing {
        shreds
            .iter()
            .chain(repairs.iter().map(|(shred, _, _)| shred))
            .for_each(|shred| receive_diet::trace_shred(TraceStage::SigverifySent, shred));
    }
    let shreds = shreds
        .into_iter()
        .map(|shred| (shred, /*is_repaired:*/ false, BlockLocation::Original));
    verified_sender.send(shreds.chain(repairs).collect())?;
    stats.elapsed_micros += now.elapsed().as_micros() as u64;
    shred_buffer.clear();
    Ok(())
}

/// Extracts shred bytes and, for repaired shreds, the location where the shred
/// should be inserted into blockstore.
fn extract_shred_and_location(
    packet: PacketRef,
    repair_nonce_location_lookup: &RepairNonceLocationLookup,
    stats: &mut ShredSigVerifyStats,
) -> Option<(Vec<u8>, Option<BlockLocation>)> {
    let (shred, nonce) = shred::layout::get_shred_and_repair_nonce(packet)?;
    let Some(nonce) = nonce else {
        // Turbine shred.
        return Some((shred.to_vec(), None));
    };

    // Repair shred.
    if let Some(location) = repair_nonce_location_lookup(nonce) {
        Some((shred.to_vec(), Some(location)))
    } else {
        // This indicates the request entry was evicted before consumption.
        stats.num_unknown_block_location += 1;
        None
    }
}

/// Checks whether the shred in the given `packet` is of resigned variant. If
/// yes, it calls [`verify_and_resign_shred`].
fn maybe_verify_and_resign_packet(
    packet: &mut PacketRefMut,
    root_bank: &Bank,
    working_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule_cache: &LeaderScheduleCache,
    cluster_nodes_cache: &ClusterNodesCache<RetransmitStage>,
    stats: &ShredSigVerifyStats,
    keypair: &Keypair,
) -> Result<(), ResignError> {
    let repair = packet.meta().repair();
    let shred = get_shred(packet.as_ref()).ok_or(shred::Error::InvalidPacketSize)?;
    let is_signed = is_retransmitter_signed_variant(shred)?;
    if is_signed {
        // Repair packets do not follow turbine tree and
        // are verified using the trailing nonce.
        if !repair
            && !verify_retransmitter_signature(
                shred,
                root_bank,
                working_bank,
                cluster_info,
                leader_schedule_cache,
                cluster_nodes_cache,
                stats,
            )
        {
            stats
                .num_invalid_retransmitter
                .fetch_add(1, Ordering::Relaxed);
            if shred::layout::get_slot(shred)
                .map(|slot| {
                    shred::filter::check_feature_activation_from_bank(
                        &feature_set::verify_retransmitter_signature::id(),
                        slot,
                        root_bank,
                    )
                })
                .unwrap_or_default()
            {
                return Err(ResignError::VerifyRetransmitterSignature);
            }
        }

        resign_packet(packet, keypair)?;
    }

    Ok(())
}

#[must_use]
fn verify_retransmitter_signature(
    shred: &[u8],
    root_bank: &Bank,
    working_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule_cache: &LeaderScheduleCache,
    cluster_nodes_cache: &ClusterNodesCache<RetransmitStage>,
    stats: &ShredSigVerifyStats,
) -> bool {
    let signature = match shred::layout::get_retransmitter_signature(shred) {
        Ok(signature) => signature,
        // If the shred is not of resigned variant,
        // then there is nothing to verify.
        Err(shred::Error::InvalidShredVariant) => return true,
        Err(_) => return false,
    };
    let Some(merkle_root) = shred::layout::get_merkle_root(shred) else {
        return false;
    };
    let Some(shred) = shred::layout::get_shred_id(shred) else {
        return false;
    };
    let Some(leader) = leader_schedule_cache.slot_leader_at(shred.slot(), Some(working_bank))
    else {
        stats
            .num_unknown_slot_leader
            .fetch_add(1, Ordering::Relaxed);
        return false;
    };
    let cluster_nodes =
        cluster_nodes_cache.get(shred.slot(), root_bank, working_bank, cluster_info);
    let parent = match cluster_nodes.get_retransmit_parent(&leader.id, &shred, DATA_PLANE_FANOUT) {
        Ok(Some(parent)) => parent,
        Ok(None) => {
            stats
                .num_retranmitter_signature_skipped
                .fetch_add(1, Ordering::Relaxed);
            return true;
        }
        Err(err) => {
            error!("get_retransmit_parent: {err:?}");
            stats
                .num_unknown_turbine_parent
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }
    };
    if signature.verify(parent.as_ref(), merkle_root.as_ref()) {
        stats
            .num_retranmitter_signature_verified
            .fetch_add(1, Ordering::Relaxed);
        true
    } else {
        false
    }
}

fn verify_packets(
    // None: verify on the calling thread.
    thread_pool: Option<&ThreadPool>,
    self_pubkey: &Pubkey,
    working_bank: &Bank,
    leader_schedule_cache: &LeaderScheduleCache,
    packets: &mut [PacketBatch],
    cache: &RwLock<LruCache>,
) {
    let leader_slots: SlotPubkeys =
        get_slot_leaders(self_pubkey, packets, leader_schedule_cache, working_bank)
            .filter_map(|(slot, pubkey)| Some((slot, pubkey?)))
            .chain(std::iter::once((Slot::MAX, Pubkey::default())))
            .collect();
    let out = match thread_pool {
        Some(thread_pool) => verify_shreds(thread_pool, packets, &leader_slots, cache),
        None => verify_shreds_serial(packets, &leader_slots, cache),
    };
    solana_perf::sigverify::mark_disabled(packets, &out);
}

/// True if the resign pass has anything to do: a non-discarded packet that is of a resigned
/// variant, or whose variant cannot be read (the pass discards those).
fn needs_resign_pass(packets: &[PacketBatch]) -> bool {
    packets
        .iter()
        .flat_map(|batch| batch.iter())
        .filter(|packet| !packet.meta().discard())
        .any(|packet| {
            get_shred(packet)
                .map(|shred| is_retransmitter_signed_variant(shred).unwrap_or(true))
                .unwrap_or(true)
        })
}

fn trace_packets(stage: TraceStage, packets: &[PacketBatch]) {
    packets
        .iter()
        .flat_map(|batch| batch.iter())
        .filter(|packet| !packet.meta().discard())
        .filter_map(get_shred)
        .for_each(|shred| receive_diet::trace_shred(stage, shred));
}

// Returns pubkey of leaders for shred slots referenced in the packets.
// Marks packets as discard if:
//   - fails to deserialize the shred slot.
//   - slot leader is unknown.
//   - slot leader is the node itself (circular transmission).
fn get_slot_leaders<'a>(
    self_pubkey: &'a Pubkey,
    batches: &'a mut [PacketBatch],
    leader_schedule_cache: &'a LeaderScheduleCache,
    bank: &'a Bank,
) -> impl Iterator<Item = (Slot, Option<Pubkey>)> + 'a {
    batches
        .iter_mut()
        .flat_map(|batch| batch.iter_mut())
        .filter(|packet| !packet.meta().discard())
        .filter_map(move |mut packet| {
            let shred = shred::layout::get_shred(packet.as_ref());
            let slot = shred.and_then(shred::layout::get_slot)?;
            let leader = leader_schedule_cache
                .slot_leader_at(slot, Some(bank))
                .map(|leader| leader.id)
                .filter(|leader| leader != self_pubkey);
            if leader.is_none() {
                packet.meta_mut().set_discard(true);
            }
            Some((slot, leader))
        })
}

fn count_discards(packets: &[PacketBatch]) -> usize {
    packets
        .iter()
        .flat_map(|batch| batch.iter())
        .filter(|packet| packet.meta().discard())
        .count()
}

impl From<RecvTimeoutError> for ShredSigverifyError {
    fn from(err: RecvTimeoutError) -> Self {
        match err {
            RecvTimeoutError::Timeout => Self::RecvTimeout,
            RecvTimeoutError::Disconnected => Self::RecvDisconnected,
        }
    }
}

impl<T> From<SendError<T>> for ShredSigverifyError {
    fn from(_: SendError<T>) -> Self {
        Self::SendError
    }
}

struct ShredSigVerifyStats {
    since: Instant,
    num_iters: usize,
    num_batches: usize,
    num_packets: usize,
    num_deduper_saturations: usize,
    num_discards_post: usize,
    num_discards_pre: usize,
    num_duplicates: usize,
    num_invalid_retransmitter: AtomicUsize,
    num_retranmitter_signature_skipped: AtomicUsize,
    num_retranmitter_signature_verified: AtomicUsize,
    num_retransmit_stage_overflow_shreds: usize,
    num_retransmit_shreds: usize,
    /// This means the OutstandingRequests cache is saturated and we
    /// threw away a verified shred due to being unable to fetch the storage location
    num_unknown_block_location: usize,
    num_unknown_slot_leader: AtomicUsize,
    num_unknown_turbine_parent: AtomicUsize,
    elapsed_micros: u64,
    resign_micros: u64,
}

impl ShredSigVerifyStats {
    const METRICS_SUBMIT_CADENCE: Duration = Duration::from_secs(2);

    fn new(now: Instant) -> Self {
        Self {
            since: now,
            num_iters: 0usize,
            num_batches: 0usize,
            num_packets: 0usize,
            num_discards_pre: 0usize,
            num_deduper_saturations: 0usize,
            num_discards_post: 0usize,
            num_duplicates: 0usize,
            num_invalid_retransmitter: AtomicUsize::default(),
            num_retranmitter_signature_skipped: AtomicUsize::default(),
            num_retranmitter_signature_verified: AtomicUsize::default(),
            num_retransmit_stage_overflow_shreds: 0usize,
            num_retransmit_shreds: 0usize,
            num_unknown_block_location: 0usize,
            num_unknown_slot_leader: AtomicUsize::default(),
            num_unknown_turbine_parent: AtomicUsize::default(),
            elapsed_micros: 0u64,
            resign_micros: 0u64,
        }
    }

    fn maybe_submit(&mut self) {
        if self.since.elapsed() <= Self::METRICS_SUBMIT_CADENCE {
            return;
        }
        datapoint_info!(
            "shred_sigverify",
            ("num_iters", self.num_iters, i64),
            ("num_batches", self.num_batches, i64),
            ("num_packets", self.num_packets, i64),
            ("num_discards_pre", self.num_discards_pre, i64),
            ("num_deduper_saturations", self.num_deduper_saturations, i64),
            ("num_discards_post", self.num_discards_post, i64),
            ("num_duplicates", self.num_duplicates, i64),
            (
                "num_invalid_retransmitter",
                self.num_invalid_retransmitter.load(Ordering::Relaxed),
                i64
            ),
            (
                "num_retranmitter_signature_skipped",
                self.num_retranmitter_signature_skipped
                    .load(Ordering::Relaxed),
                i64
            ),
            (
                "num_retranmitter_signature_verified",
                self.num_retranmitter_signature_verified
                    .load(Ordering::Relaxed),
                i64
            ),
            (
                "num_retransmit_stage_overflow_shreds",
                self.num_retransmit_stage_overflow_shreds,
                i64
            ),
            ("num_retransmit_shreds", self.num_retransmit_shreds, i64),
            (
                "num_unknown_block_location",
                self.num_unknown_block_location,
                i64
            ),
            (
                "num_unknown_slot_leader",
                self.num_unknown_slot_leader.load(Ordering::Relaxed),
                i64
            ),
            (
                "num_unknown_turbine_parent",
                self.num_unknown_turbine_parent.load(Ordering::Relaxed),
                i64
            ),
            ("elapsed_micros", self.elapsed_micros, i64),
            ("resign_micros", self.resign_micros, i64),
        );
        *self = Self::new(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        rand::Rng,
        solana_entry::entry::{Entry, create_ticks},
        solana_gossip::contact_info::ContactInfo,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::{
            genesis_utils::create_genesis_config_with_leader,
            shred::{Nonce, ProcessShredsStats, ReedSolomonCache, Shredder},
        },
        solana_net_utils::SocketAddrSpace,
        solana_perf::packet::{Packet, PacketFlags, RecycledPacketBatch},
        solana_runtime::bank::Bank,
        solana_signer::Signer,
        solana_time_utils::timestamp,
        test_case::test_matrix,
    };

    #[test]
    fn test_sigverify_shreds_verify_batches() {
        let leader_keypair = Arc::new(Keypair::new());
        let wrong_keypair = Keypair::new();
        let leader_pubkey = leader_keypair.pubkey();
        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = LeaderScheduleCache::new_from_bank(&bank);
        let bank_forks = BankForks::new_rw_arc(bank);
        let batch_size = 2;
        let mut batch = RecycledPacketBatch::with_capacity(batch_size);
        batch.resize(batch_size, Packet::default());
        let mut batches = vec![batch];

        let entries = create_ticks(1, 1, Hash::new_unique());
        let shredder = Shredder::new(1, 0, 1, 0).unwrap();
        let (shreds_data, _shreds_code) = shredder.entries_to_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            true,
            Hash::new_unique(),
            0,
            0,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );
        let (shreds_data_wrong, _shreds_code_wrong) = shredder.entries_to_merkle_shreds_for_tests(
            &wrong_keypair,
            &entries,
            true,
            Hash::new_unique(),
            0,
            0,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );

        let shred = shreds_data[0].clone();
        batches[0][0].buffer_mut()[..shred.payload().len()].copy_from_slice(shred.payload());
        batches[0][0].meta_mut().size = shred.payload().len();

        let shred = shreds_data_wrong[0].clone();
        batches[0][1].buffer_mut()[..shred.payload().len()].copy_from_slice(shred.payload());
        batches[0][1].meta_mut().size = shred.payload().len();

        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
        let thread_pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        let working_bank = bank_forks.read().unwrap().working_bank();
        let mut batches = batches
            .into_iter()
            .map(PacketBatch::from)
            .collect::<Vec<_>>();
        let fresh_batches = batches.clone();
        verify_packets(
            Some(&thread_pool),
            &Pubkey::new_unique(), // self_pubkey
            &working_bank,
            &leader_schedule_cache,
            &mut batches,
            &cache,
        );
        assert!(!batches[0].get(0).unwrap().meta().discard());
        assert!(batches[0].get(1).unwrap().meta().discard());
        // Same answer on the calling thread.
        let mut batches = fresh_batches;
        verify_packets(
            None,
            &Pubkey::new_unique(), // self_pubkey
            &working_bank,
            &leader_schedule_cache,
            &mut batches,
            &RwLock::new(LruCache::new(/*capacity:*/ 128)),
        );
        assert!(!batches[0].get(0).unwrap().meta().discard());
        assert!(batches[0].get(1).unwrap().meta().discard());
    }

    // Serial processing (receive diet `serial_sigverify`) must give exactly the thread-pool
    // results: the same verified and retransmitted payloads (including resigned bytes) and the
    // same counters.
    #[test_matrix([true, false])]
    fn test_run_shred_sigverify_serial_matches_thread_pool(is_last_in_slot: bool) {
        let leader_keypair = Arc::new(Keypair::new());
        let leader_pubkey = leader_keypair.pubkey();
        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = LeaderScheduleCache::new_from_bank(&bank);
        let bank_forks = BankForks::new_rw_arc(bank);
        let root_bank = bank_forks.read().unwrap().root_bank();
        let node_keypair = Arc::new(Keypair::new());
        let cluster_info = ClusterInfo::new(
            ContactInfo::new_localhost(&node_keypair.pubkey(), timestamp()),
            node_keypair.clone(),
            SocketAddrSpace::Unspecified,
        );
        let make_shreds = |keypair: &Keypair| -> Vec<Vec<u8>> {
            Shredder::new(root_bank.slot(), root_bank.parent_slot(), 0, 0)
                .unwrap()
                .make_merkle_shreds_from_entries(
                    keypair,
                    &create_ticks(300, 1, Hash::default()),
                    is_last_in_slot,
                    Hash::default(), // chained_merkle_root
                    0,               // next_shred_index
                    0,               // next_code_index
                    &ReedSolomonCache::default(),
                    &mut ProcessShredsStats::default(),
                )
                .map(|shred| shred.payload().to_vec())
                .collect()
        };
        let valid = make_shreds(&leader_keypair);
        let invalid = make_shreds(&Keypair::new());
        assert!(valid.len() > 8);
        // Valid shreds, some sent twice, and a few signed by the wrong key.
        let payloads: Vec<&[u8]> = valid
            .iter()
            .chain(valid.iter().step_by(3))
            .chain(invalid.iter().take(4))
            .map(Vec::as_slice)
            .collect();
        let batches: Vec<PacketBatch> = payloads
            .chunks(5)
            .map(|chunk| {
                let mut batch = RecycledPacketBatch::with_capacity(chunk.len());
                batch.resize(chunk.len(), Packet::default());
                for (packet, payload) in batch.iter_mut().zip(chunk) {
                    packet.buffer_mut()[..payload.len()].copy_from_slice(payload);
                    packet.meta_mut().size = payload.len();
                }
                PacketBatch::from(batch)
            })
            .collect();
        let thread_pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        let run = |serial_max_packets: usize| {
            let (fetch_sender, fetch_receiver) = crossbeam_channel::unbounded();
            for batch in &batches {
                fetch_sender.send(batch.clone()).unwrap();
            }
            let (retransmit_sender, retransmit_receiver) = EvictingSender::new_bounded(1024);
            let (verified_sender, verified_receiver) = crossbeam_channel::unbounded();
            let deduper = Deduper::<2, [u8]>::new(&mut rand::rng(), /*num_bits:*/ 1 << 20);
            let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
            let cluster_nodes_cache = ClusterNodesCache::<RetransmitStage>::new(
                CLUSTER_NODES_CACHE_NUM_EPOCH_CAP,
                CLUSTER_NODES_CACHE_TTL,
            );
            let mut stats = ShredSigVerifyStats::new(Instant::now());
            let mut shred_buffer = Vec::new();
            assert!(
                run_shred_sigverify(
                    &thread_pool,
                    &node_keypair,
                    &cluster_info,
                    &bank_forks,
                    &leader_schedule_cache,
                    &deduper,
                    &fetch_receiver,
                    &retransmit_sender,
                    &verified_sender,
                    &cluster_nodes_cache,
                    &|_| None,
                    &cache,
                    &mut stats,
                    &mut shred_buffer,
                    serial_max_packets,
                    false, // tracing
                )
                .is_ok()
            );
            let verified: Vec<(Vec<u8>, bool)> = verified_receiver
                .try_iter()
                .flatten()
                .map(|(shred, repaired, _)| (shred.to_vec(), repaired))
                .collect();
            let retransmitted: Vec<Vec<u8>> = retransmit_receiver
                .try_iter()
                .flatten()
                .map(|shred| shred.to_vec())
                .collect();
            let counters = [
                stats.num_iters,
                stats.num_packets,
                stats.num_duplicates,
                stats.num_discards_post,
                stats.num_retransmit_shreds,
                stats.num_invalid_retransmitter.load(Ordering::Relaxed),
                stats
                    .num_retranmitter_signature_skipped
                    .load(Ordering::Relaxed),
            ];
            (verified, retransmitted, counters)
        };
        // On the thread pool, the order in which the two copies of a duplicate are seen (and so
        // the output order) is not deterministic, and two copies deduplicated concurrently can
        // both pass the Bloom filter. Compare the sets of payloads; on this thread the result is
        // exact: every valid shred once, every copy and every forged shred discarded.
        let unique = |payloads: &[Vec<u8>]| -> Vec<Vec<u8>> {
            let mut payloads = payloads.to_vec();
            payloads.sort_unstable();
            payloads.dedup();
            payloads
        };
        let num_duplicates = valid.len().div_ceil(3);
        let serial = run(payloads.len());
        assert_eq!(serial.0.len(), valid.len());
        assert!(serial.0.iter().all(|(_, repaired)| !repaired));
        let serial_verified: Vec<_> = serial.0.iter().map(|(shred, _)| shred.clone()).collect();
        assert_eq!(unique(&serial_verified).len(), valid.len());
        assert_eq!(serial.1, serial_verified);
        assert_eq!(
            serial.2,
            [
                1,
                payloads.len(),
                num_duplicates,
                num_duplicates + 4,
                valid.len(),
                0,
                serial.2[6],
            ]
        );
        assert_eq!(run(usize::MAX), serial);
        // Bound 0 (switch off) and a bound below the iteration size use the thread pool.
        for serial_max_packets in [0, payloads.len() - 1] {
            let pooled = run(serial_max_packets);
            let pooled_verified: Vec<_> = pooled.0.iter().map(|(shred, _)| shred.clone()).collect();
            assert_eq!(unique(&pooled_verified), unique(&serial_verified));
            assert_eq!(unique(&pooled.1), unique(&serial.1));
            let num_raced = pooled.0.len() - valid.len();
            assert_eq!(pooled.2[2] + num_raced, num_duplicates);
            // A copy that raced through is resigned (and counted) a second time.
            assert!((serial.2[6]..=serial.2[6] + num_raced).contains(&pooled.2[6]));
        }
    }

    #[test_matrix(
        [true, false],
        [true, false]
    )]
    fn test_maybe_verify_and_resign_packet(repaired: bool, is_last_in_slot: bool) {
        let mut rng = rand::rng();

        let leader_keypair = Arc::new(Keypair::new());
        let leader_pubkey = leader_keypair.pubkey();
        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = LeaderScheduleCache::new_from_bank(&bank);
        let bank_forks = BankForks::new_rw_arc(bank);
        let (working_bank, root_bank) = {
            let bank_forks = bank_forks.read().unwrap();
            (bank_forks.working_bank(), bank_forks.root_bank())
        };
        let chained_merkle_root = Hash::new_from_array(rng.random());

        let shredder = Shredder::new(root_bank.slot(), root_bank.parent_slot(), 0, 0).unwrap();
        let entries = vec![Entry::new(&Hash::default(), 0, vec![])];
        let mut shreds: Vec<_> = shredder
            .make_merkle_shreds_from_entries(
                &leader_keypair,
                &entries,
                is_last_in_slot,
                chained_merkle_root,
                0,
                0,
                &ReedSolomonCache::default(),
                &mut ProcessShredsStats::default(),
            )
            .collect();

        let cluster_info = ClusterInfo::new(
            ContactInfo::new_localhost(&leader_pubkey, timestamp()),
            leader_keypair,
            SocketAddrSpace::Unspecified,
        );

        let cluster_nodes_cache = ClusterNodesCache::<RetransmitStage>::new(
            CLUSTER_NODES_CACHE_NUM_EPOCH_CAP,
            CLUSTER_NODES_CACHE_TTL,
        );
        let stats = ShredSigVerifyStats::new(Instant::now());

        for shred in shreds.iter_mut() {
            let keypair = Keypair::new();
            let nonce = repaired.then(|| rng.random::<Nonce>());
            if is_last_in_slot {
                let packet = &mut shred.payload().to_packet(nonce);
                let buf_before = packet.buffer_mut().to_vec();
                if repaired {
                    packet.meta_mut().flags |= PacketFlags::REPAIR;
                }
                maybe_verify_and_resign_packet(
                    &mut packet.into(),
                    &root_bank,
                    &working_bank,
                    &cluster_info,
                    &leader_schedule_cache,
                    &cluster_nodes_cache,
                    &stats,
                    &keypair,
                )
                .expect("packet should pass the verification");
                assert!(!packet.meta().discard());

                // Check whether the packet was modified.
                assert_ne!(&buf_before, &packet.data(..).unwrap());

                let mut bytes_packet = shred.payload().to_bytes_packet(nonce);
                if repaired {
                    bytes_packet.meta_mut().flags |= PacketFlags::REPAIR;
                }
                let buf_addr = bytes_packet.buffer().as_ptr().addr();
                maybe_verify_and_resign_packet(
                    &mut bytes_packet.as_mut(),
                    &root_bank,
                    &working_bank,
                    &cluster_info,
                    &leader_schedule_cache,
                    &cluster_nodes_cache,
                    &stats,
                    &keypair,
                )
                .expect("packet should pass the verification");
                assert!(!bytes_packet.meta().discard());

                // Check whether the packet was modified.
                let buf_addr_after = bytes_packet.buffer().as_ptr().addr();
                assert_ne!(buf_addr, buf_addr_after);
            } else {
                let packet = &mut shred.payload().to_packet(nonce);
                if repaired {
                    packet.meta_mut().flags |= PacketFlags::REPAIR;
                }
                maybe_verify_and_resign_packet(
                    &mut packet.into(),
                    &root_bank,
                    &working_bank,
                    &cluster_info,
                    &leader_schedule_cache,
                    &cluster_nodes_cache,
                    &stats,
                    &keypair,
                )
                .expect("packet should pass the verification");
                assert!(!packet.meta().discard());

                let mut bytes_packet = shred.payload().to_bytes_packet(nonce);
                if repaired {
                    bytes_packet.meta_mut().flags |= PacketFlags::REPAIR;
                }
                let buf_addr = bytes_packet.buffer().as_ptr().addr();
                maybe_verify_and_resign_packet(
                    &mut bytes_packet.as_mut(),
                    &root_bank,
                    &working_bank,
                    &cluster_info,
                    &leader_schedule_cache,
                    &cluster_nodes_cache,
                    &stats,
                    &keypair,
                )
                .expect("packet should pass the verification");
                assert!(!packet.meta().discard());

                // Packet should not be modified.
                let buf_addr_after = bytes_packet.buffer().as_ptr().addr();
                assert_eq!(buf_addr, buf_addr_after);
            }
        }
    }
}
