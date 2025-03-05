use crate::{
    object_model::{
        header::{HeapObjectHeader, OBJECT_REF_OFFSET},
        object::VMKitObject,
        VMKitObjectModel,
    },
    threading::Thread,
    VirtualMachine,
};
use mmtk::{
    util::{
        alloc::{AllocatorSelector, BumpAllocator, ImmixAllocator},
        conversions::raw_align_up,
        metadata::side_metadata::GLOBAL_SIDE_METADATA_VM_BASE_ADDRESS,
        VMMutatorThread,
    },
    vm::{
        slot::{Slot, UnimplementedMemorySlice},
        VMBinding,
    },
    AllocationSemantics, BarrierSelector, MutatorContext,
};

use std::marker::PhantomData;

#[derive(Clone, Copy)]
pub struct MemoryManager<VM: VirtualMachine>(PhantomData<VM>);

impl<VM: VirtualMachine> Default for MemoryManager<VM> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<VM: VirtualMachine> VMBinding for MemoryManager<VM> {
    type VMMemorySlice = UnimplementedMemorySlice<VM::Slot>;
    type VMSlot = VM::Slot;

    type VMObjectModel = VMKitObjectModel<VM>;
    type VMActivePlan = active_plan::VMKitActivePlan<VM>;
    type VMCollection = collection::VMKitCollection<VM>;
    type VMScanning = scanning::VMKitScanning<VM>;
    type VMReferenceGlue = ref_glue::VMKitReferenceGlue<VM>;

    const MAX_ALIGNMENT: usize = VM::MAX_ALIGNMENT;
    const MIN_ALIGNMENT: usize = VM::MIN_ALIGNMENT;
}

pub mod active_plan;
pub mod align;
pub mod aslr;
pub mod collection;
pub mod conservative_roots;
pub mod ref_glue;
pub mod scanning;
pub mod stack_bounds;
pub mod tlab;
pub mod traits;
pub mod spec;

impl<VM: VirtualMachine> MemoryManager<VM> {
    pub extern "C-unwind" fn request_gc() -> bool {
        let tls = Thread::<VM>::current();

        mmtk::memory_manager::handle_user_collection_request(
            &VM::get().vmkit().mmtk,
            VMMutatorThread(tls.to_vm_thread()),
        )
    }

    /// General purpose allocation function. Always goes to `mmtk::memory_manager::alloc`
    /// and does not attempt to perform fast-path allocation. This is useful for debugging
    /// or when your JIT/AOT compiler is not yet able to produce fast-path allocation.
    #[inline(never)]
    pub extern "C-unwind" fn allocate_out_of_line(
        thread: &Thread<VM>,
        mut size: usize,
        alignment: usize,
        metadata: VM::Metadata,
        mut semantics: AllocationSemantics,
    ) -> VMKitObject {
        size += size_of::<HeapObjectHeader<VM>>();
        if semantics == AllocationSemantics::Default
            && size >= thread.max_non_los_default_alloc_bytes()
        {
            semantics = AllocationSemantics::Los;
        }
        size = raw_align_up(size, alignment);

        match semantics {
            AllocationSemantics::Los => Self::allocate_los(thread, size, alignment, metadata),
            AllocationSemantics::NonMoving => {
                Self::allocate_nonmoving(thread, size, alignment, metadata)
            }
            AllocationSemantics::Immortal => {
                Self::allocate_immortal(thread, size, alignment, metadata)
            }
            _ => unsafe {
                Self::flush_tlab(thread);
                let object_start =
                    mmtk::memory_manager::alloc(thread.mutator(), size, alignment, 0, semantics);

                object_start.store(HeapObjectHeader::<VM>::new(metadata));
                let object = VMKitObject::from_address(object_start + OBJECT_REF_OFFSET);
                Self::set_vo_bit(object);
                Self::refill_tlab(thread);
                object
            },
        }
    }

    /// Allocate object with `size`, `alignment`, and `metadata` with specified `semantics`.
    ///
    /// This function is a fast-path for allocation. If you allocate with `Default` semantics,
    /// this function will first try to allocate through bump-pointer in TLAB. If other
    /// semantic is specified or TLAB is empty (or unavailable) we invoke MMTK API directly.
    #[inline]
    pub extern "C-unwind" fn allocate(
        thread: &Thread<VM>,
        mut size: usize,
        alignment: usize,
        metadata: VM::Metadata,
        mut semantics: AllocationSemantics,
    ) -> VMKitObject {
        let orig_size = size;
        let orig_semantics = semantics;
        size += size_of::<HeapObjectHeader<VM>>();
        if semantics == AllocationSemantics::Default
            && size >= thread.max_non_los_default_alloc_bytes()
        {
            semantics = AllocationSemantics::Los;
        }

        size = raw_align_up(size, alignment);

        // all allocator functions other than this actually invoke `flush_tlab` due to the fact
        // that GC can happen inside them.
        match semantics {
            AllocationSemantics::Default => match thread.alloc_fastpath() {
                AllocFastPath::TLAB => unsafe {
                    let tlab = thread.tlab.get().as_mut().unwrap();
                    let object_start =
                        tlab.allocate::<VM>(size, alignment, OBJECT_REF_OFFSET as usize);

                    if !object_start.is_zero() {
                        object_start.store(HeapObjectHeader::<VM>::new(metadata));
                        let object = VMKitObject::from_address(object_start + OBJECT_REF_OFFSET);
                        Self::set_vo_bit(object);
                        debug_assert!(mmtk::memory_manager::is_mapped_address(object.as_address()));
                        return object;
                    }

                    return Self::allocate_slow(thread, size, alignment, metadata, semantics);
                },

                _ => (),
            },

            _ => (),
        }

        Self::allocate_out_of_line(thread, orig_size, alignment, metadata, orig_semantics)
    }

    #[inline(never)]
    extern "C-unwind" fn allocate_los(
        thread: &Thread<VM>,
        size: usize,
        alignment: usize,
        metadata: VM::Metadata,
    ) -> VMKitObject {
        unsafe {
            Self::flush_tlab(thread);
            let object_start = mmtk::memory_manager::alloc(
                thread.mutator(),
                size,
                alignment,
                OBJECT_REF_OFFSET as usize,
                AllocationSemantics::Los,
            );

            let object = VMKitObject::from_address(object_start + OBJECT_REF_OFFSET);
            object_start.store(HeapObjectHeader::<VM>::new(metadata));

            //Self::set_vo_bit(object);
            Self::refill_tlab(thread);
            mmtk::memory_manager::post_alloc(
                thread.mutator(),
                object.as_object_unchecked(),
                size,
                AllocationSemantics::Los,
            );
            object
        }
    }

    #[inline(never)]
    extern "C-unwind" fn allocate_nonmoving(
        thread: &Thread<VM>,
        size: usize,
        alignment: usize,
        metadata: VM::Metadata,
    ) -> VMKitObject {
        debug_assert!(thread.id() == Thread::<VM>::current().id());

        unsafe {
            Self::flush_tlab(thread);
            let object_start = mmtk::memory_manager::alloc(
                thread.mutator(),
                size,
                alignment,
                OBJECT_REF_OFFSET as usize,
                AllocationSemantics::NonMoving,
            );

            let object = VMKitObject::from_address(object_start + OBJECT_REF_OFFSET);
            object_start.store(HeapObjectHeader::<VM>::new(metadata));
            Self::set_vo_bit(object);
            Self::refill_tlab(thread);
            object
        }
    }

    #[inline(never)]
    extern "C-unwind" fn allocate_immortal(
        thread: &Thread<VM>,
        size: usize,
        alignment: usize,
        metadata: VM::Metadata,
    ) -> VMKitObject {
        debug_assert!(thread.id() == Thread::<VM>::current().id());

        unsafe {
            Self::flush_tlab(thread);
            let object_start = mmtk::memory_manager::alloc(
                thread.mutator(),
                size,
                alignment,
                OBJECT_REF_OFFSET as usize,
                AllocationSemantics::Immortal,
            );

            let object = VMKitObject::from_address(object_start + OBJECT_REF_OFFSET);
            object_start.store(HeapObjectHeader::<VM>::new(metadata));
            Self::set_vo_bit(object);
            Self::refill_tlab(thread);
            object
        }
    }

    /// Allocate object in a slowpath. This will reset TLAB and invoke MMTK directly to allocate. This
    /// function potentially triggers GC or allocates more heap memory if necessary.
    #[inline(never)]
    #[cold]
    pub extern "C-unwind" fn allocate_slow(
        thread: &Thread<VM>,
        size: usize,
        alignment: usize,
        metadata: VM::Metadata,
        semantics: AllocationSemantics,
    ) -> VMKitObject {
        unsafe {
            Self::flush_tlab(thread);
            let object_start = mmtk::memory_manager::alloc_slow(
                thread.mutator(),
                size,
                alignment,
                OBJECT_REF_OFFSET as usize,
                semantics,
            );

            object_start.store(HeapObjectHeader::<VM>::new(metadata));
            let object = VMKitObject::from_address(object_start + OBJECT_REF_OFFSET);
            Self::set_vo_bit(object);
            Self::refill_tlab(thread);
            debug_assert!(mmtk::memory_manager::is_mapped_address(object.as_address()));
            object
        }
    }

    #[inline(always)]
    pub extern "C-unwind" fn set_vo_bit(object: VMKitObject) {
        #[cfg(feature = "cooperative")]
        unsafe {
            let meta = mmtk::util::metadata::vo_bit::VO_BIT_SIDE_METADATA_ADDR
                + (object.as_address() >> 6);
            let byte = meta.load::<u8>();
            let mask = 1 << ((object.as_address() >> 3) & 7);
            let new_byte = byte | mask;
            meta.store::<u8>(new_byte);
        }
    }

    /// Flushes the TLAB by storing the TLAB to underlying allocator.
    ///
    /// Leaves TLAB cursor set to zero. Invoke `refill_tlab` to rebind the TLAB.
    ///
    ///
    /// # Safety
    ///
    /// Must be invoked in the context of current thread and before `alloc()` calls into MMTK
    pub unsafe fn flush_tlab(thread: &Thread<VM>) {
        let tlab = thread.tlab.get().as_mut().unwrap();
        let (cursor, limit) = tlab.take();
        if cursor.is_zero() {
            assert!(limit.is_zero());
            return;
        }
        let selector = mmtk::memory_manager::get_allocator_mapping(
            &VM::get().vmkit().mmtk,
            AllocationSemantics::Default,
        );
        match selector {
            AllocatorSelector::Immix(_) => {
                let allocator = thread
                    .mutator_unchecked()
                    .allocator_impl_mut::<ImmixAllocator<Self>>(selector);
                allocator.bump_pointer.reset(cursor, limit);
            }

            AllocatorSelector::BumpPointer(_) => {
                let allocator = thread
                    .mutator_unchecked()
                    .allocator_impl_mut::<BumpAllocator<Self>>(selector);
                allocator.bump_pointer.reset(cursor, limit);
            }

            _ => {
                assert!(
                    cursor.is_zero() && limit.is_zero(),
                    "currently selected plan has no TLAB"
                );
            }
        }
    }

    pub unsafe fn refill_tlab(thread: &Thread<VM>) {
        let tlab = thread.tlab.get().as_mut().unwrap();

        let selector = mmtk::memory_manager::get_allocator_mapping(
            &VM::get().vmkit().mmtk,
            AllocationSemantics::Default,
        );

        match selector {
            AllocatorSelector::Immix(_) => {
                let allocator = thread
                    .mutator()
                    .allocator_impl::<ImmixAllocator<Self>>(selector);
                let (cursor, limit) = (allocator.bump_pointer.cursor, allocator.bump_pointer.limit);
                tlab.rebind(cursor, limit);
            }

            AllocatorSelector::BumpPointer(_) => {
                let allocator = thread
                    .mutator()
                    .allocator_impl::<BumpAllocator<Self>>(selector);
                let (cursor, limit) = (allocator.bump_pointer.cursor, allocator.bump_pointer.limit);
                tlab.rebind(cursor, limit);
            }

            _ => {}
        }
    }

    /// The write barrier by MMTk. This is a *post* write barrier, which we expect a binding to call
    /// *after* it modifies an object. For performance reasons, a VM should implement the write barrier
    /// fast-path on their side rather than just calling this function.
    ///
    /// For a correct barrier implementation, a VM binding needs to choose one of the following options:
    /// * Use subsuming barrier `object_reference_write`
    /// * Use both `object_reference_write_pre` and `object_reference_write_post`, or both, if the binding has difficulty delegating the store to mmtk-core with the subsuming barrier.
    /// * Implement fast-path on the VM side, and call the generic api `object_reference_write_slow` as barrier slow-path call.
    /// * Implement fast-path on the VM side, and do a specialized slow-path call.
    ///
    /// Arguments:
    /// * `thread`: a current thread.
    /// * `src`: The modified source object.
    /// * `slot`: The location of the field to be modified.
    /// * `target`: The target for the write operation.  `NULL` if the slot no longer hold an object
    ///   reference after the write operation.  This may happen when writing a `null` reference, a small
    ///   integers, or a special value such as`true`, `false`, `undefined`, etc., into the slot.
    pub fn object_reference_write_post(
        thread: &Thread<VM>,
        src: VMKitObject,
        slot: VM::Slot,
        target: VMKitObject,
    ) {
        match thread.barrier() {
            BarrierSelector::ObjectBarrier => unsafe {
                let addr = src.as_address();
                let meta_addr = GLOBAL_SIDE_METADATA_VM_BASE_ADDRESS + (addr >> 6);
                let shift = (addr >> 3) & 0b111;
                let byte_val = meta_addr.load::<u8>();
                if (byte_val >> shift) & 1 == 1 {
                    thread.mutator().barrier().object_reference_write_slow(
                        src.as_object_unchecked(),
                        slot,
                        if target.is_null() {
                            None
                        } else {
                            Some(target.as_object_unchecked())
                        },
                    );
                }
            },

            BarrierSelector::NoBarrier => {}
        }
    }
    /// The write barrier by MMTk. This is a *pre* write barrier, which we expect a binding to call
    /// *before* it modifies an object. For performance reasons, a VM should implement the write barrier
    /// fast-path on their side rather than just calling this function.
    ///
    /// For a correct barrier implementation, a VM binding needs to choose one of the following options:
    /// * Use subsuming barrier `object_reference_write`
    /// * Use both `object_reference_write_pre` and `object_reference_write_post`, or both, if the binding has difficulty delegating the store to mmtk-core with the subsuming barrier.
    /// * Implement fast-path on the VM side, and call the generic api `object_reference_write_slow` as barrier slow-path call.
    /// * Implement fast-path on the VM side, and do a specialized slow-path call.
    ///
    /// Arguments:
    /// * `thread`: a current thread.
    /// * `src`: The modified source object.
    /// * `slot`: The location of the field to be modified.
    /// * `target`: The target for the write operation.  `NULL` if the slot did not hold an object
    ///   reference before the write operation.  For example, the slot may be holding a `null`
    ///   reference, a small integer, or special values such as `true`, `false`, `undefined`, etc.
    pub fn object_reference_write_pre(
        thread: &Thread<VM>,
        src: VMKitObject,
        slot: VM::Slot,
        target: VMKitObject,
    ) {
        let _ = thread;
        let _ = src;
        let _ = slot;
        let _ = target;
    }

    pub fn object_reference_write(
        thread: &Thread<VM>,
        src: VMKitObject,
        slot: VM::Slot,
        target: VMKitObject,
    ) {
        assert!(target.is_not_null());
        Self::object_reference_write_pre(thread, src, slot, target);
        unsafe {
            slot.store(target.as_object_unchecked());
        }
        Self::object_reference_write_post(thread, src, slot, target);
    }

    pub fn disable_gc() {
        VM::get()
            .vmkit()
            .gc_disabled_depth
            .fetch_add(1, atomic::Ordering::SeqCst);
    }

    pub fn enable_gc() {
        VM::get()
            .vmkit()
            .gc_disabled_depth
            .fetch_sub(1, atomic::Ordering::SeqCst);
    }

    pub fn is_gc_enabled() -> bool {
        VM::get()
            .vmkit()
            .gc_disabled_depth
            .load(atomic::Ordering::SeqCst)
            == 0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AllocFastPath {
    TLAB,
    FreeList,
    None,
}
