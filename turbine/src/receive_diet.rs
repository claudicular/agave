//! Optional, exact latency trims for the shred receive path of a non-voting, unstaked node
//! (TVU fetch -> shred sigverify -> window insert -> completed data sets), plus an optional
//! per-stage tracer that timestamps every data set on its way through that path.
//!
//! Every switch defaults to off. None of them changes what is verified, stored, recovered,
//! notified or retransmitted; they only remove thread hand-offs and CPU work whose result is
//! already known, so they can be flipped at any time, also while the node runs.
//!
//! Configuration: `SOLANA_RECEIVE_DIET` is read at startup. If `SOLANA_RECEIVE_DIET_FILE` names
//! a file, a background thread re-reads it once per second and its content replaces the whole
//! setting (removing a word turns that switch off; a missing or empty file turns everything off).
//! Both use the same syntax, a comma or whitespace separated list of:
//!
//! - `serial_sigverify[=N]`: shred sigverify deduplicates, verifies and resigns on its own thread,
//!   without the three rayon hand-offs per iteration, when an iteration holds at most `N` packets
//!   (default [`DEFAULT_SERIAL_SIGVERIFY_MAX_PACKETS`]); larger bursts keep the thread pool. The
//!   resign pass is also skipped when no packet of the iteration is of a resigned variant.
//! - `leaf_retransmit`: the retransmit stage answers "no children" in O(1) when this node is
//!   provably a leaf of the turbine tree for the slot (see
//!   `ClusterNodes::get_retransmit_addrs_if_leaf`) instead of shuffling the whole cluster for every
//!   shred, and then processes the batch on its own thread. The output is identical: root distance
//!   2 and no addresses.
//! - `skip_empty_data_sets`: window insert sends a completed-data-sets message only when the insert
//!   completed at least one data set. Today most inserts complete none, and every empty message
//!   wakes `CompletedDataSetsService` and makes it emit a datapoint.
//! - `trace`: timestamp every data-complete shred and completed data set at each stage and append
//!   the rows to the CSV named by `SOLANA_RECEIVE_TRACE_FILE` (see [`trace`]). Without that
//!   variable the switch does nothing.
//! - `all`: the first three switches.

use {
    crossbeam_channel::{Sender, TrySendError, bounded},
    solana_clock::Slot,
    solana_ledger::shred::{self, ShredFlags},
    std::{
        fs::{File, OpenOptions},
        io::{BufWriter, Write},
        sync::{
            OnceLock,
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        },
        thread::Builder,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    },
};

pub const RECEIVE_DIET_ENV: &str = "SOLANA_RECEIVE_DIET";
pub const RECEIVE_DIET_FILE_ENV: &str = "SOLANA_RECEIVE_DIET_FILE";
pub const RECEIVE_TRACE_FILE_ENV: &str = "SOLANA_RECEIVE_TRACE_FILE";

/// Default packet bound of `serial_sigverify`. With the TVU receive coalesce disabled an
/// iteration holds ~2-3 packets; a whole FEC set (32 data + 32 coding shreds) arriving at once
/// still goes to the thread pool.
pub const DEFAULT_SERIAL_SIGVERIFY_MAX_PACKETS: usize = 32;

const CONTROL_FILE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const TRACE_CHANNEL_CAPACITY: usize = 1 << 16;
const TRACE_FLUSH_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReceiveDietConfig {
    /// 0 = off.
    pub serial_sigverify_max_packets: usize,
    pub leaf_retransmit: bool,
    pub skip_empty_data_sets: bool,
    pub trace: bool,
}

impl ReceiveDietConfig {
    pub fn parse(value: &str) -> Result<Self, String> {
        let mut config = Self::default();
        for word in value
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|word| !word.is_empty())
        {
            let (key, arg) = match word.split_once('=') {
                Some((key, arg)) => (key, Some(arg)),
                None => (word, None),
            };
            match (key.to_ascii_lowercase().as_str(), arg) {
                ("serial_sigverify", None) => {
                    config.serial_sigverify_max_packets = DEFAULT_SERIAL_SIGVERIFY_MAX_PACKETS
                }
                ("serial_sigverify", Some(arg)) => {
                    config.serial_sigverify_max_packets = arg
                        .parse()
                        .map_err(|_| format!("invalid serial_sigverify bound: {arg:?}"))?
                }
                ("leaf_retransmit", None) => config.leaf_retransmit = true,
                ("skip_empty_data_sets", None) => config.skip_empty_data_sets = true,
                ("trace", None) => config.trace = true,
                ("all", None) => {
                    config.serial_sigverify_max_packets = DEFAULT_SERIAL_SIGVERIFY_MAX_PACKETS;
                    config.leaf_retransmit = true;
                    config.skip_empty_data_sets = true;
                }
                _ => return Err(format!("unknown receive diet switch: {word:?}")),
            }
        }
        Ok(config)
    }
}

#[derive(Debug, Default)]
pub struct ReceiveDiet {
    serial_sigverify_max_packets: AtomicUsize,
    leaf_retransmit: AtomicBool,
    skip_empty_data_sets: AtomicBool,
    trace: AtomicBool,
}

impl ReceiveDiet {
    pub fn new(config: ReceiveDietConfig) -> Self {
        let diet = Self::default();
        diet.store(&config);
        diet
    }

    pub fn store(&self, config: &ReceiveDietConfig) {
        self.serial_sigverify_max_packets
            .store(config.serial_sigverify_max_packets, Ordering::Relaxed);
        self.leaf_retransmit
            .store(config.leaf_retransmit, Ordering::Relaxed);
        self.skip_empty_data_sets
            .store(config.skip_empty_data_sets, Ordering::Relaxed);
        self.trace.store(config.trace, Ordering::Relaxed);
    }

    pub fn config(&self) -> ReceiveDietConfig {
        ReceiveDietConfig {
            serial_sigverify_max_packets: self.serial_sigverify_max_packets(),
            leaf_retransmit: self.leaf_retransmit(),
            skip_empty_data_sets: self.skip_empty_data_sets(),
            trace: self.trace.load(Ordering::Relaxed),
        }
    }

    /// Largest iteration (in packets) that shred sigverify processes without the thread pool;
    /// 0 when the switch is off.
    #[inline]
    pub fn serial_sigverify_max_packets(&self) -> usize {
        self.serial_sigverify_max_packets.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn leaf_retransmit(&self) -> bool {
        self.leaf_retransmit.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn skip_empty_data_sets(&self) -> bool {
        self.skip_empty_data_sets.load(Ordering::Relaxed)
    }
}

static RECEIVE_DIET: OnceLock<ReceiveDiet> = OnceLock::new();

/// The process-wide switches, initialized from the environment on first use.
#[inline]
pub fn receive_diet() -> &'static ReceiveDiet {
    if let Some(diet) = RECEIVE_DIET.get() {
        return diet;
    }
    let mut initialized_here = false;
    let diet = RECEIVE_DIET.get_or_init(|| {
        initialized_here = true;
        let config = match std::env::var(RECEIVE_DIET_ENV) {
            Ok(value) => ReceiveDietConfig::parse(&value).unwrap_or_else(|err| {
                error!("{RECEIVE_DIET_ENV}: {err}; all receive diet switches stay off");
                ReceiveDietConfig::default()
            }),
            Err(_) => ReceiveDietConfig::default(),
        };
        info!("receive diet: {config:?} ({RECEIVE_DIET_ENV})");
        ReceiveDiet::new(config)
    });
    if initialized_here && let Some(path) = std::env::var_os(RECEIVE_DIET_FILE_ENV) {
        spawn_control_file_watcher(diet, path.into());
    }
    diet
}

fn read_control_file(path: &std::path::Path) -> Result<ReceiveDietConfig, String> {
    match std::fs::read_to_string(path) {
        Ok(content) => ReceiveDietConfig::parse(&content),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ReceiveDietConfig::default()),
        Err(err) => Err(err.to_string()),
    }
}

fn spawn_control_file_watcher(diet: &'static ReceiveDiet, path: std::path::PathBuf) {
    let spawned = Builder::new()
        .name("solRecvDietCtl".to_string())
        .spawn(move || {
            let mut last_error = None;
            loop {
                match read_control_file(&path) {
                    Ok(config) => {
                        last_error = None;
                        if config != diet.config() {
                            info!("receive diet: {config:?} (from {})", path.display());
                            diet.store(&config);
                        }
                    }
                    Err(err) => {
                        // Keep the current setting; log each distinct error once.
                        if last_error.as_ref() != Some(&err) {
                            error!("receive diet control file {}: {err}", path.display());
                            last_error = Some(err);
                        }
                    }
                }
                std::thread::sleep(CONTROL_FILE_POLL_INTERVAL);
            }
        });
    if let Err(err) = spawned {
        error!("receive diet: failed to start the control file watcher: {err}");
    }
}

/// Points on the receive path at which [`trace`] timestamps a data set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TraceStage {
    /// `solTvuPktMod` dequeued the packet batch holding a data-complete shred.
    FetchDequeued = 1,
    /// `solShredVerifr` dequeued it.
    SigverifyDequeued = 2,
    /// `solShredVerifr` handed it to window insert.
    SigverifySent = 3,
    /// `solWinInsert` dequeued it.
    InsertDequeued = 4,
    /// An insert (receipt or recovery) completed the data set ending at this shred.
    DataSetCompleted = 5,
    /// `solComplDataSet` dequeued the completed data set.
    DataSetDequeued = 6,
    /// `solComplDataSet` read and deserialized its entries.
    DataSetRead = 7,
    /// `solComplDataSet` finished the deshred notifications of its transactions.
    DataSetNotified = 8,
}

impl TraceStage {
    pub fn name(self) -> &'static str {
        match self {
            Self::FetchDequeued => "fetch_dequeued",
            Self::SigverifyDequeued => "sigverify_dequeued",
            Self::SigverifySent => "sigverify_sent",
            Self::InsertDequeued => "insert_dequeued",
            Self::DataSetCompleted => "data_set_completed",
            Self::DataSetDequeued => "data_set_dequeued",
            Self::DataSetRead => "data_set_read",
            Self::DataSetNotified => "data_set_notified",
        }
    }
}

struct TraceRecord {
    stage: TraceStage,
    slot: Slot,
    index: u32,
    unix_ns: u64,
}

struct Tracer {
    sender: Sender<TraceRecord>,
    dropped: AtomicU64,
}

fn tracer() -> Option<&'static Tracer> {
    static TRACER: OnceLock<Option<Tracer>> = OnceLock::new();
    TRACER
        .get_or_init(|| {
            let path = std::env::var_os(RECEIVE_TRACE_FILE_ENV)?;
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .inspect_err(|err| {
                    error!("receive trace: cannot open {path:?}: {err}");
                })
                .ok()?;
            let (sender, receiver) = bounded(TRACE_CHANNEL_CAPACITY);
            Builder::new()
                .name("solRecvTrace".to_string())
                .spawn(move || write_trace(file, receiver))
                .inspect_err(|err| error!("receive trace: failed to start the writer: {err}"))
                .ok()?;
            info!("receive trace: writing to {path:?}");
            Some(Tracer {
                sender,
                dropped: AtomicU64::default(),
            })
        })
        .as_ref()
}

fn write_trace(file: File, receiver: crossbeam_channel::Receiver<TraceRecord>) {
    let mut writer = BufWriter::new(file);
    let mut last_flush = Instant::now();
    loop {
        match receiver.recv_timeout(TRACE_FLUSH_INTERVAL) {
            Ok(record) => {
                let _ = writeln!(
                    writer,
                    "{},{},{},{}",
                    record.stage.name(),
                    record.slot,
                    record.index,
                    record.unix_ns
                );
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => (),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
        if last_flush.elapsed() >= TRACE_FLUSH_INTERVAL {
            let _ = writer.flush();
            last_flush = Instant::now();
        }
    }
    let _ = writer.flush();
}

/// True if the `trace` switch is on and `SOLANA_RECEIVE_TRACE_FILE` names a writable file.
#[inline]
pub fn trace_enabled() -> bool {
    receive_diet().trace.load(Ordering::Relaxed) && tracer().is_some()
}

/// Records `(stage, slot, index, wall-clock ns)`. `index` is the index of the data shred that
/// ends the data set (the data-complete shred), so rows join across stages, with a packet capture
/// of the TVU port and with geyser deshred timestamps on the same clock. Never blocks: rows are
/// dropped when the writer falls behind.
pub fn trace(stage: TraceStage, slot: Slot, index: u32) {
    let Some(tracer) = tracer() else {
        return;
    };
    let unix_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or_default();
    let record = TraceRecord {
        stage,
        slot,
        index,
        unix_ns,
    };
    if let Err(TrySendError::Full(_)) = tracer.sender.try_send(record) {
        let dropped = tracer.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        if dropped.is_power_of_two() {
            warn!("receive trace: {dropped} rows dropped");
        }
    }
}

/// Traces `shred` if it is a data shred that ends a data set.
#[inline]
pub fn trace_shred(stage: TraceStage, shred: &[u8]) {
    let Ok(flags) = shred::wire::get_flags(shred) else {
        return;
    };
    if !flags.contains(ShredFlags::DATA_COMPLETE_SHRED) {
        return;
    }
    if let (Some(slot), Some(index)) = (shred::wire::get_slot(shred), shred::wire::get_index(shred))
    {
        trace(stage, slot, index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_receive_diet_config() {
        assert_eq!(
            ReceiveDietConfig::parse("").unwrap(),
            ReceiveDietConfig::default()
        );
        assert_eq!(
            ReceiveDietConfig::parse(" \n").unwrap(),
            ReceiveDietConfig::default()
        );
        assert_eq!(
            ReceiveDietConfig::parse("serial_sigverify").unwrap(),
            ReceiveDietConfig {
                serial_sigverify_max_packets: DEFAULT_SERIAL_SIGVERIFY_MAX_PACKETS,
                ..ReceiveDietConfig::default()
            }
        );
        assert_eq!(
            ReceiveDietConfig::parse("serial_sigverify=8, leaf_retransmit\nskip_empty_data_sets")
                .unwrap(),
            ReceiveDietConfig {
                serial_sigverify_max_packets: 8,
                leaf_retransmit: true,
                skip_empty_data_sets: true,
                trace: false,
            }
        );
        assert_eq!(
            ReceiveDietConfig::parse("ALL,trace").unwrap(),
            ReceiveDietConfig {
                serial_sigverify_max_packets: DEFAULT_SERIAL_SIGVERIFY_MAX_PACKETS,
                leaf_retransmit: true,
                skip_empty_data_sets: true,
                trace: true,
            }
        );
        assert_eq!(
            ReceiveDietConfig::parse("serial_sigverify=0").unwrap(),
            ReceiveDietConfig::default()
        );
        assert!(ReceiveDietConfig::parse("serial_sigverify=x").is_err());
        assert!(ReceiveDietConfig::parse("leaf_retransmit=1").is_err());
        assert!(ReceiveDietConfig::parse("no_such_switch").is_err());
    }

    #[test]
    fn test_receive_diet_store() {
        let diet = ReceiveDiet::default();
        assert_eq!(diet.config(), ReceiveDietConfig::default());
        let config = ReceiveDietConfig::parse("all").unwrap();
        diet.store(&config);
        assert_eq!(diet.config(), config);
        assert_eq!(
            diet.serial_sigverify_max_packets(),
            DEFAULT_SERIAL_SIGVERIFY_MAX_PACKETS
        );
        assert!(diet.leaf_retransmit());
        assert!(diet.skip_empty_data_sets());
        diet.store(&ReceiveDietConfig::default());
        assert_eq!(diet.config(), ReceiveDietConfig::default());
    }

    #[test]
    fn test_read_control_file() {
        let dir = std::env::temp_dir().join(format!(
            "receive-diet-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("control");
        // A missing file turns everything off.
        assert_eq!(
            read_control_file(&path).unwrap(),
            ReceiveDietConfig::default()
        );
        std::fs::write(&path, "leaf_retransmit\n").unwrap();
        assert_eq!(
            read_control_file(&path).unwrap(),
            ReceiveDietConfig {
                leaf_retransmit: true,
                ..ReceiveDietConfig::default()
            }
        );
        std::fs::write(&path, "bogus").unwrap();
        assert!(read_control_file(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
