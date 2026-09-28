//! The fast lane's private program cache.
//!
//! FL never touches agave's program cache for writes: agave's `BankForks` relationship
//! returns `Unknown` for slots it has not created (FL would silently run stale programs or
//! re-load forever), a speculative insertion would survive agave's own merge, and
//! cooperative loading would couple FL and replay latencies. FL owns one master
//! `TransactionBatchProcessor<FlForkGraph>` per epoch environment: builtins rebuilt from
//! their prototypes, the parent's execution cost and environment Arc, and seeded (no JIT)
//! with agave's compiled entries deployed at or below root. Misses are loaded by FL itself
//! through its overlay callback (see `mv` for the program-read finality rule).

use {
    crate::forks::FlForkGraph,
    log::info,
    solana_clock::{Epoch, Slot},
    solana_program_runtime::loaded_programs::ProgramRuntimeEnvironment,
    solana_runtime::bank::{Bank, fast_lane::FastLaneUnsupported},
    solana_svm::transaction_processor::TransactionBatchProcessor,
    std::sync::{Arc, RwLock},
};

struct Master {
    epoch: Epoch,
    env: ProgramRuntimeEnvironment,
    processor: TransactionBatchProcessor<FlForkGraph>,
}

pub struct ProgramCaches {
    fork_graph: Arc<RwLock<FlForkGraph>>,
    master: Option<Master>,
    pub seeded_entries: usize,
    pub rebuilds: u64,
}

impl ProgramCaches {
    pub fn new(fork_graph: Arc<RwLock<FlForkGraph>>) -> Self {
        Self {
            fork_graph,
            master: None,
            seeded_entries: 0,
            rebuilds: 0,
        }
    }

    pub fn fork_graph(&self) -> &Arc<RwLock<FlForkGraph>> {
        &self.fork_graph
    }

    fn root(&self) -> Slot {
        self.fork_graph.read().map(|g| g.root()).unwrap_or(0)
    }

    /// A per-run processor for `slot` whose parent is `parent`, sharing the private cache.
    pub fn processor_for(
        &mut self,
        parent: &Bank,
        slot: Slot,
    ) -> Result<TransactionBatchProcessor<FlForkGraph>, FastLaneUnsupported> {
        let setup_needed = match &self.master {
            Some(master) => {
                master.epoch != parent.epoch()
                    || master.processor.program_runtime_environment
                        != parent_env_probe(parent)?
            }
            None => true,
        };
        if setup_needed {
            self.rebuild(parent)?;
        }
        let master = self
            .master
            .as_ref()
            .ok_or(FastLaneUnsupported::UnknownBuiltin)?;
        Ok(master.processor.new_from(slot, master.epoch))
    }

    fn rebuild(&mut self, parent: &Bank) -> Result<(), FastLaneUnsupported> {
        let root = self.root();
        let setup = parent.fast_lane_program_setup()?;
        let mut processor =
            TransactionBatchProcessor::<FlForkGraph>::new_uninitialized(root, setup.epoch);
        {
            let mut cache = processor.global_program_cache.write().unwrap();
            cache.set_fork_graph(Arc::downgrade(&self.fork_graph));
            cache.latest_root_slot = root;
        }
        processor.program_runtime_environment = setup.program_runtime_environment.clone();
        processor.set_execution_cost(setup.execution_cost);
        for (program_id, entry) in setup.builtins {
            processor.add_builtin(program_id, entry);
        }
        let entries =
            parent.fast_lane_rooted_program_entries(root, &setup.program_runtime_environment);
        let seeded = entries.len();
        {
            let mut cache = processor.global_program_cache.write().unwrap();
            for (key, entry) in entries {
                let deployment_slot = entry.deployment_slot;
                cache.assign_program(
                    &setup.program_runtime_environment,
                    key,
                    deployment_slot,
                    entry,
                );
            }
        }
        info!(
            "fast lane: program cache built for epoch {} at root {root}: {seeded} entries seeded",
            setup.epoch
        );
        self.seeded_entries = seeded;
        self.rebuilds += 1;
        self.master = Some(Master {
            epoch: setup.epoch,
            env: setup.program_runtime_environment,
            processor,
        });
        Ok(())
    }

    /// Advance the root: prune entries of orphaned forks (as agave does at re-root).
    pub fn set_root(&mut self, root: Slot) {
        let Ok(mut graph) = self.fork_graph.write() else {
            return;
        };
        if root <= graph.root() {
            return;
        }
        graph.set_root(root);
        drop(graph);
        if let Some(master) = &self.master {
            let graph = self.fork_graph.read().unwrap();
            let mut cache = master.processor.global_program_cache.write().unwrap();
            if cache.latest_root_slot < root {
                cache.prune(root, None, &graph);
            }
        }
    }

    /// Drop entries deployed at `slot` (a run on `slot` was abandoned or finished; later
    /// runs reload from their frozen parent instead of trusting FL-built entries).
    pub fn prune_slot(&mut self, slot: Slot) {
        if let Some(master) = &self.master {
            master
                .processor
                .global_program_cache
                .write()
                .unwrap()
                .prune_by_deployment_slot(slot);
        }
    }

    /// Drop the private program cache (the fast lane was disabled).
    pub fn reset(&mut self) {
        self.master = None;
        if let Ok(mut graph) = self.fork_graph.write() {
            graph.clear();
        }
    }

    /// Loaded (compiled) entries and their memory (ELF, sections, JIT code).
    pub fn loaded_stats(&self) -> (usize, usize) {
        let Some(master) = &self.master else {
            return (0, 0);
        };
        let Ok(cache) = master.processor.global_program_cache.read() else {
            return (0, 0);
        };
        let entries = cache.get_flattened_entries();
        let bytes = entries
            .iter()
            .map(|(_, _, entry)| match &entry.program {
                solana_program_runtime::program_cache_entry::ProgramCacheEntryType::Loaded(
                    executable,
                ) => executable.mem_size(),
                _ => 0,
            })
            .sum();
        (entries.len(), bytes)
    }

    pub fn master_env(&self) -> Option<&ProgramRuntimeEnvironment> {
        self.master.as_ref().map(|m| &m.env)
    }
}

/// The parent's execution environment Arc (cheap: one Arc clone via the setup accessor
/// would rebuild builtins, so compare through the child-context path instead).
fn parent_env_probe(parent: &Bank) -> Result<ProgramRuntimeEnvironment, FastLaneUnsupported> {
    Ok(parent.fast_lane_execution_environment())
}
