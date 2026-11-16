//! Frozen-bank cache and the fast lane's fork graph.

use {
    log::warn,
    solana_clock::{BankId, Slot},
    solana_program_runtime::loaded_programs::{BlockRelation, ForkGraph},
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    std::{
        collections::{BTreeMap, HashMap},
        sync::{Arc, RwLock},
    },
};

/// Recently frozen banks, keyed by slot. Filled from the block-metadata tee (FL looks the
/// bank up in `bank_forks` on its own thread, holding the read lock only for the clone).
#[derive(Default)]
pub struct FrozenBanks {
    banks: BTreeMap<Slot, Arc<Bank>>,
}

impl FrozenBanks {
    /// Look up `slot` in bank_forks and cache it if it is frozen with `bank_id`.
    pub fn on_frozen(
        &mut self,
        bank_forks: &RwLock<BankForks>,
        slot: Slot,
        bank_id: Option<BankId>,
    ) -> Option<Arc<Bank>> {
        let bank = bank_forks.read().ok()?.get(slot)?;
        if bank_id.is_some_and(|id| id != bank.bank_id()) || !bank.is_frozen() {
            return None;
        }
        self.banks.insert(slot, Arc::clone(&bank));
        Some(bank)
    }

    /// Seed from every frozen bank currently in bank_forks.
    pub fn seed(&mut self, bank_forks: &RwLock<BankForks>) -> Vec<Arc<Bank>> {
        let banks: Vec<Arc<Bank>> = match bank_forks.read() {
            Ok(forks) => forks.frozen_banks().map(|(_, bank)| bank).collect(),
            Err(_) => return Vec::new(),
        };
        for bank in &banks {
            self.banks.insert(bank.slot(), Arc::clone(bank));
        }
        banks
    }

    pub fn get(&self, slot: Slot) -> Option<&Arc<Bank>> {
        self.banks.get(&slot)
    }

    pub fn remove(&mut self, slot: Slot) {
        self.banks.remove(&slot);
    }

    /// Drop banks below `root`, and keep at most `max` banks.
    pub fn prune(&mut self, root: Slot, max: usize) {
        self.banks = self.banks.split_off(&root);
        while self.banks.len() > max {
            let Some((&first, _)) = self.banks.iter().next() else {
                break;
            };
            self.banks.remove(&first);
        }
    }

    /// Keep only banks agave still has (same bank id, not pruned), at or above `root`, and at
    /// most `max` (the newest). A held `Arc<Bank>` keeps the bank's memory alive in agave.
    pub fn retain_live(&mut self, bank_forks: &RwLock<BankForks>, root: Slot, max: usize) {
        self.banks = self.banks.split_off(&root);
        if let Ok(forks) = bank_forks.read() {
            self.banks.retain(|slot, bank| {
                forks
                    .get(*slot)
                    .is_some_and(|live| live.bank_id() == bank.bank_id())
            });
        }
        while self.banks.len() > max {
            let Some((&first, _)) = self.banks.iter().next() else {
                break;
            };
            self.banks.remove(&first);
        }
    }

    pub fn len(&self) -> usize {
        self.banks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.banks.is_empty()
    }
}

/// Parent links of recent slots, for the private program cache's fork queries.
#[derive(Default)]
pub struct FlForkGraph {
    parents: HashMap<Slot, Slot>,
    root: Slot,
}

impl FlForkGraph {
    pub fn set_parent(&mut self, slot: Slot, parent: Slot) {
        if parent >= slot {
            warn!("fast lane: ignoring parent {parent} >= slot {slot}");
            return;
        }
        self.parents.insert(slot, parent);
    }

    pub fn set_root(&mut self, root: Slot) {
        if root > self.root {
            self.root = root;
            // Keep links down to a margin below root so relationship queries near the
            // root still resolve.
            let keep_from = root.saturating_sub(512);
            self.parents.retain(|slot, _| *slot >= keep_from);
        }
    }

    pub fn root(&self) -> Slot {
        self.root
    }

    pub fn clear(&mut self) {
        self.parents.clear();
    }

    fn is_ancestor(&self, a: Slot, b: Slot) -> Option<bool> {
        // Walk b's ancestors down to a.
        let mut s = b;
        loop {
            if s == a {
                return Some(true);
            }
            if s < a {
                return Some(false);
            }
            match self.parents.get(&s) {
                Some(&p) => s = p,
                None => return None,
            }
        }
    }
}

impl ForkGraph for FlForkGraph {
    fn relationship(&self, a: Slot, b: Slot) -> BlockRelation {
        if a == b {
            return BlockRelation::Equal;
        }
        let (lo, hi) = if a < b { (a, b) } else { (b, a) };
        match self.is_ancestor(lo, hi) {
            Some(true) => {
                if a < b {
                    BlockRelation::Ancestor
                } else {
                    BlockRelation::Descendant
                }
            }
            Some(false) => BlockRelation::Unrelated,
            None => BlockRelation::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_relationship() {
        let mut g = FlForkGraph::default();
        // 10 <- 11 <- 13 ; 10 <- 12
        g.set_parent(11, 10);
        g.set_parent(13, 11);
        g.set_parent(12, 10);
        assert_eq!(g.relationship(11, 11), BlockRelation::Equal);
        assert_eq!(g.relationship(10, 13), BlockRelation::Ancestor);
        assert_eq!(g.relationship(13, 10), BlockRelation::Descendant);
        assert_eq!(g.relationship(12, 13), BlockRelation::Unrelated);
        assert_eq!(g.relationship(11, 12), BlockRelation::Unrelated);
        assert_eq!(g.relationship(5, 13), BlockRelation::Unknown);
    }
}
