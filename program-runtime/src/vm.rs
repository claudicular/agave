//! SBF virtual machine provisioning and execution.

#[cfg(feature = "svm-internal")]
use qualifier_attr::qualifiers;
use {
    crate::{
        execution_budget::MAX_INSTRUCTION_STACK_DEPTH_SIMD_0268,
        invoke_context::{BpfAllocator, InvokeContext},
        mem_pool::VmMemoryPool,
        memory_context::{MemoryContext, SerializedAccountMetadata},
        program_cache_entry::ProgramCacheEntry,
        serialization, stable_log, vm_opts,
    },
    solana_instruction::error::InstructionError,
    solana_program_entrypoint::{MAX_PERMITTED_DATA_INCREASE, SUCCESS},
    solana_sbpf::{
        aligned_memory::AlignedMemory,
        ebpf::{self, HOST_ALIGN, MM_HEAP_START, MM_RODATA_START, MM_STACK_START},
        elf::Executable,
        error::{EbpfError, ProgramResult},
        memory_region::{AccessType, MemoryMapping, MemoryRegion},
        vm::{ContextObject, EbpfVm, ExecutionMode},
    },
    solana_sdk_ids::bpf_loader_deprecated,
    solana_svm_log_collector::ic_logger_msg,
    solana_svm_measure::measure::Measure,
    solana_transaction_context::IndexOfAccount,
    std::{cell::RefCell, mem, time::Duration},
};

thread_local! {
    pub static MEMORY_POOL: RefCell<VmMemoryPool> = RefCell::new(VmMemoryPool::new());
}

/// Only used in macro, do not use directly!
pub fn calculate_heap_cost(heap_size: u32, heap_cost: u64) -> u64 {
    const KIBIBYTE: u64 = 1024;
    const PAGE_SIZE_KB: u64 = 32;
    let mut rounded_heap_size = u64::from(heap_size);
    rounded_heap_size =
        rounded_heap_size.saturating_add(PAGE_SIZE_KB.saturating_mul(KIBIBYTE).saturating_sub(1));
    rounded_heap_size
        .checked_div(PAGE_SIZE_KB.saturating_mul(KIBIBYTE))
        .expect("PAGE_SIZE_KB * KIBIBYTE > 0")
        .saturating_sub(1)
        .saturating_mul(heap_cost)
}

/// Only used in macro, do not use directly!
///
/// # Safety
///
/// Refer to [`configure_program_regions`].
#[cfg_attr(feature = "svm-internal", qualifiers(pub))]
pub unsafe fn create_vm<'a, 'b>(
    program: &'a Executable<InvokeContext<'b, 'b>>,
    invoke_context: &'a mut InvokeContext<'b, 'b>,
    stack: *mut [u8],
    heap: *mut [u8],
) -> Result<EbpfVm<'a, InvokeContext<'b, 'b>>, Box<dyn std::error::Error>> {
    let stack_size = stack.len();
    unsafe {
        // SAFETY: invariants delegated to the caller.
        configure_program_regions(invoke_context, program, stack, heap)?;
    }
    Ok(EbpfVm::new(
        program.get_loader().clone(),
        program.get_sbpf_version(),
        invoke_context,
        stack_size,
    ))
}

/// # Safety
///
/// The `executable`, `stack` and `heap` arguments must remain allocated for at least the lifetime
/// of [`MemoryMapping`] (or until after the `MemoryMapping` is reconfigured with different
/// `executable`, `stack` and `heap`).
unsafe fn configure_program_regions<C: ContextObject>(
    invoke_context: &mut InvokeContext,
    executable: &Executable<C>,
    stack: *mut [u8],
    heap: *mut [u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let mapping = invoke_context.memory_contexts.memory_mapping_mut()?;
    let regions = mapping.get_regions_mut();
    let [ro_area, stack_area, heap_area, ..] = regions else {
        panic!("the regions vector must have at least three entries")
    };
    *ro_area = executable.get_ro_region();
    let sbpf_version = executable.get_sbpf_version();
    let config = executable.get_config();
    *stack_area = MemoryRegion::new_gapped(
        stack,
        MM_STACK_START,
        if sbpf_version.stack_frame_gaps() && config.enable_stack_frame_gaps {
            config.stack_frame_size as u64
        } else {
            0
        },
    );
    *heap_area = MemoryRegion::new(heap, MM_HEAP_START);
    mapping
        .initialize()
        .map_err(|err| Box::new(err) as Box<dyn std::error::Error>)
}

/// Create the SBF virtual machine
#[macro_export]
macro_rules! create_vm {
    ($vm:ident, $program:expr, $invoke_context:expr $(,)?) => {
        let invoke_context = &*$invoke_context;
        let stack_size = $program.get_config().stack_size();
        let heap_size = invoke_context.get_compute_budget().heap_size;
        let heap_cost_result =
            invoke_context
                .compute_meter
                .consume_checked($crate::__private::calculate_heap_cost(
                    heap_size,
                    invoke_context.get_execution_cost().heap_cost,
                ));
        let $vm = heap_cost_result.and_then(|_| {
            let (mut stack, mut heap) = $crate::__private::MEMORY_POOL
                .with_borrow_mut(|pool| (pool.get_stack(stack_size), pool.get_heap(heap_size)));
            let vm = $crate::__private::create_vm(
                $program,
                $invoke_context,
                stack
                    .as_slice_mut()
                    .get_mut(..stack_size)
                    .expect("invalid stack size"),
                heap.as_slice_mut()
                    .get_mut(..heap_size as usize)
                    .expect("invalid heap size"),
            );
            vm.map(|vm| (vm, stack, heap))
        });
    };
}

/// # Safety
///
/// The [`MemoryRegion`]s must satisfy the safety preconditions for
/// [`MemoryMapping::new_uninitialized`].
unsafe fn set_memory_context<'b>(
    additional_initialized_regions: Vec<MemoryRegion>,
    accounts_metadata: Vec<SerializedAccountMetadata>,
    invoke_context: &mut InvokeContext<'b, 'b>,
    executable: &Executable<InvokeContext<'b, 'b>>,
    virtual_address_space_adjustments: bool,
    account_data_direct_mapping: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let heap_size = invoke_context.get_compute_budget().heap_size;
    let regions = [
        MemoryRegion::new_empty(MM_RODATA_START),
        MemoryRegion::new_empty(MM_STACK_START),
        MemoryRegion::new_empty(MM_HEAP_START),
    ]
    .into_iter()
    .chain(additional_initialized_regions)
    .collect();
    let memory_mapping = unsafe {
        // SAFETY: all memory regions are `default` (and thus implicitly valid) or valid by
        // delegating the safety invariant upon the caller.
        MemoryMapping::new_uninitialized(
            regions,
            executable.get_config(),
            executable.get_sbpf_version(),
            invoke_context.transaction_context.access_violation_handler(
                virtual_address_space_adjustments,
                account_data_direct_mapping,
            ),
        )
    };

    invoke_context
        .memory_contexts
        .set_memory_context_abi_v1(MemoryContext::new(
            BpfAllocator::new(heap_size as u64),
            accounts_metadata,
            memory_mapping,
        ))
        .map_err(|err| Box::new(err) as Box<dyn std::error::Error>)
}

/// The serialized program input of one invocation. It must outlive the invocation's
/// `MemoryMapping`, whose input regions point into it.
enum ParameterBytes {
    Aligned(AlignedMemory<HOST_ALIGN>),
    Pooled(serialization::PooledParameterBuffer),
}

impl ParameterBytes {
    fn as_slice(&self) -> &[u8] {
        match self {
            ParameterBytes::Aligned(buffer) => buffer.as_slice(),
            ParameterBytes::Pooled(buffer) => buffer.as_slice(),
        }
    }
}

#[cfg_attr(feature = "svm-internal", qualifiers(pub))]
pub fn execute<'a, 'b: 'a>(
    executable: &'a Executable<InvokeContext<'static, 'static>>,
    invoke_context: &'a mut InvokeContext<'b, 'b>,
    cache_entry: &ProgramCacheEntry,
) -> Result<(), Box<dyn std::error::Error>> {
    // We dropped the lifetime tracking in the Executor by setting it to 'static,
    // thus we need to reintroduce the correct lifetime of InvokeContext here again.
    let executable = unsafe {
        mem::transmute::<
            &'a Executable<InvokeContext<'static, 'static>>,
            &'a Executable<InvokeContext<'b, 'b>>,
        >(executable)
    };
    let log_collector = invoke_context.get_log_collector();
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let program_id = *instruction_context.get_program_key()?;
    let is_loader_deprecated =
        instruction_context.get_program_owner()? == bpf_loader_deprecated::id();
    let virtual_address_space_adjustments = invoke_context
        .get_feature_set()
        .virtual_address_space_adjustments;
    let account_data_direct_mapping = invoke_context.get_feature_set().account_data_direct_mapping;
    let direct_account_pointers_in_program_input = invoke_context
        .get_feature_set()
        .direct_account_pointers_in_program_input;

    let mut serialize_time = Measure::start("serialize");
    let (parameter_bytes, regions, accounts_metadata, instruction_data_offset) =
        if vm_opts::SER_POOL.enabled() {
            let (buffer, regions, accounts_metadata, instruction_data_offset) =
                serialization::serialize_parameters_pooled(
                    &instruction_context,
                    virtual_address_space_adjustments,
                    account_data_direct_mapping,
                    direct_account_pointers_in_program_input,
                )?;
            (
                ParameterBytes::Pooled(buffer),
                regions,
                accounts_metadata,
                instruction_data_offset,
            )
        } else {
            let (buffer, regions, accounts_metadata, instruction_data_offset) =
                serialization::serialize_parameters(
                    &instruction_context,
                    virtual_address_space_adjustments,
                    account_data_direct_mapping,
                    direct_account_pointers_in_program_input,
                )?;
            (
                ParameterBytes::Aligned(buffer),
                regions,
                accounts_metadata,
                instruction_data_offset,
            )
        };
    serialize_time.stop();

    // save the account addresses so in case we hit an AccessViolation error we
    // can map to a more specific error
    let account_region_addrs = accounts_metadata
        .iter()
        .map(|m| {
            let vm_end = m
                .vm_data_addr
                .saturating_add(m.original_data_len as u64)
                .saturating_add(if !is_loader_deprecated {
                    MAX_PERMITTED_DATA_INCREASE as u64
                } else {
                    0
                });
            m.vm_data_addr..vm_end
        })
        .collect::<Vec<_>>();

    #[cfg(feature = "sbpf-debugger")]
    let (debug_port, debug_metadata) = if invoke_context.debug_port.is_some() {
        (
            invoke_context.debug_port,
            Some(format!(
                "program_id={};cpi_level={};caller={}",
                program_id,
                instruction_context.get_stack_height().saturating_sub(1),
                invoke_context
                    .get_stack_height()
                    .checked_sub(2)
                    .and_then(|nesting_level| {
                        transaction_context
                            .get_instruction_context_at_nesting_level(nesting_level)
                            .ok()
                    })
                    .and_then(|ctx| ctx.get_program_key().ok())
                    .map(|key| key.to_string())
                    .unwrap_or_else(|| "none".into())
            )),
        )
    } else {
        (None, None)
    };

    let mut create_vm_time = Measure::start("create_vm");
    unsafe {
        // SAFETY: The memory pointed to by regions is valid for the useful lifetime of
        // `invoke_context`, which in turn contains the `MemoryMapping` that allows access to this
        // memory.
        set_memory_context(
            regions,
            accounts_metadata,
            invoke_context,
            executable,
            virtual_address_space_adjustments,
            account_data_direct_mapping,
        )?
    };

    let execution_result = {
        let mut execution_mode = ExecutionMode::PreferJit;

        #[cfg(feature = "sbpf-debugger")]
        if invoke_context.debug_port.is_some() {
            execution_mode = ExecutionMode::Interpreted;
        }

        let compute_meter_prev = invoke_context.get_remaining();
        // `create_vm!` maps exactly `heap[..heap_size]` into the VM, with `heap_size` read from
        // the same (immutable for the lifetime of the InvokeContext) compute budget.
        let mapped_heap_len = invoke_context.get_compute_budget().heap_size as usize;
        let (mut vm, stack, heap) = unsafe {
            // SAFETY: The `stack`, `heap` and `executable` live past the lifetime of
            // `invoke_context`.
            create_vm!(vm, executable, invoke_context);
            match vm {
                Ok(info) => info,
                Err(e) => {
                    ic_logger_msg!(log_collector, "Failed to create SBF VM: {}", e);
                    return Err(Box::new(InstructionError::ProgramEnvironmentSetupFailure));
                }
            }
        };

        create_vm_time.stop();
        #[cfg(feature = "sbpf-debugger")]
        {
            vm.debug_port = debug_port;
            vm.debug_metadata = debug_metadata;
        }

        let execute_time = Measure::start("execute");
        let prev_nested_exec_time = vm.context().total_nested_exec_time;

        vm.registers[1] = ebpf::MM_INPUT_START;
        vm.registers[2] = instruction_data_offset as u64;
        let mut call_frames =
            MEMORY_POOL.with_borrow_mut(|memory_pool| memory_pool.get_call_frames());
        let (compute_units_consumed, result) =
            vm.execute_program(executable, &mut execution_mode, &mut call_frames);
        let register_trace = std::mem::take(&mut vm.register_trace);
        MEMORY_POOL.with_borrow_mut(|memory_pool| {
            memory_pool.put_stack(stack);
            if vm_opts::HEAP_ZERO_OPT.enabled() {
                memory_pool.put_heap_mapped_prefix(heap, mapped_heap_len);
            } else {
                memory_pool.put_heap(heap);
            }
            memory_pool.put_call_frames(call_frames);
            debug_assert!(memory_pool.stack_len() <= MAX_INSTRUCTION_STACK_DEPTH_SIMD_0268);
            debug_assert!(memory_pool.heap_len() <= MAX_INSTRUCTION_STACK_DEPTH_SIMD_0268);
        });
        drop(vm);
        invoke_context.insert_register_trace(register_trace);

        // This section is a little convoluted due to the nested and sibling (CPI) invocations.
        let total_execute_ns = execute_time.end_as_ns();
        let nested_execution_time_delta = invoke_context
            .total_nested_exec_time
            .saturating_sub(prev_nested_exec_time);
        let this_call_ns =
            total_execute_ns.saturating_sub(nested_execution_time_delta.as_nanos() as u64);
        invoke_context.total_nested_exec_time = invoke_context
            .total_nested_exec_time
            .saturating_add(Duration::from_nanos(this_call_ns));
        let this_call_us = this_call_ns / 1000;
        invoke_context.timings.execute_us += this_call_us;
        match execution_mode {
            ExecutionMode::Interpreted => cache_entry.stats.interpreter_executed(this_call_us),
            ExecutionMode::Jit => cache_entry.stats.jit_executed(this_call_us),
            ExecutionMode::PreferJit => { /* not actually executed? */ }
        }

        ic_logger_msg!(
            log_collector,
            "Program {} consumed {} of {} compute units",
            &program_id,
            compute_units_consumed,
            compute_meter_prev
        );
        let (_returned_from_program_id, return_data) =
            invoke_context.transaction_context.get_return_data();
        if !return_data.is_empty() {
            stable_log::program_return(&log_collector, &program_id, return_data);
        }
        match result {
            ProgramResult::Ok(status) if status != SUCCESS => {
                let error: InstructionError = status.into();
                Err(Box::new(error) as Box<dyn std::error::Error>)
            }
            ProgramResult::Err(mut error) => {
                // Don't clean me up!!
                // This feature is active on all networks, but we still toggle
                // it off during fuzzing.
                if invoke_context
                    .get_feature_set()
                    .deplete_cu_meter_on_vm_failure
                    && !matches!(error, EbpfError::SyscallError(_))
                {
                    // when an exception is thrown during the execution of a
                    // Basic Block (e.g., a null memory dereference or other
                    // faults), determining the exact number of CUs consumed
                    // up to the point of failure requires additional effort
                    // and is unnecessary since these cases are rare.
                    //
                    // In order to simplify CU tracking, simply consume all
                    // remaining compute units so that the block cost
                    // tracker uses the full requested compute unit cost for
                    // this failed transaction.
                    invoke_context.consume(invoke_context.get_remaining());
                }

                if virtual_address_space_adjustments {
                    if let EbpfError::SyscallError(err) = error {
                        error = err
                            .downcast::<EbpfError>()
                            .map(|err| *err)
                            .unwrap_or_else(EbpfError::SyscallError);
                    }
                    if let EbpfError::AccessViolation(access_type, vm_addr, len, _section_name) =
                        error
                    {
                        // If virtual_address_space_adjustments is enabled and a program tries to write to a readonly
                        // region we'll get a memory access violation. Map it to a more specific
                        // error so it's easier for developers to see what happened.
                        if let Some((instruction_account_index, vm_addr_range)) =
                            account_region_addrs
                                .iter()
                                .enumerate()
                                .find(|(_, vm_addr_range)| vm_addr_range.contains(&vm_addr))
                        {
                            let transaction_context = &invoke_context.transaction_context;
                            let instruction_context =
                                transaction_context.get_current_instruction_context()?;
                            let account = instruction_context.try_borrow_instruction_account(
                                instruction_account_index as IndexOfAccount,
                            )?;
                            if vm_addr.saturating_add(len) <= vm_addr_range.end {
                                // The access was within the range of the accounts address space,
                                // but it might not be within the range of the actual data.
                                let is_access_outside_of_data = vm_addr
                                    .saturating_add(len)
                                    .saturating_sub(vm_addr_range.start)
                                    as usize
                                    > account.get_data().len();
                                error = EbpfError::SyscallError(Box::new(match access_type {
                                    AccessType::Store => {
                                        if let Err(err) = account.can_data_be_changed() {
                                            err
                                        } else {
                                            // The store was allowed but failed,
                                            // thus it must have been an attempt to grow the account.
                                            debug_assert!(is_access_outside_of_data);
                                            InstructionError::InvalidRealloc
                                        }
                                    }
                                    AccessType::Load => {
                                        // Loads should only fail when they are outside of the account data.
                                        debug_assert!(is_access_outside_of_data);
                                        if account.can_data_be_changed().is_err() {
                                            // Load beyond readonly account data happened because the program
                                            // expected more data than there actually is.
                                            InstructionError::AccountDataTooSmall
                                        } else {
                                            // Load beyond writable account data also attempted to grow.
                                            InstructionError::InvalidRealloc
                                        }
                                    }
                                }));
                            }
                        }
                    }
                }
                Err(if let EbpfError::SyscallError(err) = error {
                    err
                } else {
                    error.into()
                })
            }
            _ => Ok(()),
        }
    };

    fn deserialize_parameters(
        invoke_context: &mut InvokeContext,
        parameter_bytes: &[u8],
        virtual_address_space_adjustments: bool,
        account_data_direct_mapping: bool,
    ) -> Result<(), InstructionError> {
        serialization::deserialize_parameters(
            &invoke_context
                .transaction_context
                .get_current_instruction_context()?,
            virtual_address_space_adjustments,
            account_data_direct_mapping,
            parameter_bytes,
            &invoke_context
                .memory_contexts
                .memory_context_abi_v1()?
                .accounts_metadata,
        )
    }

    let mut deserialize_time = Measure::start("deserialize");
    let execute_or_deserialize_result = execution_result.and_then(|_| {
        deserialize_parameters(
            invoke_context,
            parameter_bytes.as_slice(),
            virtual_address_space_adjustments,
            account_data_direct_mapping,
        )
        .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
    });
    deserialize_time.stop();

    // Update the timings
    invoke_context.timings.serialize_us += serialize_time.as_us();
    invoke_context.timings.create_vm_us += create_vm_time.as_us();
    invoke_context.timings.deserialize_us += deserialize_time.as_us();

    execute_or_deserialize_result
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub(crate) mod tests {
    use {
        super::*,
        crate::{
            execution_budget::{MAX_HEAP_FRAME_BYTES, MIN_HEAP_FRAME_BYTES},
            program_cache_entry::{ProgramCacheEntryOwner, ProgramCacheEntryType},
            with_mock_invoke_context,
        },
        solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
        solana_pubkey::Pubkey,
        solana_sbpf::{
            assembler::assemble,
            program::{BuiltinProgram, SBPFVersion},
            vm::Config,
        },
        solana_sdk_ids::bpf_loader,
        std::sync::{Arc, atomic::AtomicU64},
    };

    /// Assembles an SBPFv0 program (the version of every hot mainnet program) without syscalls.
    pub(crate) fn assemble_v0(src: &str) -> Executable<InvokeContext<'static, 'static>> {
        let config = Config {
            enabled_sbpf_versions: SBPFVersion::V0..=SBPFVersion::V0,
            ..Config::default()
        };
        let loader = Arc::new(BuiltinProgram::new_loader(config));
        let executable = assemble::<InvokeContext<'static, 'static>>(src, loader).unwrap();
        #[cfg(all(not(target_os = "windows"), target_arch = "x86_64"))]
        executable.jit_compile().unwrap();
        executable
    }

    pub(crate) fn dummy_cache_entry() -> ProgramCacheEntry {
        ProgramCacheEntry {
            program: ProgramCacheEntryType::Closed,
            account_owner: ProgramCacheEntryOwner::LoaderV2,
            deployment_slot: 0,
            stats: Arc::default(),
            latest_access_slot: AtomicU64::new(0),
        }
    }

    /// Outcome of one top-level invocation that a program or the cluster could observe.
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) struct Observed {
        pub result: Result<(), String>,
        pub remaining_cu: u64,
        pub logs: Vec<String>,
    }

    /// Runs `executable` as a loader-v2 program in a fresh invoke context with `heap_size`.
    pub(crate) fn run_program(
        executable: &Executable<InvokeContext<'static, 'static>>,
        heap_size: u32,
        instruction_data: &[u8],
    ) -> Observed {
        run_program_with_accounts(executable, heap_size, instruction_data, vec![]).0
    }

    /// Like [`run_program`], with instruction accounts `(key, account, is_writable)`. Also
    /// returns the accounts after the invocation.
    pub(crate) fn run_program_with_accounts(
        executable: &Executable<InvokeContext<'static, 'static>>,
        heap_size: u32,
        instruction_data: &[u8],
        accounts: Vec<(Pubkey, AccountSharedData, bool)>,
    ) -> (Observed, Vec<AccountSharedData>) {
        use solana_transaction_context::instruction_accounts::InstructionAccount;
        // Fixed, so that logs of repeated runs are comparable.
        let program_id = Pubkey::new_from_array([7; 32]);
        let mut program_account = AccountSharedData::new(1, 0, &bpf_loader::id());
        program_account.set_executable(true);
        let instruction_accounts = accounts
            .iter()
            .enumerate()
            .map(|(index, (_, _, is_writable))| {
                InstructionAccount::new(index as u16 + 1, false, *is_writable)
            })
            .collect::<Vec<_>>();
        let transaction_accounts = std::iter::once((program_id, program_account))
            .chain(accounts.into_iter().map(|(key, account, _)| (key, account)))
            .collect::<Vec<_>>();
        with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);
        invoke_context.set_heap_size_for_tests(heap_size);
        invoke_context
            .transaction_context
            .configure_top_level_instruction_for_tests(
                0,
                instruction_accounts,
                instruction_data.to_vec(),
            )
            .unwrap();
        invoke_context.push().unwrap();
        let result = execute(executable, &mut invoke_context, &dummy_cache_entry())
            .map_err(|err| err.to_string());
        let remaining_cu = invoke_context.get_remaining();
        let logs = invoke_context
            .get_log_collector()
            .unwrap()
            .borrow()
            .get_recorded_content()
            .to_vec();
        let post_accounts = (0..invoke_context.transaction_context.get_number_of_accounts())
            .map(|index| {
                invoke_context
                    .transaction_context
                    .accounts()
                    .try_borrow(index)
                    .map(|account| {
                        #[allow(deprecated)]
                        let executable = account.executable();
                        AccountSharedData::from(solana_account::Account {
                            lamports: account.lamports(),
                            data: account.data().to_vec(),
                            owner: *account.owner(),
                            executable,
                            rent_epoch: account.rent_epoch(),
                        })
                    })
                    .unwrap()
            })
            .collect();
        (
            Observed {
                result,
                remaining_cu,
                logs,
            },
            post_accounts,
        )
    }

    /// Writes 0xff to every byte of `[MM_HEAP_START, MM_HEAP_START + len)`, 8 bytes at a time.
    fn heap_fill_program(len: u32) -> Executable<InvokeContext<'static, 'static>> {
        assemble_v0(&format!(
            "
            lddw r2, {start:#x}
            lddw r3, {end:#x}
            mov64 r4, -1
            stxdw [r2+0], r4
            add64 r2, 8
            jlt r2, r3, -3
            mov64 r0, 0
            exit",
            start = MM_HEAP_START,
            end = MM_HEAP_START + u64::from(len),
        ))
    }

    /// ORs every 8-byte word of `[MM_HEAP_START, MM_HEAP_START + len)` into r0 and returns it, so
    /// the invocation fails unless the whole range is zero.
    fn heap_check_program(len: u32) -> Executable<InvokeContext<'static, 'static>> {
        assemble_v0(&format!(
            "
            lddw r2, {start:#x}
            lddw r3, {end:#x}
            mov64 r0, 0
            ldxdw r4, [r2+0]
            or64 r0, r4
            add64 r2, 8
            jlt r2, r3, -4
            exit",
            start = MM_HEAP_START,
            end = MM_HEAP_START + u64::from(len),
        ))
    }

    /// Dirty heaps of several sizes through the VM and read them back from later invocations
    /// with larger heaps, on the same thread (so the same pooled buffer is reused).
    fn heap_reuse_sequence() -> Vec<Observed> {
        let max = MAX_HEAP_FRAME_BYTES;
        let min = MIN_HEAP_FRAME_BYTES;
        let odd = 40 * 1024;
        let steps: [(bool, u32, u32); 9] = [
            // (fill?, program range, heap_size)
            (false, max, max),
            (true, max, max),
            (false, min, min),
            (false, max, max),
            (true, min, min),
            (false, max, max),
            (true, odd, odd),
            (false, max, max),
            // Out of range for the mapped heap: must fault identically in both modes.
            (true, max, min),
        ];
        steps
            .iter()
            .map(|(fill, range, heap_size)| {
                let program = if *fill {
                    heap_fill_program(*range)
                } else {
                    heap_check_program(*range)
                };
                run_program(&program, *heap_size, &[])
            })
            .collect()
    }

    #[test]
    fn test_heap_zero_opt_is_unobservable() {
        let stock = {
            vm_opts::HEAP_ZERO_OPT.set(false);
            heap_reuse_sequence()
        };
        let optimized = {
            vm_opts::HEAP_ZERO_OPT.set(true);
            heap_reuse_sequence()
        };
        vm_opts::HEAP_ZERO_OPT.set(false);

        // Every read-back of a previously dirtied heap sees zeros...
        for (index, observed) in stock.iter().enumerate() {
            if index == stock.len() - 1 {
                assert!(observed.result.is_err(), "{observed:?}");
            } else {
                assert_eq!(observed.result, Ok(()), "step {index}: {observed:?}");
            }
        }
        // ...and results, compute units and logs are identical with the switch on.
        assert_eq!(stock, optimized);
    }

    /// Hashes every byte of the serialized input `[MM_INPUT_START, end)`, where `end` is the
    /// end of the program id that follows the instruction data (r2 points at the instruction
    /// data, whose u64 length precedes it). Stores the hash into the first 8 data bytes of the
    /// first account (writable, 16 data bytes, program-owned), writes a pattern into its realloc
    /// padding and grows it by 3 bytes, so the hash and the padding handling become visible in
    /// the post-invocation account state (via deserialization).
    fn input_hash_program() -> Executable<InvokeContext<'static, 'static>> {
        assemble_v0(
            "
            mov64 r9, r1
            ldxdw r3, [r2-8]
            add64 r3, r2
            add64 r3, 32
            mov64 r0, 0
            mov64 r4, r1
            ldxb r5, [r4+0]
            mul64 r0, 31
            add64 r0, r5
            add64 r4, 1
            jlt r4, r3, -5
            stxdw [r9+96], r0
            mov64 r6, 0x77
            stxb [r9+112], r6
            stxb [r9+113], r6
            stxb [r9+114], r6
            ldxdw r6, [r9+88]
            add64 r6, 3
            stxdw [r9+88], r6
            mov64 r0, 0
            exit",
        )
    }

    fn input_hash_sequence() -> Vec<(Observed, Vec<AccountSharedData>)> {
        let program = input_hash_program();
        let owner = Pubkey::new_from_array([7; 32]);
        let account = |seed: u8, len: usize| {
            let mut account = AccountSharedData::new(u64::from(seed), len, &owner);
            account
                .data_as_mut_slice()
                .iter_mut()
                .enumerate()
                .for_each(|(i, b)| *b = (i as u8).wrapping_mul(seed));
            account
        };
        [
            // Large input first so later, smaller inputs reuse its (dirty) allocation.
            vec![(5, 16), (9, 120_000), (8, 30_000), (3, 0)],
            vec![(5, 16), (7, 9)],
            vec![(5, 16), (11, 12_345), (2, 1), (4, 0)],
            vec![(5, 16)],
        ]
        .into_iter()
        .map(|accounts| {
            let accounts = accounts
                .into_iter()
                .enumerate()
                .map(|(i, (seed, len))| {
                    (
                        Pubkey::new_from_array([i as u8 + 10; 32]),
                        account(seed, len),
                        i == 0,
                    )
                })
                .collect();
            run_program_with_accounts(&program, MIN_HEAP_FRAME_BYTES, &[1, 2, 3], accounts)
        })
        .collect()
    }

    #[test]
    fn test_ser_pool_is_unobservable() {
        let stock = {
            vm_opts::SER_POOL.set(false);
            input_hash_sequence()
        };
        let pooled = {
            vm_opts::SER_POOL.set(true);
            crate::serialization::PooledParameterBuffer::dirty_pool_for_tests(0xa5);
            let first = input_hash_sequence();
            crate::serialization::PooledParameterBuffer::dirty_pool_for_tests(0xff);
            let second = input_hash_sequence();
            assert_eq!(first, second);
            first
        };
        vm_opts::SER_POOL.set(false);
        for (observed, accounts) in &stock {
            assert_eq!(observed.result, Ok(()), "{observed:?}");
            // The first instruction account (index 1) holds the input hash, then its original
            // bytes 8..16, then the 3 pattern bytes it grew into.
            let data = accounts[1].data();
            assert_eq!(data.len(), 19);
            assert_ne!(data[..8], [0; 8]);
            assert_eq!(data[16..], [0x77; 3]);
        }
        // Different inputs hash differently (the hash covers the whole input).
        assert_ne!(stock[1].1[1].data()[..8], stock[3].1[1].data()[..8]);
        assert_eq!(stock, pooled);
    }

    #[test]
    fn test_heap_check_program_detects_dirty_heap() {
        // Negative control: if a pooled heap were returned with a too-short reset, the check
        // program would see it. Simulate that bug directly on the pool.
        let fill = heap_fill_program(MAX_HEAP_FRAME_BYTES);
        let check = heap_check_program(MAX_HEAP_FRAME_BYTES);
        assert_eq!(run_program(&fill, MAX_HEAP_FRAME_BYTES, &[]).result, Ok(()));
        MEMORY_POOL.with_borrow_mut(|pool| {
            let mut heap = pool.get_heap(MAX_HEAP_FRAME_BYTES);
            heap.as_slice_mut().fill(0x5a);
            assert!(pool.put_heap_mapped_prefix(heap, 0));
        });
        assert!(
            run_program(&check, MAX_HEAP_FRAME_BYTES, &[])
                .result
                .is_err()
        );
        // Restore a clean pool for other tests on this thread.
        MEMORY_POOL.with_borrow_mut(|pool| {
            let heap = pool.get_heap(MAX_HEAP_FRAME_BYTES);
            assert!(pool.put_heap(heap));
        });
        assert_eq!(
            run_program(&check, MAX_HEAP_FRAME_BYTES, &[]).result,
            Ok(())
        );
    }
}
