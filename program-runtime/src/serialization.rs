#![allow(clippy::arithmetic_side_effects)]

use {
    crate::memory_context::SerializedAccountMetadata,
    solana_instruction::error::InstructionError,
    solana_program_entrypoint::{BPF_ALIGN_OF_U128, MAX_PERMITTED_DATA_INCREASE, NON_DUP_MARKER},
    solana_pubkey::Pubkey,
    solana_sbpf::{
        aligned_memory::{AlignedMemory, Pod},
        ebpf::{HOST_ALIGN, MM_INPUT_START},
        memory_region::MemoryRegion,
    },
    solana_sdk_ids::bpf_loader_deprecated,
    solana_system_interface::MAX_PERMITTED_DATA_LENGTH,
    solana_transaction_context::{
        IndexOfAccount, MAX_ACCOUNTS_PER_INSTRUCTION, instruction::InstructionContext,
        instruction_accounts::BorrowedInstructionAccount,
    },
    std::{
        alloc::{Layout, alloc, dealloc, handle_alloc_error},
        cell::RefCell,
        mem::{self, size_of},
        ptr::NonNull,
    },
};

/// Modifies the memory mapping in serialization and CPI return for virtual_address_space_adjustments
pub fn modify_memory_region_of_account(
    account: &mut BorrowedInstructionAccount<'_, '_>,
    region: &mut MemoryRegion,
) {
    let data_ptr = region.host_buffer().ptr() as *mut u8;
    let new_buffer = std::ptr::slice_from_raw_parts_mut(data_ptr, account.get_data().len());
    if account.can_data_be_changed().is_ok() {
        unsafe {
            // SAFETY:
            // Contract from `MemoryRegion::redirect`: The memory pointed to by the MemoryRegions
            // must point to a valid object live for the duration of this MemoryMapping.
            //
            // TODO(nagisa): Local reasoning for this contract is infeasible. In particular for the
            // `serialization.rs` code it is pretty easy to see that the regions passed in will
            // always be larger than `account.get_data().len()`. However for `cpi.rs` callsite this
            // is not as easy to prove and relies on careful coordination between any code that
            // might increase the account data buffer length.
            region.redirect(new_buffer);
        }
        region.access_violation_handler_payload = Some(account.get_index_in_transaction());
    } else {
        unsafe {
            // SAFETY:
            //
            // Contract from `MemoryRegion::redirect`: same as for the call above.
            // Evidence: same as for the call above.
            region.redirect(new_buffer.cast_const());
        }
        region.access_violation_handler_payload = None;
    }
}

/// Creates the memory mapping in serialization and CPI return for account_data_direct_mapping
pub fn create_memory_region_of_account(
    account: &mut BorrowedInstructionAccount<'_, '_>,
    vaddr: u64,
) -> Result<MemoryRegion, InstructionError> {
    let can_data_be_changed = account.can_data_be_changed().is_ok();
    let mut memory_region = if can_data_be_changed && !account.is_shared() {
        MemoryRegion::new(&raw mut account.get_data_mut()?[..], vaddr)
    } else {
        MemoryRegion::new(&raw const account.get_data()[..], vaddr)
    };
    if can_data_be_changed {
        memory_region.access_violation_handler_payload = Some(account.get_index_in_transaction());
    }
    Ok(memory_region)
}

#[expect(dead_code)]
enum SerializeAccount<'a, 'ix_data> {
    Account(IndexOfAccount, BorrowedInstructionAccount<'a, 'ix_data>),
    Duplicate(IndexOfAccount),
}

/// The append-only byte buffer the [`Serializer`] writes the program input into.
///
/// The serializer computes the exact size first and then writes every byte from offset 0 up to
/// that size, in order; only the written prefix (`len()`) is ever exposed, through
/// `as_slice[_mut]` and the memory regions built from it.
trait SerializationBuffer {
    fn len(&self) -> usize;
    fn as_slice(&self) -> &[u8];
    fn as_slice_mut(&mut self) -> &mut [u8];
    /// Appends `num` copies of `value`, failing if capacity would be exceeded.
    fn fill_write(&mut self, num: usize, value: u8) -> std::io::Result<()>;
    /// # Safety
    /// The caller guarantees `len() + size_of::<T>() <= capacity`.
    unsafe fn write_unchecked<T: Pod>(&mut self, value: T);
    /// # Safety
    /// The caller guarantees `len() + value.len() <= capacity`.
    unsafe fn write_all_unchecked(&mut self, value: &[u8]);
}

impl SerializationBuffer for AlignedMemory<HOST_ALIGN> {
    fn len(&self) -> usize {
        AlignedMemory::len(self)
    }
    fn as_slice(&self) -> &[u8] {
        AlignedMemory::as_slice(self)
    }
    fn as_slice_mut(&mut self) -> &mut [u8] {
        AlignedMemory::as_slice_mut(self)
    }
    fn fill_write(&mut self, num: usize, value: u8) -> std::io::Result<()> {
        AlignedMemory::fill_write(self, num, value)
    }
    unsafe fn write_unchecked<T: Pod>(&mut self, value: T) {
        unsafe { AlignedMemory::write_unchecked(self, value) }
    }
    unsafe fn write_all_unchecked(&mut self, value: &[u8]) {
        unsafe { AlignedMemory::write_all_unchecked(self, value) }
    }
}

/// Maximum number of idle parameter buffers kept per thread (one per nesting level).
const MAX_POOLED_PARAMETER_BUFFERS: usize =
    crate::execution_budget::MAX_INSTRUCTION_STACK_DEPTH_SIMD_0268;
/// Parameter buffers larger than this are freed instead of pooled, bounding idle memory at
/// `MAX_POOLED_PARAMETER_BUFFERS * MAX_POOLED_PARAMETER_BUFFER_CAPACITY` per thread.
const MAX_POOLED_PARAMETER_BUFFER_CAPACITY: usize = 4 * 1024 * 1024;
/// Capacities are rounded up to this granularity so that buffers of similar size are reusable.
const PARAMETER_BUFFER_CAPACITY_GRANULARITY: usize = 64 * 1024;

/// An owned, `HOST_ALIGN`-aligned, uninitialized allocation.
struct RawParameterBuffer {
    ptr: NonNull<u8>,
    capacity: usize,
}

impl RawParameterBuffer {
    fn allocate(min_capacity: usize) -> Self {
        let capacity = min_capacity
            .max(1)
            .div_ceil(PARAMETER_BUFFER_CAPACITY_GRANULARITY)
            .saturating_mul(PARAMETER_BUFFER_CAPACITY_GRANULARITY);
        let layout = Layout::from_size_align(capacity, HOST_ALIGN).expect("valid layout");
        // SAFETY: `layout` has a non-zero size.
        let ptr = unsafe { alloc(layout) };
        let Some(ptr) = NonNull::new(ptr) else {
            handle_alloc_error(layout)
        };
        Self { ptr, capacity }
    }
}

impl Drop for RawParameterBuffer {
    fn drop(&mut self) {
        // SAFETY: allocated in `allocate` with exactly this layout.
        unsafe {
            dealloc(
                self.ptr.as_ptr(),
                Layout::from_size_align_unchecked(self.capacity, HOST_ALIGN),
            )
        }
    }
}

thread_local! {
    static PARAMETER_BUFFER_POOL: RefCell<Vec<RawParameterBuffer>> =
        RefCell::new(Vec::with_capacity(MAX_POOLED_PARAMETER_BUFFERS));
}

/// A program-input (parameter) buffer whose allocation is recycled through a per-thread pool
/// (`SOLANA_VM_SER_POOL`, see [`crate::vm_opts::SER_POOL`]).
///
/// Serializing into a recycled allocation avoids a large allocator round trip and fresh page
/// faults per invocation. It is byte-for-byte equivalent to [`AlignedMemory::with_capacity`]:
/// both start empty over uninitialized memory, the serializer writes every byte up to `len`
/// (including the explicit zero fill of the realloc padding, which is always written because
/// this buffer, like `with_capacity`, never assumes zeroed memory), and nothing beyond `len` is
/// ever exposed. Stale bytes from a previous use therefore can never be observed.
pub struct PooledParameterBuffer {
    raw: Option<RawParameterBuffer>,
    len: usize,
}

impl PooledParameterBuffer {
    /// Takes a pooled allocation of at least `size` bytes, or allocates one.
    fn with_capacity(size: usize) -> Self {
        let raw = PARAMETER_BUFFER_POOL
            .try_with(|pool| {
                let mut pool = pool.borrow_mut();
                // Best fit: the smallest idle allocation that is large enough.
                let best_fit = pool
                    .iter()
                    .enumerate()
                    .filter(|(_, raw)| raw.capacity >= size)
                    .min_by_key(|(_, raw)| raw.capacity)
                    .map(|(position, _)| position);
                match best_fit {
                    Some(position) => Some(pool.swap_remove(position)),
                    None => {
                        // Nothing fits: free the most recently returned allocation so that the
                        // larger replacement takes its place instead of growing the pool.
                        pool.pop();
                        None
                    }
                }
            })
            .ok()
            .flatten()
            .unwrap_or_else(|| RawParameterBuffer::allocate(size));
        Self {
            raw: Some(raw),
            len: 0,
        }
    }

    fn raw(&self) -> &RawParameterBuffer {
        self.raw.as_ref().expect("present until drop")
    }

    fn capacity(&self) -> usize {
        self.raw().capacity
    }

    fn ptr(&self) -> *mut u8 {
        self.raw().ptr.as_ptr()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The written bytes.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: the first `len` bytes were initialized by the serializer.
        unsafe { std::slice::from_raw_parts(self.ptr(), self.len) }
    }

    /// The written bytes.
    pub fn as_slice_mut(&mut self) -> &mut [u8] {
        // SAFETY: the first `len` bytes were initialized by the serializer.
        unsafe { std::slice::from_raw_parts_mut(self.ptr(), self.len) }
    }

    /// Returns the allocation to the pool (or frees it when the pool is full or it is large).
    fn recycle(raw: RawParameterBuffer) {
        if raw.capacity > MAX_POOLED_PARAMETER_BUFFER_CAPACITY {
            return;
        }
        let _ = PARAMETER_BUFFER_POOL.try_with(|pool| {
            let mut pool = pool.borrow_mut();
            if pool.len() < MAX_POOLED_PARAMETER_BUFFERS {
                pool.push(raw);
            }
        });
    }

    /// Scribbles over every idle pooled allocation of this thread, so tests can prove that
    /// stale bytes never leak into a later serialization.
    #[cfg(test)]
    pub(crate) fn dirty_pool_for_tests(value: u8) {
        PARAMETER_BUFFER_POOL.with_borrow_mut(|pool| {
            for raw in pool.iter_mut() {
                // SAFETY: the allocation is `capacity` bytes long and not in use.
                unsafe { std::ptr::write_bytes(raw.ptr.as_ptr(), value, raw.capacity) };
            }
        });
    }
}

impl Drop for PooledParameterBuffer {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() {
            Self::recycle(raw);
        }
    }
}

impl SerializationBuffer for PooledParameterBuffer {
    fn len(&self) -> usize {
        self.len
    }
    fn as_slice(&self) -> &[u8] {
        PooledParameterBuffer::as_slice(self)
    }
    fn as_slice_mut(&mut self) -> &mut [u8] {
        PooledParameterBuffer::as_slice_mut(self)
    }
    fn fill_write(&mut self, num: usize, value: u8) -> std::io::Result<()> {
        let new_len = self
            .len
            .checked_add(num)
            .filter(|new_len| *new_len <= self.capacity())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "parameter buffer fill_write failed",
                )
            })?;
        // SAFETY: `len..new_len` is within the allocation.
        unsafe { std::ptr::write_bytes(self.ptr().add(self.len), value, num) };
        self.len = new_len;
        Ok(())
    }
    unsafe fn write_unchecked<T: Pod>(&mut self, value: T) {
        let new_len = self.len.saturating_add(size_of::<T>());
        debug_assert!(new_len <= self.capacity());
        // SAFETY: the caller guarantees capacity.
        unsafe {
            self.ptr().add(self.len).cast::<T>().write_unaligned(value);
        }
        self.len = new_len;
    }
    unsafe fn write_all_unchecked(&mut self, value: &[u8]) {
        let new_len = self.len.saturating_add(value.len());
        debug_assert!(new_len <= self.capacity());
        // SAFETY: the caller guarantees capacity; `value` cannot alias the uniquely owned buffer.
        unsafe {
            std::ptr::copy_nonoverlapping(value.as_ptr(), self.ptr().add(self.len), value.len());
        }
        self.len = new_len;
    }
}

struct Serializer<B: SerializationBuffer> {
    buffer: B,
    regions: Vec<MemoryRegion>,
    vaddr: u64,
    region_start: usize,
    is_loader_v1: bool,
    virtual_address_space_adjustments: bool,
    account_data_direct_mapping: bool,
}

impl<B: SerializationBuffer> Serializer<B> {
    fn new(
        buffer: B,
        start_addr: u64,
        is_loader_v1: bool,
        virtual_address_space_adjustments: bool,
        account_data_direct_mapping: bool,
    ) -> Serializer<B> {
        debug_assert_eq!(buffer.len(), 0);
        Serializer {
            buffer,
            regions: Vec::new(),
            region_start: 0,
            vaddr: start_addr,
            is_loader_v1,
            virtual_address_space_adjustments,
            account_data_direct_mapping,
        }
    }

    fn fill_write(&mut self, num: usize, value: u8) -> std::io::Result<()> {
        self.buffer.fill_write(num, value)
    }

    fn write<T: Pod>(&mut self, value: T) -> u64 {
        self.debug_assert_alignment::<T>();
        let vaddr = self
            .vaddr
            .saturating_add(self.buffer.len() as u64)
            .saturating_sub(self.region_start as u64);
        // Safety:
        // in serialize_parameters_(aligned|unaligned) first we compute the
        // required size then we write into the newly allocated buffer. There's
        // no need to check bounds at every write.
        //
        // AlignedMemory::write_unchecked _does_ debug_assert!() that the capacity
        // is enough, so in the unlikely case we introduce a bug in the size
        // computation, tests will abort.
        unsafe {
            self.buffer.write_unchecked(value);
        }

        vaddr
    }

    fn write_all(&mut self, value: &[u8]) -> u64 {
        let vaddr = self
            .vaddr
            .saturating_add(self.buffer.len() as u64)
            .saturating_sub(self.region_start as u64);
        // Safety:
        // see write() - the buffer is guaranteed to be large enough
        unsafe {
            self.buffer.write_all_unchecked(value);
        }

        vaddr
    }

    fn write_account(
        &mut self,
        account: &mut BorrowedInstructionAccount<'_, '_>,
    ) -> Result<u64, InstructionError> {
        if !self.virtual_address_space_adjustments {
            let vm_data_addr = self.vaddr.saturating_add(self.buffer.len() as u64);
            self.write_all(account.get_data());
            if !self.is_loader_v1 {
                let align_offset =
                    (account.get_data().len() as *const u8).align_offset(BPF_ALIGN_OF_U128);
                self.fill_write(MAX_PERMITTED_DATA_INCREASE + align_offset, 0)
                    .map_err(|_| InstructionError::InvalidArgument)?;
            }
            Ok(vm_data_addr)
        } else {
            self.push_region();
            let vm_data_addr = self.vaddr;
            if !self.account_data_direct_mapping {
                self.write_all(account.get_data());
                if !self.is_loader_v1 {
                    self.fill_write(MAX_PERMITTED_DATA_INCREASE, 0)
                        .map_err(|_| InstructionError::InvalidArgument)?;
                }
            }
            let address_space_reserved_for_account = if !self.is_loader_v1 {
                account
                    .get_data()
                    .len()
                    .saturating_add(MAX_PERMITTED_DATA_INCREASE)
            } else {
                account.get_data().len()
            };
            if address_space_reserved_for_account > 0 {
                if !self.account_data_direct_mapping {
                    self.push_region();
                    let region = self.regions.last_mut().unwrap();
                    modify_memory_region_of_account(account, region);
                } else {
                    let new_region = create_memory_region_of_account(account, self.vaddr)?;
                    self.vaddr += address_space_reserved_for_account as u64;
                    self.regions.push(new_region);
                }
            }
            if !self.is_loader_v1 {
                let align_offset =
                    (account.get_data().len() as *const u8).align_offset(BPF_ALIGN_OF_U128);
                if !self.account_data_direct_mapping {
                    self.fill_write(align_offset, 0)
                        .map_err(|_| InstructionError::InvalidArgument)?;
                } else {
                    // The deserialization code is going to align the vm_addr to
                    // BPF_ALIGN_OF_U128. Always add one BPF_ALIGN_OF_U128 worth of
                    // padding and shift the start of the next region, so that once
                    // vm_addr is aligned, the corresponding host_addr is aligned
                    // too.
                    self.fill_write(BPF_ALIGN_OF_U128, 0)
                        .map_err(|_| InstructionError::InvalidArgument)?;
                    self.region_start += BPF_ALIGN_OF_U128.saturating_sub(align_offset);
                }
            }
            Ok(vm_data_addr)
        }
    }

    fn push_region(&mut self) {
        let range = self.region_start..self.buffer.len();
        let region_slice = self.buffer.as_slice_mut().get_mut(range.clone()).unwrap();
        self.regions
            .push(MemoryRegion::new(&raw mut region_slice[..], self.vaddr));
        self.region_start = range.end;
        self.vaddr += range.len() as u64;
    }

    fn finish(mut self) -> (B, Vec<MemoryRegion>) {
        self.push_region();
        debug_assert_eq!(self.region_start, self.buffer.len());
        (self.buffer, self.regions)
    }

    fn debug_assert_alignment<T>(&self) {
        debug_assert!(
            self.is_loader_v1
                || self
                    .buffer
                    .as_slice()
                    .as_ptr_range()
                    .end
                    .align_offset(mem::align_of::<T>())
                    == 0
        );
    }
}

pub fn serialize_parameters(
    instruction_context: &InstructionContext,
    virtual_address_space_adjustments: bool,
    account_data_direct_mapping: bool,
    direct_account_pointers_in_program_input: bool,
) -> Result<
    (
        AlignedMemory<HOST_ALIGN>,
        Vec<MemoryRegion>,
        Vec<SerializedAccountMetadata>,
        usize,
    ),
    InstructionError,
> {
    serialize_parameters_into(
        instruction_context,
        virtual_address_space_adjustments,
        account_data_direct_mapping,
        direct_account_pointers_in_program_input,
        AlignedMemory::with_capacity,
    )
}

/// [`serialize_parameters`] into a [`PooledParameterBuffer`]: the same bytes, regions,
/// metadata and instruction-data offset, in a recycled allocation.
pub fn serialize_parameters_pooled(
    instruction_context: &InstructionContext,
    virtual_address_space_adjustments: bool,
    account_data_direct_mapping: bool,
    direct_account_pointers_in_program_input: bool,
) -> Result<
    (
        PooledParameterBuffer,
        Vec<MemoryRegion>,
        Vec<SerializedAccountMetadata>,
        usize,
    ),
    InstructionError,
> {
    serialize_parameters_into(
        instruction_context,
        virtual_address_space_adjustments,
        account_data_direct_mapping,
        direct_account_pointers_in_program_input,
        PooledParameterBuffer::with_capacity,
    )
}

#[allow(clippy::type_complexity)]
fn serialize_parameters_into<B: SerializationBuffer>(
    instruction_context: &InstructionContext,
    virtual_address_space_adjustments: bool,
    account_data_direct_mapping: bool,
    direct_account_pointers_in_program_input: bool,
    new_buffer: impl FnOnce(usize) -> B,
) -> Result<(B, Vec<MemoryRegion>, Vec<SerializedAccountMetadata>, usize), InstructionError> {
    let num_ix_accounts = instruction_context.get_number_of_instruction_accounts();
    if num_ix_accounts > MAX_ACCOUNTS_PER_INSTRUCTION as IndexOfAccount {
        return Err(InstructionError::MaxAccountsExceeded);
    }

    let program_id = *instruction_context.get_program_key()?;
    let is_loader_deprecated =
        instruction_context.get_program_owner()? == bpf_loader_deprecated::id();

    let accounts = (0..instruction_context.get_number_of_instruction_accounts())
        .map(|instruction_account_index| {
            if let Some(index) = instruction_context
                .is_instruction_account_duplicate(instruction_account_index)
                .unwrap()
            {
                SerializeAccount::Duplicate(index)
            } else {
                let account = instruction_context
                    .try_borrow_instruction_account(instruction_account_index)
                    .unwrap();
                SerializeAccount::Account(instruction_account_index, account)
            }
        })
        // fun fact: jemalloc is good at caching tiny allocations like this one,
        // so collecting here is actually faster than passing the iterator
        // around, since the iterator does the work to produce its items each
        // time it's iterated on.
        .collect::<Vec<_>>();

    if is_loader_deprecated {
        // Used by loader-v1 (bpf_loader_deprecated)
        serialize_parameters_for_abiv0(
            accounts,
            instruction_context.get_instruction_data(),
            &program_id,
            virtual_address_space_adjustments,
            account_data_direct_mapping,
            new_buffer,
        )
    } else {
        // Used by loader-v2 (bpf_loader) and loader-v3 (bpf_loader_upgradeable)
        serialize_parameters_for_abiv1(
            accounts,
            instruction_context.get_instruction_data(),
            &program_id,
            virtual_address_space_adjustments,
            account_data_direct_mapping,
            // SIMD-0449: only available on ABIv1
            direct_account_pointers_in_program_input,
            new_buffer,
        )
    }
}

pub fn deserialize_parameters(
    instruction_context: &InstructionContext,
    virtual_address_space_adjustments: bool,
    account_data_direct_mapping: bool,
    buffer: &[u8],
    accounts_metadata: &[SerializedAccountMetadata],
) -> Result<(), InstructionError> {
    let is_loader_deprecated =
        instruction_context.get_program_owner()? == bpf_loader_deprecated::id();
    let account_lengths = accounts_metadata.iter().map(|a| a.original_data_len);
    if is_loader_deprecated {
        // Used by loader-v1 (bpf_loader_deprecated)
        deserialize_parameters_for_abiv0(
            instruction_context,
            virtual_address_space_adjustments,
            account_data_direct_mapping,
            buffer,
            account_lengths,
        )
    } else {
        // Used by loader-v2 (bpf_loader) and loader-v3 (bpf_loader_upgradeable)
        deserialize_parameters_for_abiv1(
            instruction_context,
            virtual_address_space_adjustments,
            account_data_direct_mapping,
            buffer,
            account_lengths,
        )
    }
}

#[allow(clippy::type_complexity)]
fn serialize_parameters_for_abiv0<B: SerializationBuffer>(
    accounts: Vec<SerializeAccount>,
    instruction_data: &[u8],
    program_id: &Pubkey,
    virtual_address_space_adjustments: bool,
    account_data_direct_mapping: bool,
    new_buffer: impl FnOnce(usize) -> B,
) -> Result<(B, Vec<MemoryRegion>, Vec<SerializedAccountMetadata>, usize), InstructionError> {
    // Calculate size in order to alloc once
    let mut size = size_of::<u64>();
    for account in &accounts {
        size += 1; // dup
        match account {
            SerializeAccount::Duplicate(_) => {}
            SerializeAccount::Account(_, account) => {
                size += size_of::<u8>() // is_signer
                + size_of::<u8>() // is_writable
                + size_of::<Pubkey>() // key
                + size_of::<u64>()  // lamports
                + size_of::<u64>()  // data len
                + size_of::<Pubkey>() // owner
                + size_of::<u8>() // executable
                + size_of::<u64>(); // rent_epoch
                if !(virtual_address_space_adjustments && account_data_direct_mapping) {
                    size += account.get_data().len();
                }
            }
        }
    }
    size += size_of::<u64>() // instruction data len
         + instruction_data.len() // instruction data
         + size_of::<Pubkey>(); // program id

    let mut s = Serializer::new(
        new_buffer(size),
        MM_INPUT_START,
        true,
        virtual_address_space_adjustments,
        account_data_direct_mapping,
    );

    let mut accounts_metadata: Vec<SerializedAccountMetadata> = Vec::with_capacity(accounts.len());
    s.write::<u64>((accounts.len() as u64).to_le());
    for account in accounts {
        match account {
            SerializeAccount::Duplicate(position) => {
                accounts_metadata.push(accounts_metadata.get(position as usize).unwrap().clone());
                s.write(position as u8);
            }
            SerializeAccount::Account(_, mut account) => {
                let vm_addr = s.write::<u8>(NON_DUP_MARKER);
                s.write::<u8>(account.is_signer() as u8);
                s.write::<u8>(account.is_writable() as u8);
                let vm_key_addr = s.write_all(account.get_key().as_ref());
                let vm_lamports_addr = s.write::<u64>(account.get_lamports().to_le());
                s.write::<u64>((account.get_data().len() as u64).to_le());
                let vm_data_addr = s.write_account(&mut account)?;
                let vm_owner_addr = s.write_all(account.get_owner().as_ref());
                #[expect(deprecated)]
                s.write::<u8>(account.is_executable() as u8);
                let rent_epoch = u64::MAX;
                s.write::<u64>(rent_epoch.to_le());
                accounts_metadata.push(SerializedAccountMetadata {
                    vm_addr,
                    original_data_len: account.get_data().len(),
                    vm_key_addr,
                    vm_lamports_addr,
                    vm_owner_addr,
                    vm_data_addr,
                });
            }
        };
    }
    s.write::<u64>((instruction_data.len() as u64).to_le());
    let instruction_data_offset = s.write_all(instruction_data);
    s.write_all(program_id.as_ref());

    let (mem, regions) = s.finish();
    Ok((
        mem,
        regions,
        accounts_metadata,
        instruction_data_offset as usize,
    ))
}

fn deserialize_parameters_for_abiv0<I: IntoIterator<Item = usize>>(
    instruction_context: &InstructionContext,
    virtual_address_space_adjustments: bool,
    account_data_direct_mapping: bool,
    buffer: &[u8],
    account_lengths: I,
) -> Result<(), InstructionError> {
    let mut start = size_of::<u64>(); // number of accounts
    for (instruction_account_index, pre_len) in
        (0..instruction_context.get_number_of_instruction_accounts()).zip(account_lengths)
    {
        let duplicate =
            instruction_context.is_instruction_account_duplicate(instruction_account_index)?;
        start += 1; // is_dup
        if duplicate.is_none() {
            let mut borrowed_account =
                instruction_context.try_borrow_instruction_account(instruction_account_index)?;
            start += size_of::<u8>(); // is_signer
            start += size_of::<u8>(); // is_writable
            start += size_of::<Pubkey>(); // key
            let lamports = buffer
                .get(start..start.saturating_add(8))
                .map(<[u8; 8]>::try_from)
                .and_then(Result::ok)
                .map(u64::from_le_bytes)
                .ok_or(InstructionError::InvalidArgument)?;
            if borrowed_account.get_lamports() != lamports {
                borrowed_account.set_lamports(lamports)?;
            }
            start += size_of::<u64>() // lamports
                + size_of::<u64>(); // data length
            if !virtual_address_space_adjustments {
                let data = buffer
                    .get(start..start + pre_len)
                    .ok_or(InstructionError::InvalidArgument)?;
                // The redundant check helps to avoid the expensive data comparison if we can
                match borrowed_account.can_data_be_resized(pre_len) {
                    Ok(()) => borrowed_account.set_data_from_slice(data)?,
                    Err(err) if borrowed_account.get_data() != data => return Err(err),
                    _ => {}
                }
            } else if !account_data_direct_mapping && borrowed_account.can_data_be_changed().is_ok()
            {
                let data = buffer
                    .get(start..start + pre_len)
                    .ok_or(InstructionError::InvalidArgument)?;
                borrowed_account.set_data_from_slice(data)?;
            } else if borrowed_account.get_data().len() != pre_len {
                borrowed_account.set_data_length(pre_len)?;
            }
            if !(virtual_address_space_adjustments && account_data_direct_mapping) {
                start += pre_len; // data
            }
            start += size_of::<Pubkey>() // owner
                + size_of::<u8>() // executable
                + size_of::<u64>(); // rent_epoch
        }
    }
    Ok(())
}

#[allow(clippy::type_complexity)]
fn serialize_parameters_for_abiv1<B: SerializationBuffer>(
    accounts: Vec<SerializeAccount>,
    instruction_data: &[u8],
    program_id: &Pubkey,
    virtual_address_space_adjustments: bool,
    account_data_direct_mapping: bool,
    direct_account_pointers_program_input: bool,
    new_buffer: impl FnOnce(usize) -> B,
) -> Result<(B, Vec<MemoryRegion>, Vec<SerializedAccountMetadata>, usize), InstructionError> {
    let mut accounts_metadata = Vec::with_capacity(accounts.len());
    // Calculate size in order to alloc once
    let mut size = size_of::<u64>();
    for account in &accounts {
        size += 1; // dup
        match account {
            SerializeAccount::Duplicate(_) => size += 7, // padding to 64-bit aligned
            SerializeAccount::Account(_, account) => {
                let data_len = account.get_data().len();
                size += size_of::<u8>() // is_signer
                + size_of::<u8>() // is_writable
                + size_of::<u8>() // executable
                + size_of::<u32>() // original_data_len
                + size_of::<Pubkey>()  // key
                + size_of::<Pubkey>() // owner
                + size_of::<u64>()  // lamports
                + size_of::<u64>()  // data len
                + size_of::<u64>(); // rent epoch
                if !(virtual_address_space_adjustments && account_data_direct_mapping) {
                    size += data_len
                        + MAX_PERMITTED_DATA_INCREASE
                        + (data_len as *const u8).align_offset(BPF_ALIGN_OF_U128);
                } else {
                    size += BPF_ALIGN_OF_U128;
                }
            }
        }
    }
    size += size_of::<u64>() // data len
    + instruction_data.len()
    + size_of::<Pubkey>(); // program id;

    // reserve space for account pointer array if SIMD-0449 is enabled
    let account_pointers_offset = if direct_account_pointers_program_input {
        let offset = (size as *const u8).align_offset(BPF_ALIGN_OF_U128);
        size += offset + accounts.len() * size_of::<u64>();
        Some(offset)
    } else {
        None
    };

    let mut s = Serializer::new(
        new_buffer(size),
        MM_INPUT_START,
        false,
        virtual_address_space_adjustments,
        account_data_direct_mapping,
    );

    // Serialize into the buffer
    s.write::<u64>((accounts.len() as u64).to_le());
    for account in accounts {
        match account {
            SerializeAccount::Account(_, mut borrowed_account) => {
                let vm_addr = s.write::<u8>(NON_DUP_MARKER);
                s.write::<u8>(borrowed_account.is_signer() as u8);
                s.write::<u8>(borrowed_account.is_writable() as u8);
                #[expect(deprecated)]
                s.write::<u8>(borrowed_account.is_executable() as u8);
                s.write_all(&[0u8, 0, 0, 0]);
                let vm_key_addr = s.write_all(borrowed_account.get_key().as_ref());
                let vm_owner_addr = s.write_all(borrowed_account.get_owner().as_ref());
                let vm_lamports_addr = s.write::<u64>(borrowed_account.get_lamports().to_le());
                s.write::<u64>((borrowed_account.get_data().len() as u64).to_le());
                let vm_data_addr = s.write_account(&mut borrowed_account)?;
                let rent_epoch = u64::MAX;
                s.write::<u64>(rent_epoch.to_le());
                accounts_metadata.push(SerializedAccountMetadata {
                    vm_addr,
                    original_data_len: borrowed_account.get_data().len(),
                    vm_key_addr,
                    vm_owner_addr,
                    vm_lamports_addr,
                    vm_data_addr,
                });
            }
            SerializeAccount::Duplicate(position) => {
                accounts_metadata.push(accounts_metadata.get(position as usize).unwrap().clone());
                s.write::<u8>(position as u8);
                s.write_all(&[0u8, 0, 0, 0, 0, 0, 0]);
            }
        };
    }
    s.write::<u64>((instruction_data.len() as u64).to_le());
    let instruction_data_offset = s.write_all(instruction_data);
    s.write_all(program_id.as_ref());

    if let Some(offset) = account_pointers_offset {
        // Add padding before the account pointer array to reach 8-byte alignment
        // (BPF_ALIGN_OF_U128).
        s.fill_write(offset, 0)
            .map_err(|_| InstructionError::InvalidArgument)?;
        for entry in accounts_metadata.iter() {
            s.write::<u64>(entry.vm_addr.to_le());
        }
    }

    let (mem, regions) = s.finish();
    Ok((
        mem,
        regions,
        accounts_metadata,
        instruction_data_offset as usize,
    ))
}

fn deserialize_parameters_for_abiv1<I: IntoIterator<Item = usize>>(
    instruction_context: &InstructionContext,
    virtual_address_space_adjustments: bool,
    account_data_direct_mapping: bool,
    buffer: &[u8],
    account_lengths: I,
) -> Result<(), InstructionError> {
    let mut start = size_of::<u64>(); // number of accounts
    for (instruction_account_index, pre_len) in
        (0..instruction_context.get_number_of_instruction_accounts()).zip(account_lengths)
    {
        let duplicate =
            instruction_context.is_instruction_account_duplicate(instruction_account_index)?;
        start += size_of::<u8>(); // position
        if duplicate.is_some() {
            start += 7; // padding to 64-bit aligned
        } else {
            let mut borrowed_account =
                instruction_context.try_borrow_instruction_account(instruction_account_index)?;
            start += size_of::<u8>() // is_signer
                + size_of::<u8>() // is_writable
                + size_of::<u8>() // executable
                + size_of::<u32>() // original_data_len
                + size_of::<Pubkey>(); // key
            let owner = buffer
                .get(start..start + size_of::<Pubkey>())
                .ok_or(InstructionError::InvalidArgument)?;
            start += size_of::<Pubkey>(); // owner
            let lamports = buffer
                .get(start..start.saturating_add(8))
                .map(<[u8; 8]>::try_from)
                .and_then(Result::ok)
                .map(u64::from_le_bytes)
                .ok_or(InstructionError::InvalidArgument)?;
            if borrowed_account.get_lamports() != lamports {
                borrowed_account.set_lamports(lamports)?;
            }
            start += size_of::<u64>(); // lamports
            let post_len = buffer
                .get(start..start.saturating_add(8))
                .map(<[u8; 8]>::try_from)
                .and_then(Result::ok)
                .map(u64::from_le_bytes)
                .ok_or(InstructionError::InvalidArgument)? as usize;
            start += size_of::<u64>(); // data length
            if post_len.saturating_sub(pre_len) > MAX_PERMITTED_DATA_INCREASE
                || post_len > MAX_PERMITTED_DATA_LENGTH as usize
            {
                return Err(InstructionError::InvalidRealloc);
            }
            if !virtual_address_space_adjustments {
                let data = buffer
                    .get(start..start + post_len)
                    .ok_or(InstructionError::InvalidArgument)?;
                // The redundant check helps to avoid the expensive data comparison if we can
                match borrowed_account.can_data_be_resized(post_len) {
                    Ok(()) => borrowed_account.set_data_from_slice(data)?,
                    Err(err) if borrowed_account.get_data() != data => return Err(err),
                    _ => {}
                }
            } else if !account_data_direct_mapping && borrowed_account.can_data_be_changed().is_ok()
            {
                let data = buffer
                    .get(start..start + post_len)
                    .ok_or(InstructionError::InvalidArgument)?;
                borrowed_account.set_data_from_slice(data)?;
            } else if borrowed_account.get_data().len() != post_len {
                borrowed_account.set_data_length(post_len)?;
            }
            start += if !(virtual_address_space_adjustments && account_data_direct_mapping) {
                let alignment_offset = (pre_len as *const u8).align_offset(BPF_ALIGN_OF_U128);
                pre_len // data
                    .saturating_add(MAX_PERMITTED_DATA_INCREASE) // realloc padding
                    .saturating_add(alignment_offset)
            } else {
                // See Serializer::write_account() as to why we have this
                BPF_ALIGN_OF_U128
            };
            start += size_of::<u64>(); // rent_epoch
            if borrowed_account.get_owner().to_bytes() != owner {
                // Change the owner at the end so that we are allowed to change the lamports and data before
                borrowed_account.set_owner(owner)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use {
        super::*,
        crate::with_mock_invoke_context,
        solana_account::{Account, AccountSharedData, ReadableAccount},
        solana_account_info::AccountInfo,
        solana_program_entrypoint::deserialize,
        solana_rent::Rent,
        solana_sbpf::{memory_region::MemoryMapping, program::SBPFVersion, vm::Config},
        solana_sdk_ids::{bpf_loader, bpf_loader_upgradeable},
        solana_system_interface::MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION,
        solana_transaction_context::{
            MAX_ACCOUNTS_PER_TRANSACTION, instruction_accounts::InstructionAccount,
            transaction::TransactionContext,
        },
        std::{
            borrow::Cow,
            cell::RefCell,
            mem::transmute,
            rc::Rc,
            slice::{from_raw_parts, from_raw_parts_mut},
        },
        test_case::test_case,
    };

    fn deduplicated_instruction_accounts(
        transaction_indexes: &[IndexOfAccount],
        is_writable: fn(usize) -> bool,
    ) -> Vec<InstructionAccount> {
        transaction_indexes
            .iter()
            .enumerate()
            .map(|(index_in_instruction, index_in_transaction)| {
                InstructionAccount::new(
                    *index_in_transaction,
                    false,
                    is_writable(index_in_instruction),
                )
            })
            .collect()
    }

    #[test_case(false; "direct_account_pointers_in_program_input disabled")]
    #[test_case(true; "direct_account_pointers_in_program_input enabled")]
    fn test_serialize_parameters_with_many_accounts(
        direct_account_pointers_in_program_input: bool,
    ) {
        struct TestCase {
            num_ix_accounts: usize,
            append_dup_account: bool,
            expected_err: Option<InstructionError>,
            name: &'static str,
        }

        for virtual_address_space_adjustments in [false, true] {
            for TestCase {
                num_ix_accounts,
                append_dup_account,
                expected_err,
                name,
            } in [
                TestCase {
                    name: "serialize max accounts with cap",
                    num_ix_accounts: MAX_ACCOUNTS_PER_INSTRUCTION,
                    append_dup_account: false,
                    expected_err: None,
                },
                TestCase {
                    name: "serialize too many accounts with cap",
                    num_ix_accounts: MAX_ACCOUNTS_PER_INSTRUCTION + 1,
                    append_dup_account: false,
                    expected_err: Some(InstructionError::MaxAccountsExceeded),
                },
                TestCase {
                    name: "serialize too many accounts and append dup with cap",
                    num_ix_accounts: MAX_ACCOUNTS_PER_INSTRUCTION,
                    append_dup_account: true,
                    expected_err: Some(InstructionError::MaxAccountsExceeded),
                },
            ] {
                let program_id = solana_pubkey::new_rand();
                let mut transaction_accounts = vec![(
                    program_id,
                    AccountSharedData::from(Account {
                        lamports: 0,
                        data: vec![],
                        owner: bpf_loader::id(),
                        executable: true,
                        rent_epoch: 0,
                    }),
                )];
                for _ in 0..num_ix_accounts {
                    transaction_accounts.push((
                        Pubkey::new_unique(),
                        AccountSharedData::from(Account {
                            lamports: 0,
                            data: vec![],
                            owner: program_id,
                            executable: false,
                            rent_epoch: 0,
                        }),
                    ));
                }

                let transaction_accounts_indexes: Vec<IndexOfAccount> =
                    (0..num_ix_accounts as u16).collect();
                let mut instruction_accounts =
                    deduplicated_instruction_accounts(&transaction_accounts_indexes, |_| false);
                if append_dup_account {
                    instruction_accounts.push(instruction_accounts.last().cloned().unwrap());
                }
                let instruction_data = vec![];
                let num_transaction_accounts =
                    transaction_accounts.len().min(MAX_ACCOUNTS_PER_TRANSACTION);

                with_mock_invoke_context!(
                    invoke_context,
                    transaction_context,
                    transaction_accounts
                );
                if instruction_accounts.len() > MAX_ACCOUNTS_PER_INSTRUCTION {
                    // Special case implementation of configure_next_instruction_for_tests()
                    // which avoids the overflow when constructing the dedup_map
                    // by simply not filling it.
                    let dedup_map = vec![u16::MAX; num_transaction_accounts];
                    invoke_context
                        .transaction_context
                        .configure_instruction_at_index(
                            0,
                            0,
                            instruction_accounts,
                            dedup_map,
                            Cow::Owned(instruction_data.clone()),
                            Some(0),
                        )
                        .unwrap();
                } else {
                    invoke_context
                        .transaction_context
                        .configure_top_level_instruction_for_tests(
                            0,
                            instruction_accounts,
                            instruction_data.clone(),
                        )
                        .unwrap();
                }
                invoke_context.push().unwrap();
                let instruction_context = invoke_context
                    .transaction_context
                    .get_current_instruction_context()
                    .unwrap();

                let serialization_result = serialize_parameters(
                    &instruction_context,
                    virtual_address_space_adjustments,
                    false, // account_data_direct_mapping
                    direct_account_pointers_in_program_input,
                );
                assert_eq!(
                    serialization_result.as_ref().err(),
                    expected_err.as_ref(),
                    "{name} test case failed",
                );
                if expected_err.is_some() {
                    continue;
                }

                let (mut serialized, regions, _account_lengths, _instruction_data_offset) =
                    serialization_result.unwrap();
                let mut serialized_regions = unsafe {
                    // SAFETY: test code, serialize_parameters should be constructing valid regions.
                    concat_regions(&regions)
                };
                let (de_program_id, de_accounts, de_instruction_data) = unsafe {
                    deserialize(
                        if !virtual_address_space_adjustments {
                            serialized.as_slice_mut()
                        } else {
                            serialized_regions.as_slice_mut()
                        }
                        .first_mut()
                        .unwrap() as *mut u8,
                    )
                };
                assert_eq!(de_program_id, &program_id);
                assert_eq!(de_instruction_data, &instruction_data);
                for account_info in de_accounts {
                    let index_in_transaction = invoke_context
                        .transaction_context
                        .find_index_of_account(account_info.key)
                        .unwrap();
                    let account = invoke_context
                        .transaction_context
                        .accounts()
                        .try_borrow(index_in_transaction)
                        .unwrap();
                    assert_eq!(account.lamports(), account_info.lamports());
                    assert_eq!(account.data(), &account_info.data.borrow()[..]);
                    assert_eq!(account.owner(), account_info.owner);
                    assert_eq!(account.executable(), account_info.executable);
                    #[allow(deprecated)]
                    {
                        // Using the sdk entrypoint, the rent-epoch is skipped
                        assert_eq!(0, account_info._unused);
                    }
                }
            }
        }
    }

    #[test_case(false; "direct_account_pointers_in_program_input disabled")]
    #[test_case(true; "direct_account_pointers_in_program_input enabled")]
    fn test_serialize_parameters(direct_account_pointers_in_program_input: bool) {
        for virtual_address_space_adjustments in [false, true] {
            let program_id = solana_pubkey::new_rand();
            let transaction_accounts = vec![
                (
                    program_id,
                    AccountSharedData::from(Account {
                        lamports: 0,
                        data: vec![],
                        owner: bpf_loader::id(),
                        executable: true,
                        rent_epoch: 0,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 1,
                        data: vec![1u8, 2, 3, 4, 5],
                        owner: bpf_loader::id(),
                        executable: false,
                        rent_epoch: 100,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 2,
                        data: vec![11u8, 12, 13, 14, 15, 16, 17, 18, 19],
                        owner: bpf_loader::id(),
                        executable: true,
                        rent_epoch: 200,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 3,
                        data: vec![],
                        owner: bpf_loader::id(),
                        executable: false,
                        rent_epoch: 3100,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 4,
                        data: vec![1u8, 2, 3, 4, 5],
                        owner: bpf_loader::id(),
                        executable: false,
                        rent_epoch: 100,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 5,
                        data: vec![11u8, 12, 13, 14, 15, 16, 17, 18, 19],
                        owner: bpf_loader::id(),
                        executable: true,
                        rent_epoch: 200,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 6,
                        data: vec![],
                        owner: bpf_loader::id(),
                        executable: false,
                        rent_epoch: 3100,
                    }),
                ),
                (
                    program_id,
                    AccountSharedData::from(Account {
                        lamports: 0,
                        data: vec![],
                        owner: bpf_loader_deprecated::id(),
                        executable: true,
                        rent_epoch: 0,
                    }),
                ),
            ];
            let instruction_accounts =
                deduplicated_instruction_accounts(&[1, 1, 2, 3, 4, 4, 5, 6], |index| index >= 4);
            let instruction_data = vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
            let original_accounts = transaction_accounts.clone();
            with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);
            invoke_context
                .transaction_context
                .configure_top_level_instruction_for_tests(
                    0,
                    instruction_accounts.clone(),
                    instruction_data.clone(),
                )
                .unwrap();
            invoke_context.push().unwrap();
            let instruction_context = invoke_context
                .transaction_context
                .get_current_instruction_context()
                .unwrap();

            // check serialize_parameters_for_abiv1
            let (mut serialized, regions, accounts_metadata, _instruction_data_offset) =
                serialize_parameters(
                    &instruction_context,
                    virtual_address_space_adjustments,
                    false, // account_data_direct_mapping
                    direct_account_pointers_in_program_input,
                )
                .unwrap();

            let mut serialized_regions = unsafe {
                // SAFETY: test code, serialize_parameters should be constructing valid regions.
                concat_regions(&regions)
            };
            if !virtual_address_space_adjustments {
                assert_eq!(serialized.as_slice(), serialized_regions.as_slice());
            }
            let (de_program_id, de_accounts, de_instruction_data) = unsafe {
                deserialize(
                    if !virtual_address_space_adjustments {
                        serialized.as_slice_mut()
                    } else {
                        serialized_regions.as_slice_mut()
                    }
                    .first_mut()
                    .unwrap() as *mut u8,
                )
            };

            assert_eq!(&program_id, de_program_id);
            assert_eq!(instruction_data, de_instruction_data);
            assert_eq!(
                (de_instruction_data.first().unwrap() as *const u8).align_offset(BPF_ALIGN_OF_U128),
                0
            );
            for account_info in de_accounts {
                let index_in_transaction = invoke_context
                    .transaction_context
                    .find_index_of_account(account_info.key)
                    .unwrap();
                let account = invoke_context
                    .transaction_context
                    .accounts()
                    .try_borrow(index_in_transaction)
                    .unwrap();
                assert_eq!(account.lamports(), account_info.lamports());
                assert_eq!(account.data(), &account_info.data.borrow()[..]);
                assert_eq!(account.owner(), account_info.owner);
                assert_eq!(account.executable(), account_info.executable);
                #[allow(deprecated)]
                {
                    // Using the sdk entrypoint, the rent-epoch is skipped
                    assert_eq!(0, account_info._unused);
                }

                assert_eq!(
                    (*account_info.lamports.borrow() as *const u64).align_offset(BPF_ALIGN_OF_U128),
                    0
                );
                assert_eq!(
                    account_info
                        .data
                        .borrow()
                        .as_ptr()
                        .align_offset(BPF_ALIGN_OF_U128),
                    0
                );
            }

            deserialize_parameters(
                &instruction_context,
                virtual_address_space_adjustments,
                false, // account_data_direct_mapping
                serialized.as_slice(),
                &accounts_metadata,
            )
            .unwrap();
            for (index_in_transaction, (_key, original_account)) in
                original_accounts.iter().enumerate()
            {
                let account = invoke_context
                    .transaction_context
                    .accounts()
                    .try_borrow(index_in_transaction as IndexOfAccount)
                    .unwrap();
                assert_eq!(&*account, original_account);
            }

            invoke_context.pop().unwrap();
            // check serialize_parameters_for_abiv0
            invoke_context
                .transaction_context
                .configure_top_level_instruction_for_tests(
                    7,
                    instruction_accounts,
                    instruction_data.clone(),
                )
                .unwrap();
            invoke_context.push().unwrap();
            let instruction_context = invoke_context
                .transaction_context
                .get_current_instruction_context()
                .unwrap();

            let (mut serialized, regions, account_lengths, _instruction_data_offset) =
                serialize_parameters(
                    &instruction_context,
                    virtual_address_space_adjustments,
                    false, // account_data_direct_mapping
                    direct_account_pointers_in_program_input,
                )
                .unwrap();
            let mut serialized_regions = unsafe {
                // SAFETY: test code, serialize_parameters should be constructing valid regions.
                concat_regions(&regions)
            };

            let (de_program_id, de_accounts, de_instruction_data) = unsafe {
                deserialize_for_abiv0(
                    if !virtual_address_space_adjustments {
                        serialized.as_slice_mut()
                    } else {
                        serialized_regions.as_slice_mut()
                    }
                    .first_mut()
                    .unwrap() as *mut u8,
                )
            };
            assert_eq!(&program_id, de_program_id);
            assert_eq!(instruction_data, de_instruction_data);
            for account_info in de_accounts {
                let index_in_transaction = invoke_context
                    .transaction_context
                    .find_index_of_account(account_info.key)
                    .unwrap();
                let account = invoke_context
                    .transaction_context
                    .accounts()
                    .try_borrow(index_in_transaction)
                    .unwrap();
                assert_eq!(account.lamports(), account_info.lamports());
                assert_eq!(account.data(), &account_info.data.borrow()[..]);
                assert_eq!(account.owner(), account_info.owner);
                assert_eq!(account.executable(), account_info.executable);
                #[allow(deprecated)]
                {
                    assert_eq!(u64::MAX, account_info._unused);
                }
            }

            deserialize_parameters(
                &instruction_context,
                virtual_address_space_adjustments,
                false, // account_data_direct_mapping
                serialized.as_slice(),
                &account_lengths,
            )
            .unwrap();
            for (index_in_transaction, (_key, original_account)) in
                original_accounts.iter().enumerate()
            {
                let account = invoke_context
                    .transaction_context
                    .accounts()
                    .try_borrow(index_in_transaction as IndexOfAccount)
                    .unwrap();
                assert_eq!(&*account, original_account);
            }
        }
    }

    #[test_case(false; "direct_account_pointers_in_program_input disabled")]
    #[test_case(true; "direct_account_pointers_in_program_input enabled")]
    fn test_serialize_parameters_mask_out_rent_epoch_in_vm_serialization(
        direct_account_pointers_in_program_input: bool,
    ) {
        let transaction_accounts = vec![
            (
                solana_pubkey::new_rand(),
                AccountSharedData::from(Account {
                    lamports: 0,
                    data: vec![],
                    owner: bpf_loader::id(),
                    executable: true,
                    rent_epoch: 0,
                }),
            ),
            (
                solana_pubkey::new_rand(),
                AccountSharedData::from(Account {
                    lamports: 1,
                    data: vec![1u8, 2, 3, 4, 5],
                    owner: bpf_loader::id(),
                    executable: false,
                    rent_epoch: 100,
                }),
            ),
            (
                solana_pubkey::new_rand(),
                AccountSharedData::from(Account {
                    lamports: 2,
                    data: vec![11u8, 12, 13, 14, 15, 16, 17, 18, 19],
                    owner: bpf_loader::id(),
                    executable: true,
                    rent_epoch: 200,
                }),
            ),
            (
                solana_pubkey::new_rand(),
                AccountSharedData::from(Account {
                    lamports: 3,
                    data: vec![],
                    owner: bpf_loader::id(),
                    executable: false,
                    rent_epoch: 300,
                }),
            ),
            (
                solana_pubkey::new_rand(),
                AccountSharedData::from(Account {
                    lamports: 4,
                    data: vec![1u8, 2, 3, 4, 5],
                    owner: bpf_loader::id(),
                    executable: false,
                    rent_epoch: 100,
                }),
            ),
            (
                solana_pubkey::new_rand(),
                AccountSharedData::from(Account {
                    lamports: 5,
                    data: vec![11u8, 12, 13, 14, 15, 16, 17, 18, 19],
                    owner: bpf_loader::id(),
                    executable: true,
                    rent_epoch: 200,
                }),
            ),
            (
                solana_pubkey::new_rand(),
                AccountSharedData::from(Account {
                    lamports: 6,
                    data: vec![],
                    owner: bpf_loader::id(),
                    executable: false,
                    rent_epoch: 3100,
                }),
            ),
            (
                solana_pubkey::new_rand(),
                AccountSharedData::from(Account {
                    lamports: 0,
                    data: vec![],
                    owner: bpf_loader_deprecated::id(),
                    executable: true,
                    rent_epoch: 0,
                }),
            ),
        ];
        let instruction_accounts =
            deduplicated_instruction_accounts(&[1, 1, 2, 3, 4, 4, 5, 6], |index| index >= 4);
        with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);
        invoke_context
            .transaction_context
            .configure_top_level_instruction_for_tests(0, instruction_accounts.clone(), vec![])
            .unwrap();
        invoke_context.push().unwrap();
        let instruction_context = invoke_context
            .transaction_context
            .get_current_instruction_context()
            .unwrap();

        // check serialize_parameters_for_abiv1
        let (_serialized, regions, _accounts_metadata, _instruction_data_offset) =
            serialize_parameters(
                &instruction_context,
                true,
                false, // account_data_direct_mapping
                direct_account_pointers_in_program_input,
            )
            .unwrap();

        let mut serialized_regions = unsafe {
            // SAFETY: test code, serialize_parameters should be constructing valid regions.
            concat_regions(&regions)
        };
        let (_de_program_id, de_accounts, _de_instruction_data) = unsafe {
            deserialize(serialized_regions.as_slice_mut().first_mut().unwrap() as *mut u8)
        };

        for account_info in de_accounts {
            // Using program-entrypoint, the rent-epoch will always be 0
            #[allow(deprecated)]
            {
                assert_eq!(0, account_info._unused);
            }
        }

        // check serialize_parameters_for_abiv0
        invoke_context
            .transaction_context
            .configure_top_level_instruction_for_tests(7, instruction_accounts, vec![])
            .unwrap();
        invoke_context.push().unwrap();
        let instruction_context = invoke_context
            .transaction_context
            .get_current_instruction_context()
            .unwrap();

        let (_serialized, regions, _account_lengths, _instruction_data_offset) =
            serialize_parameters(
                &instruction_context,
                true,
                false, // account_data_direct_mapping
                direct_account_pointers_in_program_input,
            )
            .unwrap();
        let mut serialized_regions = unsafe {
            // SAFETY: test code, serialize_parameters should be constructing valid regions.
            concat_regions(&regions)
        };

        let (_de_program_id, de_accounts, _de_instruction_data) = unsafe {
            deserialize_for_abiv0(serialized_regions.as_slice_mut().first_mut().unwrap() as *mut u8)
        };
        for account_info in de_accounts {
            #[allow(deprecated)]
            {
                assert_eq!(account_info._unused, u64::MAX);
            }
        }
    }

    // the old bpf_loader in-program deserializer bpf_loader::id()
    #[deny(unsafe_op_in_unsafe_fn)]
    unsafe fn deserialize_for_abiv0<'a>(
        input: *mut u8,
    ) -> (&'a Pubkey, Vec<AccountInfo<'a>>, &'a [u8]) {
        // this boring boilerplate struct is needed until inline const...
        struct Ptr<T>(std::marker::PhantomData<T>);
        impl<T> Ptr<T> {
            const COULD_BE_UNALIGNED: bool = std::mem::align_of::<T>() > 1;

            #[inline(always)]
            fn read_possibly_unaligned(input: *mut u8, offset: usize) -> T {
                unsafe {
                    let src = input.add(offset) as *const T;
                    if Self::COULD_BE_UNALIGNED {
                        src.read_unaligned()
                    } else {
                        src.read()
                    }
                }
            }

            // rustc inserts debug_assert! for misaligned pointer dereferences when
            // deserializing, starting from [1]. so, use std::mem::transmute as the last resort
            // while preventing clippy from complaining to suggest not to use it.
            // [1]: https://github.com/rust-lang/rust/commit/22a7a19f9333bc1fcba97ce444a3515cb5fb33e6
            // as for the ub nature of the misaligned pointer dereference, this is
            // acceptable in this code, given that this is cfg(test) and it's cared only with
            // x86-64 and the target only incurs some performance penalty, not like segfaults
            // in other targets.
            #[inline(always)]
            fn ref_possibly_unaligned<'a>(input: *mut u8, offset: usize) -> &'a T {
                #[allow(clippy::transmute_ptr_to_ref)]
                unsafe {
                    transmute(input.add(offset) as *const T)
                }
            }

            // See ref_possibly_unaligned's comment
            #[inline(always)]
            fn mut_possibly_unaligned<'a>(input: *mut u8, offset: usize) -> &'a mut T {
                #[allow(clippy::transmute_ptr_to_ref)]
                unsafe {
                    transmute(input.add(offset) as *mut T)
                }
            }
        }

        let mut offset: usize = 0;

        // number of accounts present

        let num_accounts = Ptr::<u64>::read_possibly_unaligned(input, offset) as usize;
        offset += size_of::<u64>();

        // account Infos

        let mut accounts = Vec::with_capacity(num_accounts);
        for _ in 0..num_accounts {
            let dup_info = Ptr::<u8>::read_possibly_unaligned(input, offset);
            offset += size_of::<u8>();
            if dup_info == NON_DUP_MARKER {
                let is_signer = Ptr::<u8>::read_possibly_unaligned(input, offset) != 0;
                offset += size_of::<u8>();

                let is_writable = Ptr::<u8>::read_possibly_unaligned(input, offset) != 0;
                offset += size_of::<u8>();

                let key = Ptr::<Pubkey>::ref_possibly_unaligned(input, offset);
                offset += size_of::<Pubkey>();

                let lamports = Rc::new(RefCell::new(Ptr::mut_possibly_unaligned(input, offset)));
                offset += size_of::<u64>();

                let data_len = Ptr::<u64>::read_possibly_unaligned(input, offset) as usize;
                offset += size_of::<u64>();

                let data = Rc::new(RefCell::new(unsafe {
                    from_raw_parts_mut(input.add(offset), data_len)
                }));
                offset += data_len;

                let owner: &Pubkey = Ptr::<Pubkey>::ref_possibly_unaligned(input, offset);
                offset += size_of::<Pubkey>();

                let executable = Ptr::<u8>::read_possibly_unaligned(input, offset) != 0;
                offset += size_of::<u8>();

                let unused = Ptr::<u64>::read_possibly_unaligned(input, offset);
                offset += size_of::<u64>();

                #[allow(deprecated)]
                accounts.push(AccountInfo {
                    key,
                    is_signer,
                    is_writable,
                    lamports,
                    data,
                    owner,
                    executable,
                    _unused: unused,
                });
            } else {
                // duplicate account, clone the original
                accounts.push(accounts.get(dup_info as usize).unwrap().clone());
            }
        }

        // instruction data

        let instruction_data_len = Ptr::<u64>::read_possibly_unaligned(input, offset) as usize;
        offset += size_of::<u64>();

        let instruction_data = unsafe { from_raw_parts(input.add(offset), instruction_data_len) };
        offset += instruction_data_len;

        // program Id

        let program_id = Ptr::<Pubkey>::ref_possibly_unaligned(input, offset);

        (program_id, accounts, instruction_data)
    }

    /// # Safety
    ///
    /// All memory regions must be pointing to valid to dereference host buffers.
    unsafe fn concat_regions(regions: &[MemoryRegion]) -> AlignedMemory<HOST_ALIGN> {
        let last_region = regions.last().unwrap();
        let last_region_vm_addr = last_region.vm_addr_range().start;
        let mut mem = AlignedMemory::zero_filled(
            (last_region_vm_addr - MM_INPUT_START + last_region.len() as u64) as usize,
        );
        for region in regions {
            let vm_start = region.vm_addr_range().start;
            let buffer = region.host_buffer().ptr();
            mem.as_slice_mut()[(vm_start - MM_INPUT_START) as usize..][..buffer.len()]
                .copy_from_slice(unsafe {
                    // SAFETY:
                    // Contract from `<*const [u8]>::as_ref_unchecked`: ensure that the pointer is
                    // convertible to reference.
                    // Evidence: The contract delegated to the callers.
                    buffer.as_ref_unchecked()
                })
        }
        mem
    }

    #[test]
    fn test_access_violation_handler() {
        let program_id = Pubkey::new_unique();
        let shared_account = AccountSharedData::new(0, 4, &program_id);
        let mut transaction_context = TransactionContext::new(
            vec![
                (
                    Pubkey::new_unique(),
                    AccountSharedData::new(0, 4, &program_id),
                ), // readonly
                (Pubkey::new_unique(), shared_account.clone()), // writable shared
                (
                    Pubkey::new_unique(),
                    AccountSharedData::new(0, 0, &program_id),
                ), // another writable account
                (
                    Pubkey::new_unique(),
                    AccountSharedData::new(
                        0,
                        MAX_PERMITTED_DATA_LENGTH as usize - 0x100,
                        &program_id,
                    ),
                ), // almost max sized writable account
                (
                    Pubkey::new_unique(),
                    AccountSharedData::new(0, 0, &program_id),
                ), // writable dummy to burn accounts_resize_delta
                (
                    Pubkey::new_unique(),
                    AccountSharedData::new(0, 0x3000, &program_id),
                ), // writable dummy to burn accounts_resize_delta
                (program_id, AccountSharedData::default()),     // program
            ],
            Rent::default(),
            /* max_instruction_stack_depth */ 1,
            /* max_instruction_trace_length */ 1,
            /* number_of_top_level_instructions */ 1,
        );
        let transaction_accounts_indexes = [0, 1, 2, 3, 4, 5];
        let instruction_accounts =
            deduplicated_instruction_accounts(&transaction_accounts_indexes, |index| index > 0);
        transaction_context
            .configure_top_level_instruction_for_tests(6, instruction_accounts, vec![])
            .unwrap();
        transaction_context.push().unwrap();
        let instruction_context = transaction_context
            .get_current_instruction_context()
            .unwrap();
        let account_start_offsets = [
            MM_INPUT_START,
            MM_INPUT_START + 4 + MAX_PERMITTED_DATA_INCREASE as u64,
            MM_INPUT_START + (4 + MAX_PERMITTED_DATA_INCREASE as u64) * 2,
            MM_INPUT_START + (4 + MAX_PERMITTED_DATA_INCREASE as u64) * 3,
        ];
        let regions = account_start_offsets
            .iter()
            .enumerate()
            .map(|(index_in_instruction, account_start_offset)| {
                create_memory_region_of_account(
                    &mut instruction_context
                        .try_borrow_instruction_account(index_in_instruction as IndexOfAccount)
                        .unwrap(),
                    *account_start_offset,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let mut memory_mapping = unsafe {
            MemoryMapping::new_with_access_violation_handler(
                regions,
                &config,
                SBPFVersion::V3,
                transaction_context.access_violation_handler(true, true),
            )
            .unwrap()
        };

        // Reading readonly account is allowed
        memory_mapping
            .load::<u32>(account_start_offsets[0])
            .unwrap();

        // Reading writable account is allowed
        memory_mapping
            .load::<u32>(account_start_offsets[1])
            .unwrap();

        // Reading beyond readonly accounts current size is denied
        memory_mapping
            .load::<u32>(account_start_offsets[0] + 4)
            .unwrap_err();

        // Writing to readonly account is denied
        memory_mapping
            .store::<u32>(0, account_start_offsets[0])
            .unwrap_err();

        // Writing to shared writable account makes it unique (CoW logic.)
        // It has been previously been made non-unique at the beginning of
        // the test through a clone.
        let _shared_account_ref = shared_account;
        assert!(
            transaction_context
                .accounts()
                .try_borrow_mut(1)
                .unwrap()
                .is_shared()
        );
        memory_mapping
            .store::<u32>(0, account_start_offsets[1])
            .unwrap();
        assert!(
            !transaction_context
                .accounts()
                .try_borrow_mut(1)
                .unwrap()
                .is_shared()
        );
        assert_eq!(
            transaction_context
                .accounts()
                .try_borrow(1)
                .unwrap()
                .data()
                .len(),
            4,
        );

        // Reading beyond writable accounts current size grows is denied
        memory_mapping
            .load::<u32>(account_start_offsets[1] + 4)
            .unwrap_err();

        // Writing beyond writable accounts current size grows it
        // to original length plus MAX_PERMITTED_DATA_INCREASE
        memory_mapping
            .store::<u32>(0, account_start_offsets[1] + 4)
            .unwrap();
        assert_eq!(
            transaction_context
                .accounts()
                .try_borrow(1)
                .unwrap()
                .data()
                .len(),
            4 + MAX_PERMITTED_DATA_INCREASE,
        );
        assert!(
            transaction_context
                .accounts()
                .try_borrow(1)
                .unwrap()
                .data()
                .len()
                < 0x3000
        );

        // Writing beyond almost max sized writable accounts current size only grows it
        // to MAX_PERMITTED_DATA_LENGTH
        memory_mapping
            .store::<u32>(0, account_start_offsets[3] + MAX_PERMITTED_DATA_LENGTH - 4)
            .unwrap();
        assert_eq!(
            transaction_context
                .accounts()
                .try_borrow(3)
                .unwrap()
                .data()
                .len(),
            MAX_PERMITTED_DATA_LENGTH as usize,
        );

        // Accessing the rest of the address space reserved for
        // the almost max sized writable account is denied
        memory_mapping
            .load::<u32>(account_start_offsets[3] + MAX_PERMITTED_DATA_LENGTH)
            .unwrap_err();
        memory_mapping
            .store::<u32>(0, account_start_offsets[3] + MAX_PERMITTED_DATA_LENGTH)
            .unwrap_err();

        // Burn through most of the accounts_resize_delta budget
        let remaining_allowed_growth: usize = 0x700;
        for index_in_instruction in 4..6 {
            let mut borrowed_account = instruction_context
                .try_borrow_instruction_account(index_in_instruction)
                .unwrap();
            borrowed_account
                .set_data_from_slice(&vec![0u8; MAX_PERMITTED_DATA_LENGTH as usize])
                .unwrap();
        }
        assert_eq!(
            transaction_context.accounts().resize_delta(),
            MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION
                - remaining_allowed_growth as i64,
        );

        // Writing beyond empty writable accounts current size
        // only grows it to fill up MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION
        memory_mapping
            .store::<u32>(0, account_start_offsets[2] + 0x500)
            .unwrap();
        assert_eq!(
            transaction_context
                .accounts()
                .try_borrow(2)
                .unwrap()
                .data()
                .len(),
            remaining_allowed_growth,
        );
    }

    #[test]
    fn test_regression_initial_serialized_account_region_does_not_include_resize_affordance() {
        let program_id = Pubkey::new_unique();
        let transaction_accounts = vec![
            (
                Pubkey::new_unique(),
                AccountSharedData::new(0, 4, &program_id),
            ),
            (
                solana_pubkey::new_rand(),
                AccountSharedData::from(Account {
                    lamports: 0,
                    data: b"agave".into(),
                    owner: bpf_loader_upgradeable::id(),
                    executable: false,
                    rent_epoch: 0,
                }),
            ),
        ];
        with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);
        invoke_context
            .transaction_context
            .configure_top_level_instruction_for_tests(
                0,
                vec![InstructionAccount::new(1, false, true)],
                vec![],
            )
            .unwrap();
        invoke_context.push().unwrap();
        let instruction_context = invoke_context
            .transaction_context
            .get_current_instruction_context()
            .unwrap();
        let (_serialized, regions, accounts_metadata, _instruction_data_offset) =
            crate::serialization::serialize_parameters(
                &instruction_context,
                true,  // virtual_address_space_adjustments
                false, // account_data_direct_mapping
                false, // direct_account_pointers_in_program_input
            )
            .unwrap();
        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let memory_mapping =
            unsafe { MemoryMapping::new(regions, &config, SBPFVersion::V3).unwrap() };
        let account_metadata = &accounts_metadata[0];
        let vm_data_addr = account_metadata.vm_data_addr;
        let (_region_index, region) = memory_mapping.find_region(vm_data_addr).unwrap();
        assert_eq!(region.len(), 5);
    }

    /// Everything a program (or deserialization) can observe of one serialization: the bytes,
    /// every region (vm range, length, writability, payload, where its host memory lies relative
    /// to the buffer, and its content), the account metadata and the instruction-data offset.
    /// (vm range, len, gap size, writable, payload, host location, content)
    type ObservedRegion = (
        std::ops::Range<u64>,
        usize,
        u64,
        bool,
        Option<u16>,
        String,
        Vec<u8>,
    );

    #[derive(Debug, PartialEq, Eq)]
    struct ObservedSerialization {
        bytes: Vec<u8>,
        regions: Vec<ObservedRegion>,
        metadata: Vec<(u64, usize, u64, u64, u64, u64)>,
        instruction_data_offset: usize,
    }

    fn observe(
        bytes: &[u8],
        regions: &[MemoryRegion],
        metadata: &[SerializedAccountMetadata],
        instruction_data_offset: usize,
    ) -> ObservedSerialization {
        let base = bytes.as_ptr() as usize;
        ObservedSerialization {
            bytes: bytes.to_vec(),
            regions: regions
                .iter()
                .map(|region| {
                    let host = region.host_buffer();
                    let ptr = host.ptr() as *const u8 as usize;
                    // Regions either point into the serialization buffer (compare offsets) or
                    // into account storage (identical pointers in both runs).
                    let location = if ptr >= base && ptr <= base + bytes.len() {
                        format!("buffer+{}", ptr - base)
                    } else {
                        format!("external {ptr:#x}")
                    };
                    let content = unsafe {
                        // SAFETY: test code, regions point at live memory of `len` bytes.
                        std::slice::from_raw_parts(ptr as *const u8, region.len()).to_vec()
                    };
                    (
                        region.vm_addr_range(),
                        region.len(),
                        region.gap_size(),
                        host.is_mutable(),
                        region.access_violation_handler_payload,
                        location,
                        content,
                    )
                })
                .collect(),
            metadata: metadata
                .iter()
                .map(|m| {
                    (
                        m.vm_addr,
                        m.original_data_len,
                        m.vm_data_addr,
                        m.vm_key_addr,
                        m.vm_lamports_addr,
                        m.vm_owner_addr,
                    )
                })
                .collect(),
            instruction_data_offset,
        }
    }

    /// Serializes the current instruction with the stock allocation and with a recycled pooled
    /// allocation whose idle bytes were scribbled over, and asserts identical observations.
    fn assert_pooled_serialization_is_identical(
        instruction_context: &InstructionContext,
        virtual_address_space_adjustments: bool,
        account_data_direct_mapping: bool,
        direct_account_pointers_in_program_input: bool,
    ) {
        let stock = serialize_parameters(
            instruction_context,
            virtual_address_space_adjustments,
            account_data_direct_mapping,
            direct_account_pointers_in_program_input,
        )
        .map(|(buffer, regions, metadata, offset)| {
            observe(buffer.as_slice(), &regions, &metadata, offset)
        });
        for scribble in [0xa5, 0xff, 0x00] {
            PooledParameterBuffer::dirty_pool_for_tests(scribble);
            let pooled = serialize_parameters_pooled(
                instruction_context,
                virtual_address_space_adjustments,
                account_data_direct_mapping,
                direct_account_pointers_in_program_input,
            )
            .map(|(buffer, regions, metadata, offset)| {
                observe(buffer.as_slice(), &regions, &metadata, offset)
            });
            assert_eq!(stock, pooled);
        }
    }

    fn account_with_data(len: usize, owner: Pubkey, executable: bool) -> AccountSharedData {
        AccountSharedData::from(Account {
            lamports: len as u64 + 1,
            data: (0..len).map(|i| (i % 251) as u8).collect(),
            owner,
            executable,
            rent_epoch: 0,
        })
    }

    #[test]
    fn test_serialize_parameters_pooled_is_identical() {
        // Data lengths cover every u128 alignment residue, empty accounts and accounts larger
        // than the pool granularity; the order of the cases makes later (smaller) serializations
        // reuse allocations that previously held larger, different inputs.
        let data_lens: [&[usize]; 5] = [
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 15, 16, 17],
            &[200_000, 3, 0],
            &[33, 10_240, 10_241],
            &[],
            &[1_000_000],
        ];
        for loader_id in [
            bpf_loader::id(),
            bpf_loader_upgradeable::id(),
            bpf_loader_deprecated::id(),
        ] {
            for lens in data_lens {
                for virtual_address_space_adjustments in [false, true] {
                    for account_data_direct_mapping in [false, true] {
                        if account_data_direct_mapping && !virtual_address_space_adjustments {
                            continue;
                        }
                        for direct_account_pointers_in_program_input in [false, true] {
                            let program_id = Pubkey::new_unique();
                            let mut transaction_accounts =
                                vec![(program_id, account_with_data(0, loader_id, true))];
                            for (i, len) in lens.iter().enumerate() {
                                transaction_accounts.push((
                                    Pubkey::new_unique(),
                                    account_with_data(*len, program_id, i % 3 == 2),
                                ));
                            }
                            let mut indexes: Vec<IndexOfAccount> =
                                (1..transaction_accounts.len() as IndexOfAccount).collect();
                            // Duplicates of the first and last account.
                            if let (Some(first), Some(last)) =
                                (indexes.first().copied(), indexes.last().copied())
                            {
                                indexes.push(first);
                                indexes.push(last);
                            }
                            let instruction_accounts =
                                deduplicated_instruction_accounts(&indexes, |i| i % 2 == 0);
                            with_mock_invoke_context!(
                                invoke_context,
                                transaction_context,
                                transaction_accounts
                            );
                            invoke_context
                                .transaction_context
                                .configure_top_level_instruction_for_tests(
                                    0,
                                    instruction_accounts,
                                    vec![1, 2, 3, 4, 5, 6, 7],
                                )
                                .unwrap();
                            invoke_context.push().unwrap();
                            let instruction_context = invoke_context
                                .transaction_context
                                .get_current_instruction_context()
                                .unwrap();
                            assert_pooled_serialization_is_identical(
                                &instruction_context,
                                virtual_address_space_adjustments,
                                account_data_direct_mapping,
                                direct_account_pointers_in_program_input,
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn test_serialize_parameters_pooled_error_is_identical() {
        // Too many instruction accounts fails before any buffer is taken, identically.
        let program_id = Pubkey::new_unique();
        let mut transaction_accounts =
            vec![(program_id, account_with_data(0, bpf_loader::id(), true))];
        for _ in 0..MAX_ACCOUNTS_PER_INSTRUCTION {
            transaction_accounts.push((
                Pubkey::new_unique(),
                account_with_data(0, program_id, false),
            ));
        }
        let num_transaction_accounts = transaction_accounts.len();
        let mut instruction_accounts = deduplicated_instruction_accounts(
            &(1..num_transaction_accounts as IndexOfAccount).collect::<Vec<_>>(),
            |_| false,
        );
        instruction_accounts.push(instruction_accounts.last().cloned().unwrap());
        with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);
        invoke_context
            .transaction_context
            .configure_instruction_at_index(
                0,
                0,
                instruction_accounts,
                vec![u16::MAX; num_transaction_accounts],
                Cow::Owned(vec![]),
                Some(0),
            )
            .unwrap();
        invoke_context.push().unwrap();
        let instruction_context = invoke_context
            .transaction_context
            .get_current_instruction_context()
            .unwrap();
        assert_eq!(
            serialize_parameters(&instruction_context, false, false, false).err(),
            Some(InstructionError::MaxAccountsExceeded)
        );
        assert_eq!(
            serialize_parameters_pooled(&instruction_context, false, false, false).err(),
            Some(InstructionError::MaxAccountsExceeded)
        );
    }

    #[test]
    fn test_parameter_buffer_pool_recycles_and_bounds() {
        // Start from an empty pool (tests may share a thread with --test-threads=1).
        PARAMETER_BUFFER_POOL.with_borrow_mut(Vec::clear);
        // A dropped buffer's allocation is reused by the next request that fits.
        let first = PooledParameterBuffer::with_capacity(100_000);
        let first_ptr = first.ptr();
        let first_capacity = first.capacity();
        assert!(first_capacity >= 100_000);
        assert_eq!(first_capacity % PARAMETER_BUFFER_CAPACITY_GRANULARITY, 0);
        assert_eq!(first_ptr as usize % HOST_ALIGN, 0);
        drop(first);
        let second = PooledParameterBuffer::with_capacity(90_000);
        assert_eq!(second.ptr(), first_ptr);
        assert!(second.is_empty());
        drop(second);
        // Oversized buffers are not retained.
        let huge = PooledParameterBuffer::with_capacity(MAX_POOLED_PARAMETER_BUFFER_CAPACITY + 1);
        drop(huge);
        PARAMETER_BUFFER_POOL.with_borrow(|pool| {
            assert!(pool.len() <= MAX_POOLED_PARAMETER_BUFFERS);
            assert!(
                pool.iter()
                    .all(|raw| raw.capacity <= MAX_POOLED_PARAMETER_BUFFER_CAPACITY)
            );
        });
        // Nested (simultaneously live) buffers never share an allocation, and the pool never
        // holds more than one allocation per nesting level.
        let nested: Vec<_> = (0..MAX_POOLED_PARAMETER_BUFFERS + 2)
            .map(|i| PooledParameterBuffer::with_capacity(1000 * (i + 1)))
            .collect();
        let mut ptrs: Vec<_> = nested.iter().map(|b| b.ptr() as usize).collect();
        ptrs.sort_unstable();
        ptrs.dedup();
        assert_eq!(ptrs.len(), nested.len());
        drop(nested);
        PARAMETER_BUFFER_POOL
            .with_borrow(|pool| assert_eq!(pool.len(), MAX_POOLED_PARAMETER_BUFFERS));
        // A request larger than every idle allocation replaces one instead of growing the pool.
        let big = PooledParameterBuffer::with_capacity(3 * 1024 * 1024);
        drop(big);
        PARAMETER_BUFFER_POOL.with_borrow(|pool| {
            assert_eq!(pool.len(), MAX_POOLED_PARAMETER_BUFFERS);
            assert!(pool.iter().any(|raw| raw.capacity >= 3 * 1024 * 1024));
        });
    }

    #[test]
    fn test_pooled_fill_write_bounds() {
        let mut buffer = PooledParameterBuffer::with_capacity(16);
        let capacity = buffer.capacity();
        buffer.fill_write(capacity - 1, 0).unwrap();
        assert!(buffer.fill_write(2, 0).is_err());
        assert_eq!(buffer.len(), capacity - 1);
        buffer.fill_write(1, 9).unwrap();
        assert_eq!(buffer.as_slice().last(), Some(&9));
        assert!(buffer.as_slice()[..capacity - 1].iter().all(|b| *b == 0));
    }

    /// Microbenchmark (run with `--release -- --ignored --nocapture`): serializing a
    /// PumpSwap-like input (26 accounts, one 105 KB) with fresh vs pooled allocations.
    #[test]
    #[ignore]
    fn bench_serialize_parameters_pooled() {
        const ITERATIONS: u32 = 2_000;
        let program_id = Pubkey::new_unique();
        let mut transaction_accounts =
            vec![(program_id, account_with_data(0, bpf_loader::id(), true))];
        for i in 0..26 {
            let len = match i {
                0 => 105_000,
                1..=6 => 165,
                7..=12 => 82,
                _ => 0,
            };
            transaction_accounts.push((
                Pubkey::new_unique(),
                account_with_data(len, program_id, false),
            ));
        }
        let indexes: Vec<IndexOfAccount> =
            (1..transaction_accounts.len() as IndexOfAccount).collect();
        let instruction_accounts = deduplicated_instruction_accounts(&indexes, |i| i % 2 == 0);
        with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);
        invoke_context
            .transaction_context
            .configure_top_level_instruction_for_tests(0, instruction_accounts, vec![0; 24])
            .unwrap();
        invoke_context.push().unwrap();
        let instruction_context = invoke_context
            .transaction_context
            .get_current_instruction_context()
            .unwrap();
        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            std::hint::black_box(
                serialize_parameters(&instruction_context, false, false, false).unwrap(),
            );
        }
        let fresh = start.elapsed().as_nanos() / u128::from(ITERATIONS);
        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            std::hint::black_box(
                serialize_parameters_pooled(&instruction_context, false, false, false).unwrap(),
            );
        }
        let pooled = start.elapsed().as_nanos() / u128::from(ITERATIONS);
        println!(
            "serialize_parameters (26 accounts, ~382 KB): fresh {fresh} ns, pooled {pooled} ns"
        );
    }
}
