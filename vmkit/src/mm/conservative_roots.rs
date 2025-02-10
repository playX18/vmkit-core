use std::{
    collections::HashSet,
    hash::Hash,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use mmtk::{
    util::{Address, ObjectReference},
    vm::{slot::Slot, RootsWorkFactory},
};

use crate::{object_model::object::VMKitObject, options::OPTIONS, VirtualMachine};

pub struct ConservativeRoots {
    pub roots: HashSet<ObjectReference>,
    pub internal_pointer_limit: usize,
}

impl ConservativeRoots {
    /// Add a pointer to conservative root set.
    ///
    /// If pointer is not in the heap, this function does nothing.
    pub fn add_pointer(&mut self, pointer: Address) {
        let starting_address = mmtk::memory_manager::starting_heap_address();
        let ending_address = mmtk::memory_manager::last_heap_address();

        if pointer < starting_address || pointer > ending_address {
            return;
        }

        if self
            .roots
            .contains(unsafe { &ObjectReference::from_raw_address_unchecked(pointer) })
        {
            return;
        }

        let Some(start) = mmtk::memory_manager::find_object_from_internal_pointer(
            pointer,
            self.internal_pointer_limit,
        ) else {
            return;
        };

        self.roots.insert(start);
    }

    /// Add all pointers in the span to the roots.
    ///
    /// # SAFETY
    ///
    /// `start` and `end` must be valid addresses.
    pub unsafe fn add_span(&mut self, mut start: Address, mut end: Address) {
        if start > end {
            std::mem::swap(&mut start, &mut end);
        }
        let mut current = start;
        while current < end {
            let addr = current.load::<Address>();
            self.add_pointer(addr);
            current = current.add(size_of::<Address>());
        }
    }

    pub fn new() -> Self {
        Self {
            roots: HashSet::new(),
            internal_pointer_limit: OPTIONS.interior_pointer_max_bytes,
        }
    }

    pub fn add_to_factory<SL: Slot>(&mut self, factory: &mut impl RootsWorkFactory<SL>) {
        factory.create_process_tpinning_roots_work(std::mem::take(
            &mut self.roots.clone().into_iter().collect(),
        ));
    }
}

/// Pointer to some part of an heap object.
/// This type can only be used when `cooperative` feature is enabled and [`VM::CONSERVATIVE_TRACING`](VirtualMachine::CONSERVATIVE_TRACING) is `true`.
pub struct InternalPointer<T, VM: VirtualMachine> {
    address: Address,
    _marker: std::marker::PhantomData<(NonNull<T>, &'static VM)>,
}

unsafe impl<T: Send, VM: VirtualMachine> Send for InternalPointer<T, VM> {}
unsafe impl<T: Sync, VM: VirtualMachine> Sync for InternalPointer<T, VM> {}

impl<T, VM: VirtualMachine> InternalPointer<T, VM> {
    pub fn new(address: Address) -> Self {
        if !cfg!(feature = "cooperative") {
            unreachable!("Internal pointers are not supported in precise mode");
        }
        debug_assert!(
            mmtk::memory_manager::find_object_from_internal_pointer(
                address,
                OPTIONS.interior_pointer_max_bytes
            )
            .is_some(),
            "Internal pointer is not in the heap"
        );
        assert!(
            VM::CONSERVATIVE_TRACING,
            "Internal pointers are not supported without VM::CONSERVATIVE_TRACING set to true"
        );
        Self {
            address,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn as_address(&self) -> Address {
        self.address
    }

    /// Get original object from the pointer.
    ///
    /// # Panics
    ///
    /// Panics if the pointer is not in the heap.
    pub fn object(&self) -> VMKitObject {
        mmtk::memory_manager::find_object_from_internal_pointer(
            self.address,
            OPTIONS.interior_pointer_max_bytes,
        )
        .unwrap()
        .into()
    }

    /// Return offset from the object start.
    pub fn offset(&self) -> usize {
        self.address.as_usize() - self.object().as_address().as_usize()
    }
}

impl<T, VM: VirtualMachine> Deref for InternalPointer<T, VM> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.address.as_ref() }
    }
}

impl<T, VM: VirtualMachine> DerefMut for InternalPointer<T, VM> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.address.as_mut_ref() }
    }
}

/// Identical to [`InternalPointer`] but does not rely on VO-bits being enabled.
///
/// Stores original object and offset instead. This makes
/// this struct "fat" as its a 16-byte pointer on 64-bit systems compared to 8-bytes
/// of [`InternalPointer`]. This type can be used in precise mode.
pub struct FatInternalPointer<T, VM: VirtualMachine> {
    offset: usize,
    object: VMKitObject,
    marker: PhantomData<(NonNull<T>, &'static VM)>,
}

unsafe impl<T: Send, VM: VirtualMachine> Send for FatInternalPointer<T, VM> {}
unsafe impl<T: Sync, VM: VirtualMachine> Sync for FatInternalPointer<T, VM> {}

impl<T, VM: VirtualMachine> Deref for FatInternalPointer<T, VM> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { self.object.as_address().add(self.offset).as_ref() }
    }
}

impl<T, VM: VirtualMachine> DerefMut for FatInternalPointer<T, VM> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.object.as_address().add(self.offset).as_mut_ref() }
    }
}

impl<T, VM: VirtualMachine> Clone for FatInternalPointer<T, VM> {
    fn clone(&self) -> Self {
        Self {
            offset: self.offset,
            object: self.object,
            marker: PhantomData,
        }
    }
}

impl<T, VM: VirtualMachine> PartialEq for FatInternalPointer<T, VM> {
    fn eq(&self, other: &Self) -> bool {
        self.offset == other.offset && self.object == other.object
    }
}

impl<T, VM: VirtualMachine> Eq for FatInternalPointer<T, VM> {}

impl<T, VM: VirtualMachine> Hash for FatInternalPointer<T, VM> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.offset.hash(state);
        self.object.hashcode::<VM>().hash(state);
    }
}

impl<T, VM: VirtualMachine> FatInternalPointer<T, VM> {
    pub fn new(object: VMKitObject, offset: usize) -> Self {
        Self {
            offset,
            object,
            marker: PhantomData,
        }
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn object(&self) -> VMKitObject {
        self.object
    }
}
