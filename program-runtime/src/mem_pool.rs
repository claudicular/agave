use {
    crate::execution_budget::{
        MAX_CALL_DEPTH, MAX_HEAP_FRAME_BYTES, MAX_INSTRUCTION_STACK_DEPTH_SIMD_0268,
        MIN_HEAP_FRAME_BYTES,
    },
    solana_sbpf::{aligned_memory::AlignedMemory, ebpf::HOST_ALIGN, vm::CallFrame},
    std::{
        array,
        ops::{Deref, DerefMut},
    },
};

trait Reset {
    fn reset(&mut self);
}

struct Pool<T: Reset, const SIZE: usize> {
    items: [Option<T>; SIZE],
    next_empty: usize,
}

impl<T: Reset, const SIZE: usize> Pool<T, SIZE> {
    fn new(items: [T; SIZE]) -> Self {
        Self {
            items: items.map(|i| Some(i)),
            next_empty: SIZE,
        }
    }

    fn len(&self) -> usize {
        SIZE
    }

    fn get(&mut self) -> Option<T> {
        if self.next_empty == 0 {
            return None;
        }
        self.next_empty = self.next_empty.saturating_sub(1);
        self.items
            .get_mut(self.next_empty)
            .and_then(|item| item.take())
    }

    fn put(&mut self, value: T) -> bool {
        self.put_with(value, T::reset)
    }

    /// Like [`Self::put`], but resets `value` with `reset` instead of [`Reset::reset`].
    fn put_with(&mut self, mut value: T, reset: impl FnOnce(&mut T)) -> bool {
        self.items
            .get_mut(self.next_empty)
            .map(|item| {
                reset(&mut value);
                item.replace(value);
                self.next_empty = self.next_empty.saturating_add(1);
                true
            })
            .unwrap_or(false)
    }
}

impl Reset for AlignedMemory<{ HOST_ALIGN }> {
    fn reset(&mut self) {
        self.as_slice_mut().fill(0)
    }
}

pub struct CallFrameBuffer(Box<[CallFrame; MAX_CALL_DEPTH]>);

impl Default for CallFrameBuffer {
    fn default() -> Self {
        let mut mem = Box::<[CallFrame; MAX_CALL_DEPTH]>::new_uninit();
        let ptr = mem.as_mut_ptr().cast::<CallFrame>();
        for i in 0..MAX_CALL_DEPTH {
            unsafe { ptr.add(i).write(CallFrame::default()) }
        }
        Self(unsafe { mem.assume_init() })
    }
}

impl Reset for CallFrameBuffer {
    fn reset(&mut self) {
        self.fill(CallFrame::default())
    }
}

impl Deref for CallFrameBuffer {
    type Target = [CallFrame];

    fn deref(&self) -> &Self::Target {
        self.0.as_slice()
    }
}

impl DerefMut for CallFrameBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut_slice()
    }
}

pub struct VmMemoryPool {
    stack: Pool<AlignedMemory<{ HOST_ALIGN }>, MAX_INSTRUCTION_STACK_DEPTH_SIMD_0268>,
    heap: Pool<AlignedMemory<{ HOST_ALIGN }>, MAX_INSTRUCTION_STACK_DEPTH_SIMD_0268>,
    call_frame: Pool<CallFrameBuffer, MAX_INSTRUCTION_STACK_DEPTH_SIMD_0268>,
}

impl VmMemoryPool {
    pub fn new() -> Self {
        Self {
            stack: Pool::new(array::from_fn(|_| {
                #[allow(clippy::arithmetic_side_effects)]
                AlignedMemory::zero_filled(solana_sbpf::vm::get_stack_frame_size() * MAX_CALL_DEPTH)
            })),
            heap: Pool::new(array::from_fn(|_| {
                AlignedMemory::zero_filled(MAX_HEAP_FRAME_BYTES as usize)
            })),
            call_frame: Pool::new(array::from_fn(|_| CallFrameBuffer::default())),
        }
    }

    pub fn stack_len(&self) -> usize {
        self.stack.len()
    }

    pub fn heap_len(&self) -> usize {
        self.heap.len()
    }

    #[allow(clippy::arithmetic_side_effects)]
    pub fn get_stack(&mut self, size: usize) -> AlignedMemory<{ HOST_ALIGN }> {
        debug_assert!(size == solana_sbpf::vm::get_stack_frame_size() * MAX_CALL_DEPTH);
        self.stack
            .get()
            .unwrap_or_else(|| AlignedMemory::zero_filled(size))
    }

    pub fn put_stack(&mut self, stack: AlignedMemory<{ HOST_ALIGN }>) -> bool {
        self.stack.put(stack)
    }

    pub fn get_heap(&mut self, heap_size: u32) -> AlignedMemory<{ HOST_ALIGN }> {
        debug_assert!((MIN_HEAP_FRAME_BYTES..=MAX_HEAP_FRAME_BYTES).contains(&heap_size));
        self.heap
            .get()
            .unwrap_or_else(|| AlignedMemory::zero_filled(MAX_HEAP_FRAME_BYTES as usize))
    }

    pub fn put_heap(&mut self, heap: AlignedMemory<{ HOST_ALIGN }>) -> bool {
        let heap_size = heap.len();
        debug_assert!(
            heap_size >= MIN_HEAP_FRAME_BYTES as usize
                && heap_size <= MAX_HEAP_FRAME_BYTES as usize
        );
        self.heap.put(heap)
    }

    /// Returns a heap to the pool, zeroing only its first `mapped_len` bytes.
    ///
    /// This is equivalent to [`Self::put_heap`] provided that nothing wrote to the heap beyond
    /// `mapped_len`, which holds when `mapped_len` is the length of the heap slice that was
    /// mapped into the VM (`heap_size` from the compute budget, see `create_vm!`):
    ///
    /// - Every buffer in the pool is entirely zero. Buffers are created zero-filled, and each
    ///   return re-zeroes at least every byte that could have been written since it was handed
    ///   out, so the invariant is preserved.
    /// - The VM, all syscalls and the bump allocator reach the heap only through the
    ///   `MemoryMapping` heap region, whose host slice is `heap[..mapped_len]`. Translation
    ///   (`MemoryRegion::vm_to_host_buffer`) returns `None` for any access whose end exceeds the
    ///   region length, and the access-violation handler never grows the heap region (it has no
    ///   account payload). Bytes at or beyond `mapped_len` are therefore still zero.
    ///
    /// So after zeroing `[..mapped_len]` the buffer is identical to a fully reset one, whatever
    /// heap size the next user requests.
    pub fn put_heap_mapped_prefix(
        &mut self,
        heap: AlignedMemory<{ HOST_ALIGN }>,
        mapped_len: usize,
    ) -> bool {
        debug_assert!(
            heap.len() >= MIN_HEAP_FRAME_BYTES as usize
                && heap.len() <= MAX_HEAP_FRAME_BYTES as usize
        );
        debug_assert!(mapped_len <= heap.len());
        self.heap.put_with(heap, |heap| {
            let slice = heap.as_slice_mut();
            let mapped_len = mapped_len.min(slice.len());
            if let Some(mapped) = slice.get_mut(..mapped_len) {
                mapped.fill(0);
            }
        })
    }

    pub fn get_call_frames(&mut self) -> CallFrameBuffer {
        self.call_frame.get().unwrap_or_default()
    }

    pub fn put_call_frames(&mut self, call_frame: CallFrameBuffer) -> bool {
        self.call_frame.put(call_frame)
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod test {
    use super::*;

    #[derive(Debug, Eq, PartialEq)]
    struct Item(u8, u8);
    impl Reset for Item {
        fn reset(&mut self) {
            self.1 = 0;
        }
    }

    fn dirty_heap(pool: &mut VmMemoryPool, heap_size: u32, value: u8) {
        let mut heap = pool.get_heap(heap_size);
        heap.as_slice_mut()
            .get_mut(..heap_size as usize)
            .unwrap()
            .fill(value);
        assert!(pool.put_heap_mapped_prefix(heap, heap_size as usize));
    }

    #[test]
    fn test_put_heap_mapped_prefix_restores_all_zero_pool() {
        let mut pool = VmMemoryPool::new();
        // Dirty every pooled heap through its mapped prefix, with the largest, the default and
        // an odd heap size, then check that every pooled heap is entirely zero again, exactly
        // as after a full reset.
        for heap_size in [MAX_HEAP_FRAME_BYTES, MIN_HEAP_FRAME_BYTES, 33 * 1024] {
            let mut taken = Vec::new();
            for _ in 0..pool.heap_len() {
                let mut heap = pool.get_heap(heap_size);
                heap.as_slice_mut()
                    .get_mut(..heap_size as usize)
                    .unwrap()
                    .fill(0xa5);
                taken.push(heap);
            }
            for heap in taken {
                assert!(pool.put_heap_mapped_prefix(heap, heap_size as usize));
            }
            for _ in 0..pool.heap_len() {
                let heap = pool.get_heap(MAX_HEAP_FRAME_BYTES);
                assert_eq!(heap.len(), MAX_HEAP_FRAME_BYTES as usize);
                assert!(heap.as_slice().iter().all(|b| *b == 0));
                // Put back with a full reset to keep the pool populated.
                assert!(pool.put_heap(heap));
            }
        }
    }

    #[test]
    fn test_put_heap_mapped_prefix_matches_full_reset() {
        // A large heap dirtied by one invocation must come back fully zeroed even if the next
        // invocation (with a smaller heap_size) returns it with a smaller prefix.
        let mut pool = VmMemoryPool::new();
        dirty_heap(&mut pool, MAX_HEAP_FRAME_BYTES, 0xff);
        dirty_heap(&mut pool, MIN_HEAP_FRAME_BYTES, 0x11);
        let heap = pool.get_heap(MAX_HEAP_FRAME_BYTES);
        assert!(heap.as_slice().iter().all(|b| *b == 0));
        assert_eq!(
            heap,
            AlignedMemory::zero_filled(MAX_HEAP_FRAME_BYTES as usize)
        );
    }

    #[test]
    fn test_put_heap_mapped_prefix_full_pool() {
        let mut pool = VmMemoryPool::new();
        // The pool starts full, so returning an extra heap is rejected like `put_heap`.
        let extra = AlignedMemory::zero_filled(MAX_HEAP_FRAME_BYTES as usize);
        assert!(!pool.put_heap_mapped_prefix(extra, MIN_HEAP_FRAME_BYTES as usize));
    }

    /// Microbenchmark (run with `--release -- --ignored --nocapture`): cost of returning a heap
    /// to the pool with a full reset versus a mapped-prefix reset of the default 32 KiB.
    #[test]
    #[ignore]
    fn bench_heap_reset() {
        const ITERATIONS: u32 = 20_000;
        let mut pool = VmMemoryPool::new();
        for (label, prefix) in [
            ("full 256 KiB reset", None),
            (
                "mapped 32 KiB prefix reset",
                Some(MIN_HEAP_FRAME_BYTES as usize),
            ),
        ] {
            let start = std::time::Instant::now();
            for _ in 0..ITERATIONS {
                let mut heap = pool.get_heap(MIN_HEAP_FRAME_BYTES);
                // Touch the mapped prefix like a program would.
                heap.as_slice_mut()[0] = 1;
                match prefix {
                    None => assert!(pool.put_heap(std::hint::black_box(heap))),
                    Some(len) => {
                        assert!(pool.put_heap_mapped_prefix(std::hint::black_box(heap), len))
                    }
                }
            }
            let ns = start.elapsed().as_nanos() / u128::from(ITERATIONS);
            println!("{label}: {ns} ns per get+put");
        }
    }

    #[test]
    fn test_pool() {
        let mut pool = Pool::<Item, 2>::new([Item(0, 1), Item(1, 1)]);
        assert_eq!(pool.get(), Some(Item(1, 1)));
        assert_eq!(pool.get(), Some(Item(0, 1)));
        assert_eq!(pool.get(), None);
        pool.put(Item(1, 1));
        assert_eq!(pool.get(), Some(Item(1, 0)));
        pool.put(Item(2, 2));
        pool.put(Item(3, 3));
        assert!(!pool.put(Item(4, 4)));
        assert_eq!(pool.get(), Some(Item(3, 0)));
        assert_eq!(pool.get(), Some(Item(2, 0)));
        assert_eq!(pool.get(), None);
    }
}
