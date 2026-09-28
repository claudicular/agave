//! Multi-version account overlay for one run (one child slot over one frozen parent).
//!
//! Every account has a lazily read base value (the parent's) and a list of versions
//! written by transactions of the run, sorted by transaction index. A read by transaction
//! `k` sees the latest version written by a transaction `< k`, final or not, else the base.
//! One exception ("program-read finality"): if that version is not final and either it or
//! the value it would replace is owned by a program loader, the read falls back to the
//! latest *final* version (or base). This keeps program-cache entries, which the SVM
//! inserts into the fast lane's shared cache while loading programs, built only from final
//! state, so a speculative incarnation can never leave a program version behind that final
//! state would not produce.
//!
//! Values are stored exactly as the SVM produced them. The zero-lamport rule of
//! accounts-db (`AccountsDb::load` filters zero-lamport accounts) is applied at read time:
//! a version with 0 lamports reads as absent.

use {
    parking_lot::Mutex,
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, loader_v4},
    std::sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

pub type TxIdx = u32;

/// Where a read value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Base,
    /// Written by transaction `tx`, incarnation `inc`.
    Ver(TxIdx, u32),
}

/// One recorded read of an incarnation.
#[derive(Debug, Clone)]
pub struct Read {
    pub key: Pubkey,
    pub origin: Origin,
    /// The value as returned to the reader (`None` = absent or zero lamports).
    pub value: Option<AccountSharedData>,
}

#[derive(Debug, Clone)]
struct Version {
    tx: TxIdx,
    inc: u32,
    account: AccountSharedData,
    final_: bool,
}

/// Reads the parent's state. Implemented for `Arc<Bank>` and for test/corpus bases.
pub trait BaseReader: Send + Sync {
    fn read(&self, key: &Pubkey) -> Option<(AccountSharedData, Slot)>;
}

#[derive(Default)]
struct AcctEntry {
    base: OnceLock<Option<(AccountSharedData, Slot)>>,
    versions: Mutex<Vec<Version>>,
}

pub fn is_loader_owned(account: &AccountSharedData) -> bool {
    let owner = account.owner();
    bpf_loader_upgradeable::check_id(owner)
        || loader_v4::check_id(owner)
        || bpf_loader::check_id(owner)
        || bpf_loader_deprecated::check_id(owner)
}

fn visible(account: &AccountSharedData) -> Option<AccountSharedData> {
    (account.lamports() != 0).then(|| account.clone())
}

/// Value equality as the SVM observes it (all fields, byte-for-byte data).
pub fn same_value(a: &Option<AccountSharedData>, b: &Option<AccountSharedData>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => accounts_equal(a, b),
        _ => false,
    }
}

pub fn accounts_equal(a: &AccountSharedData, b: &AccountSharedData) -> bool {
    a.lamports() == b.lamports()
        && a.owner() == b.owner()
        && a.executable() == b.executable()
        && a.rent_epoch() == b.rent_epoch()
        && (std::ptr::eq(a.data().as_ptr(), b.data().as_ptr()) && a.data().len() == b.data().len()
            || a.data() == b.data())
}

pub struct Overlay {
    slot: Slot,
    base: Arc<dyn BaseReader>,
    accounts: dashmap::DashMap<Pubkey, Arc<AcctEntry>>,
    /// Account data held (versions and cached base reads, plus per-account overhead); also
    /// added to the process-wide [`crate::mem::OVERLAY_BYTES`] and released on drop.
    bytes: std::sync::atomic::AtomicI64,
    base_reads: AtomicUsize,
}

impl Drop for Overlay {
    fn drop(&mut self) {
        crate::mem::OVERLAY_BYTES.sub(self.bytes.load(Ordering::Relaxed));
    }
}

/// What a reader would see for `key` below `tx`.
#[derive(Debug, Clone)]
pub struct Visible {
    pub origin: Origin,
    /// Value as returned to the reader (zero-lamport = None).
    pub value: Option<AccountSharedData>,
    /// Slot tag returned to the SVM (informational).
    pub slot: Slot,
}

impl Overlay {
    pub fn new(slot: Slot, base: Arc<dyn BaseReader>) -> Self {
        Self {
            slot,
            base,
            accounts: dashmap::DashMap::new(),
            bytes: std::sync::atomic::AtomicI64::new(0),
            base_reads: AtomicUsize::new(0),
        }
    }

    pub fn slot(&self) -> Slot {
        self.slot
    }

    /// Approximate bytes of account data held (versions and cached base reads).
    pub fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed).max(0) as usize
    }

    fn account_bytes(&self, delta: i64) {
        self.bytes.fetch_add(delta, Ordering::Relaxed);
        crate::mem::OVERLAY_BYTES.add(delta);
    }

    pub fn base_reads(&self) -> usize {
        self.base_reads.load(Ordering::Relaxed)
    }

    fn entry(&self, key: &Pubkey) -> Arc<AcctEntry> {
        if let Some(entry) = self.accounts.get(key) {
            return Arc::clone(entry.value());
        }
        Arc::clone(self.accounts.entry(*key).or_default().value())
    }

    fn base_of(&self, entry: &AcctEntry, key: &Pubkey) -> Option<(AccountSharedData, Slot)> {
        entry
            .base
            .get_or_init(|| {
                self.base_reads.fetch_add(1, Ordering::Relaxed);
                let value = self.base.read(key);
                self.account_bytes(
                    value
                        .as_ref()
                        .map(|(a, _)| crate::mem::account_bytes(a))
                        .unwrap_or(crate::mem::ACCOUNT_OVERHEAD),
                );
                value
            })
            .clone()
    }

    fn base_visible(&self, entry: &AcctEntry, key: &Pubkey) -> Visible {
        match self.base_of(entry, key) {
            Some((account, slot)) => Visible {
                origin: Origin::Base,
                value: visible(&account),
                slot,
            },
            None => Visible {
                origin: Origin::Base,
                value: None,
                slot: 0,
            },
        }
    }

    /// The value transaction `tx` reads for `key` (see the module docs for the rule).
    pub fn visible_below(&self, key: &Pubkey, tx: TxIdx) -> Visible {
        enum Pick {
            Base,
            Version(TxIdx, u32, AccountSharedData),
            NonFinal {
                latest: (TxIdx, u32, AccountSharedData),
                fallback: Option<(TxIdx, u32, AccountSharedData)>,
                nonfinal_loader: bool,
            },
        }
        let entry = self.entry(key);
        let pick = {
            let versions = entry.versions.lock();
            let below = versions.partition_point(|v| v.tx < tx);
            if below == 0 {
                Pick::Base
            } else {
                let latest = &versions[below - 1];
                if latest.final_ {
                    Pick::Version(latest.tx, latest.inc, latest.account.clone())
                } else {
                    let final_pos = versions[..below].iter().rposition(|v| v.final_);
                    let nonfinal_start = final_pos.map(|p| p + 1).unwrap_or(0);
                    Pick::NonFinal {
                        latest: (latest.tx, latest.inc, latest.account.clone()),
                        fallback: final_pos.map(|p| {
                            let v = &versions[p];
                            (v.tx, v.inc, v.account.clone())
                        }),
                        nonfinal_loader: versions[nonfinal_start..below]
                            .iter()
                            .any(|v| is_loader_owned(&v.account)),
                    }
                }
            }
        };
        let ver = |tx: TxIdx, inc: u32, account: &AccountSharedData| Visible {
            origin: Origin::Ver(tx, inc),
            value: visible(account),
            slot: self.slot,
        };
        match pick {
            Pick::Base => self.base_visible(&entry, key),
            Pick::Version(tx, inc, account) => ver(tx, inc, &account),
            Pick::NonFinal {
                latest,
                fallback,
                nonfinal_loader,
            } => {
                // `latest` is not final: fall back to the final state below it if a
                // loader-owned value is involved on either side.
                let (fallback_visible, fallback_is_loader) = match fallback {
                    Some((tx, inc, account)) => {
                        let is_loader = is_loader_owned(&account);
                        (ver(tx, inc, &account), is_loader)
                    }
                    None => {
                        let is_loader = self
                            .base_of(&entry, key)
                            .map(|(account, _)| is_loader_owned(&account))
                            .unwrap_or(false);
                        (self.base_visible(&entry, key), is_loader)
                    }
                };
                if nonfinal_loader || fallback_is_loader {
                    fallback_visible
                } else {
                    ver(latest.0, latest.1, &latest.2)
                }
            }
        }
    }

    /// The final value below `tx`, assuming every writer below `tx` is final (validation).
    /// Non-final versions below `tx` are ignored.
    pub fn final_below(&self, key: &Pubkey, tx: TxIdx) -> Visible {
        let entry = self.entry(key);
        let chosen = {
            let versions = entry.versions.lock();
            let below = versions.partition_point(|v| v.tx < tx);
            versions[..below]
                .iter()
                .rev()
                .find(|v| v.final_)
                .map(|v| (v.tx, v.inc, v.account.clone()))
        };
        match chosen {
            Some((tx, inc, account)) => Visible {
                origin: Origin::Ver(tx, inc),
                value: visible(&account),
                slot: self.slot,
            },
            None => self.base_visible(&entry, key),
        }
    }

    /// Install incarnation `inc` of `tx`: its `writes` replace any version of `tx`, and
    /// versions of `tx` for keys in `prev_keys` but not in `writes` are removed.
    pub fn install(
        &self,
        tx: TxIdx,
        inc: u32,
        writes: &[(Pubkey, AccountSharedData)],
        prev_keys: &[Pubkey],
    ) {
        for (key, account) in writes {
            let entry = self.entry(key);
            let mut versions = entry.versions.lock();
            let pos = versions.partition_point(|v| v.tx < tx);
            let version = Version {
                tx,
                inc,
                account: account.clone(),
                final_: false,
            };
            if pos < versions.len() && versions[pos].tx == tx {
                self.account_bytes(
                    crate::mem::account_bytes(account)
                        - crate::mem::account_bytes(&versions[pos].account),
                );
                versions[pos] = version;
            } else {
                self.account_bytes(crate::mem::account_bytes(account));
                versions.insert(pos, version);
            }
        }
        for key in prev_keys {
            if writes.iter().any(|(k, _)| k == key) {
                continue;
            }
            self.remove_version(key, tx);
        }
    }

    /// Remove `tx`'s version of `key`, if any.
    pub fn remove_version(&self, key: &Pubkey, tx: TxIdx) {
        let entry = self.entry(key);
        let mut versions = entry.versions.lock();
        let pos = versions.partition_point(|v| v.tx < tx);
        if pos < versions.len() && versions[pos].tx == tx {
            let removed = versions.remove(pos);
            self.account_bytes(-crate::mem::account_bytes(&removed.account));
        }
    }

    /// Mark `tx`'s versions of `keys` final.
    pub fn mark_final(&self, tx: TxIdx, keys: &[Pubkey]) {
        for key in keys {
            let entry = self.entry(key);
            let mut versions = entry.versions.lock();
            let pos = versions.partition_point(|v| v.tx < tx);
            if pos < versions.len() && versions[pos].tx == tx {
                versions[pos].final_ = true;
            }
        }
    }

    /// The latest version of `key` (any writer), or base: the account's state after every
    /// executed transaction of the run (used by the slot-level check).
    pub fn latest(&self, key: &Pubkey) -> Visible {
        self.visible_below(key, TxIdx::MAX)
    }

    /// The raw latest stored value of `key` (zero-lamport accounts included), if any
    /// version exists.
    pub fn latest_raw(&self, key: &Pubkey) -> Option<AccountSharedData> {
        let entry = self.accounts.get(key)?;
        let versions = entry.value().versions.lock();
        versions.last().map(|v| v.account.clone())
    }

    /// Keys that have at least one version.
    pub fn written_keys(&self) -> Vec<Pubkey> {
        self.accounts
            .iter()
            .filter(|e| !e.value().versions.lock().is_empty())
            .map(|e| *e.key())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::collections::HashMap};

    pub struct MapBase(pub HashMap<Pubkey, AccountSharedData>);
    impl BaseReader for MapBase {
        fn read(&self, key: &Pubkey) -> Option<(AccountSharedData, Slot)> {
            self.0.get(key).cloned().map(|a| (a, 7))
        }
    }

    fn acct(lamports: u64, byte: u8) -> AccountSharedData {
        let mut a = AccountSharedData::new(lamports, 4, &Pubkey::new_from_array([9; 32]));
        a.data_as_mut_slice().fill(byte);
        a
    }

    use solana_account::WritableAccount;

    fn acct_len(lamports: u64, len: usize) -> AccountSharedData {
        AccountSharedData::new(lamports, len, &Pubkey::new_from_array([9; 32]))
    }

    #[test]
    fn test_versions_and_visibility() {
        let key = Pubkey::new_unique();
        let base = MapBase([(key, acct(10, 0))].into_iter().collect());
        let ov = Overlay::new(8, Arc::new(base));
        assert_eq!(ov.visible_below(&key, 5).origin, Origin::Base);
        ov.install(3, 0, &[(key, acct(11, 3))], &[]);
        ov.install(6, 0, &[(key, acct(12, 6))], &[]);
        let v = ov.visible_below(&key, 5);
        assert_eq!(v.origin, Origin::Ver(3, 0));
        assert_eq!(v.value.unwrap().lamports(), 11);
        assert_eq!(ov.visible_below(&key, 3).origin, Origin::Base);
        assert_eq!(ov.visible_below(&key, 7).origin, Origin::Ver(6, 0));
        // Re-incarnation replaces.
        ov.install(3, 1, &[(key, acct(13, 3))], &[key]);
        assert_eq!(ov.visible_below(&key, 5).origin, Origin::Ver(3, 1));
        // Incarnation without the write removes it.
        ov.install(3, 2, &[], &[key]);
        assert_eq!(ov.visible_below(&key, 5).origin, Origin::Base);
        // final_below ignores non-final.
        assert_eq!(ov.final_below(&key, 7).origin, Origin::Base);
        ov.mark_final(6, &[key]);
        assert_eq!(ov.final_below(&key, 7).origin, Origin::Ver(6, 0));
    }

    #[test]
    fn test_zero_lamports_read_as_absent() {
        let key = Pubkey::new_unique();
        let ov = Overlay::new(8, Arc::new(MapBase(HashMap::new())));
        assert!(ov.visible_below(&key, 1).value.is_none());
        ov.install(1, 0, &[(key, acct(5, 1))], &[]);
        assert!(ov.visible_below(&key, 2).value.is_some());
        ov.install(2, 0, &[(key, acct(0, 0))], &[]);
        let v = ov.visible_below(&key, 3);
        assert_eq!(v.origin, Origin::Ver(2, 0));
        assert!(v.value.is_none());
    }

    #[test]
    fn test_program_read_finality() {
        let key = Pubkey::new_unique();
        let mut programdata = acct(100, 1);
        programdata.set_owner(bpf_loader_upgradeable::id());
        let base = MapBase([(key, programdata.clone())].into_iter().collect());
        let ov = Overlay::new(8, Arc::new(base));
        let mut upgraded = programdata.clone();
        upgraded.data_as_mut_slice().fill(2);
        ov.install(2, 0, &[(key, upgraded)], &[]);
        // Non-final loader-owned version: reader falls back to base.
        assert_eq!(ov.visible_below(&key, 5).origin, Origin::Base);
        ov.mark_final(2, &[key]);
        assert_eq!(ov.visible_below(&key, 5).origin, Origin::Ver(2, 0));
        // A non-final close (0 lamports, system-owned) over a loader-owned final value
        // also falls back.
        ov.install(3, 0, &[(key, AccountSharedData::default())], &[]);
        assert_eq!(ov.visible_below(&key, 5).origin, Origin::Ver(2, 0));
    }

    #[test]
    fn test_bytes_accounting() {
        let key = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let base = HashMap::from([(other, acct_len(7, 1000))]);
        let ov = Overlay::new(2, Arc::new(MapBase(base)));
        assert_eq!(ov.bytes(), 0);
        ov.install(1, 0, &[(key, acct_len(1, 100))], &[]);
        assert_eq!(ov.bytes(), 228);
        // Re-install with a bigger account replaces the version's bytes.
        ov.install(1, 1, &[(key, acct_len(1, 300))], &[key]);
        assert_eq!(ov.bytes(), 428);
        // A base read of a 1000-byte account is held too.
        let _ = ov.visible_below(&other, 5);
        assert_eq!(ov.bytes(), 428 + 1128);
        // Removing the version releases it.
        ov.install(1, 2, &[], &[key]);
        assert_eq!(ov.bytes(), 1128);
    }
}
