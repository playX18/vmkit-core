use crate::mm::traits::SlotExtra;
use crate::prelude::Trace;
use crate::threading::Thread;
use crate::{mm::MemoryManager, VirtualMachine};
use atomic::Atomic;
use core::ops::Range;
use mmtk::util::{
    constants::LOG_BYTES_IN_ADDRESS, conversions::raw_align_up, Address, ObjectReference,
};
use mmtk::vm::slot::{MemorySlice, SimpleSlot, Slot};
use std::fmt;
use std::hash::Hash;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ptr::NonNull;

use super::{
    compression::CompressedOps,
    header::{
        HashState, HeapObjectHeader, HASHCODE_OFFSET, OBJECT_HEADER_OFFSET, OBJECT_REF_OFFSET,
    },
    metadata::Metadata,
};

/// Is address based hash enabled? If true
/// then object header uses 2 bits to indicate hash state and if GC moves
/// the object, object hash is stored in the object itself.
///
/// When disabled, `hashcode()` instead calls into VM to get the hashcode.
pub const ADDRESS_BASED_HASHING: bool = cfg!(feature = "address_based_hashing");

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VMKitObject(Address);

impl From<ObjectReference> for VMKitObject {
    fn from(value: ObjectReference) -> Self {
        Self::from_address(value.to_raw_address())
    }
}

impl TryInto<ObjectReference> for VMKitObject {
    type Error = ();

    fn try_into(self) -> Result<ObjectReference, Self::Error> {
        ObjectReference::from_raw_address(self.0).ok_or(())
    }
}
impl Into<Option<ObjectReference>> for VMKitObject {
    fn into(self) -> Option<ObjectReference> {
        self.try_into().ok()
    }
}

impl VMKitObject {
    /// The null `VMKitObject`.
    pub const NULL: Self = Self(Address::ZERO);

    /// Creates a new `VMKitObject` from a given address.
    ///
    /// # Arguments
    ///
    /// * `address` - The address of the object.
    ///
    /// # Returns
    ///
    /// * `VMKitObject` - A new `VMKitObject` instance.
    #[inline(always)]
    pub fn from_address(address: Address) -> Self {
        Self(address)
    }

    /// Returns the address of the `VMKitObject`.
    ///
    /// # Returns
    ///
    /// * `Address` - The address of the object.
    #[inline(always)]
    pub fn as_address(self) -> Address {
        self.0
    }

    pub unsafe fn as_object_unchecked(self) -> ObjectReference {
        unsafe { ObjectReference::from_raw_address_unchecked(self.as_address()) }
    }

    /// Creates a new `VMKitObject` from an optional object reference.
    ///
    /// # Arguments
    ///
    /// * `objref` - An optional object reference.
    ///
    /// # Returns
    ///
    /// * `VMKitObject` - A new `VMKitObject` instance.
    #[inline(always)]
    pub fn from_objref_nullable(objref: Option<ObjectReference>) -> Self {
        match objref {
            Some(objref) => Self::from_address(objref.to_raw_address()),
            None => Self::NULL,
        }
    }

    /// Checks if the `VMKitObject` is null.
    ///
    /// # Returns
    ///
    /// * `bool` - `true` if the object is null, `false` otherwise.
    #[inline(always)]
    pub fn is_null(self) -> bool {
        self == Self::NULL
    }

    /// Checks if the `VMKitObject` is not null.
    ///
    /// # Returns
    ///
    /// * `bool` - `true` if the object is not null, `false` otherwise.
    #[inline(always)]
    pub fn is_not_null(self) -> bool {
        self != Self::NULL
    }

    /// Returns a reference to the `HeapObjectHeader` of the `VMKitObject`.
    ///
    /// # Returns
    ///
    /// * `&HeapObjectHeader<VM>` - A reference to the header.
    #[inline(always)]
    pub fn header<'a, VM: VirtualMachine>(self) -> &'a HeapObjectHeader<VM> {
        assert!(!self.is_null());
        unsafe { self.0.offset(OBJECT_HEADER_OFFSET).as_ref() }
    }

    /// Returns the alignment of the `VMKitObject`.
    ///
    /// # Returns
    ///
    /// * `usize` - The alignment of the object.
    pub fn alignment<VM: VirtualMachine>(self) -> usize {
        let alignment = self.header::<VM>().metadata().gc_metadata().alignment;
        if alignment == 0 {
            return self
                .header::<VM>()
                .metadata()
                .gc_metadata()
                .compute_alignment
                .map(|f| f(self))
                .unwrap_or(VM::MAX_ALIGNMENT);
        }
        alignment
    }

    /// Returns the number of bytes used by the `VMKitObject`.
    ///
    /// # Returns
    ///
    /// * `usize` - The number of bytes used.
    #[inline(always)]
    pub fn bytes_used<VM: VirtualMachine>(self) -> usize {
        let metadata = self.header::<VM>().metadata();
        let metadata = metadata.gc_metadata();
        let overhead = self.hashcode_overhead::<VM, false>();

        let res = if metadata.instance_size != 0 {
            raw_align_up(
                metadata.instance_size + size_of::<HeapObjectHeader<VM>>(),
                align_of::<usize>(),
            ) + overhead
        } else {
            let Some(compute_size) = metadata.compute_size else {
                panic!("compute_size is not set for object at {}", self.0);
            };

            raw_align_up(
                compute_size(self) + size_of::<HeapObjectHeader<VM>>(),
                align_of::<usize>(),
            ) + overhead
        };

        res
    }

    /// Returns the number of bytes required when the `VMKitObject` is copied.
    ///
    /// # Returns
    ///
    /// * `usize` - The number of bytes required.
    #[inline(always)]
    pub fn bytes_required_when_copied<VM: VirtualMachine>(self) -> usize {
        let metadata = self.header::<VM>().metadata();
        let metadata = metadata.gc_metadata();
        let overhead = self.hashcode_overhead::<VM, true>();

        if metadata.instance_size != 0 {
            raw_align_up(
                metadata.instance_size + size_of::<HeapObjectHeader<VM>>(),
                align_of::<usize>(),
            ) + overhead
        } else {
            let Some(compute_size) = metadata.compute_size else {
                panic!("compute_size is not set for object at {}", self.0);
            };

            raw_align_up(
                compute_size(self) + size_of::<HeapObjectHeader<VM>>(),
                align_of::<usize>(),
            ) + overhead
        }
    }

    /// Returns the overhead for the hashcode of the `VMKitObject`.
    ///
    /// # Arguments
    ///
    /// * `WHEN_COPIED` - A constant indicating whether the object is being copied.
    ///
    /// # Returns
    ///
    /// * `usize` - The hashcode overhead.
    #[inline(always)]
    pub fn hashcode_overhead<VM: VirtualMachine, const WHEN_COPIED: bool>(&self) -> usize {
        if !ADDRESS_BASED_HASHING {
            return 0;
        }
        let hash_state = self.header::<VM>().hash_state();

        let has_hashcode = if WHEN_COPIED {
            hash_state != HashState::Unhashed
        } else {
            hash_state == HashState::HashedAndMoved
        };

        if has_hashcode {
            size_of::<usize>()
        } else {
            0
        }
    }

    /// Returns the real starting address of the `VMKitObject`.
    ///
    /// # Returns
    ///
    /// * `Address` - The starting address of the object.
    #[inline(always)]
    pub fn object_start<VM: VirtualMachine>(&self) -> Address {
        let res = self
            .0
            .offset(-(OBJECT_REF_OFFSET as isize + self.hashcode_overhead::<VM, false>() as isize));

        res
    }

    /// Returns the offset for alignment of the `VMKitObject`.
    ///
    /// # Returns
    ///
    /// * `usize` - The offset for alignment.
    #[inline(always)]
    pub fn get_offset_for_alignment<VM: VirtualMachine>(&self) -> usize {
        size_of::<HeapObjectHeader<VM>>() + self.hashcode_overhead::<VM, true>()
    }

    /// Returns the current size of the `VMKitObject`.
    ///
    /// # Returns
    ///
    /// * `usize` - The current size.
    #[inline(always)]
    pub fn get_current_size<VM: VirtualMachine>(&self) -> usize {
        self.bytes_used::<VM>()
    }

    /// Returns the size of the `VMKitObject` when it is copied.
    ///
    /// # Returns
    ///
    /// * `usize` - The size when copied.
    #[inline(always)]
    pub fn get_size_when_copied<VM: VirtualMachine>(&self) -> usize {
        self.bytes_required_when_copied::<VM>()
    }

    pub fn hashcode<VM: VirtualMachine>(self) -> usize {
        if !ADDRESS_BASED_HASHING {
            return VM::compute_hashcode(self);
        }
        let header = self.header::<VM>();
        match header.hash_state() {
            HashState::HashedAndMoved => {
                return unsafe { self.as_address().offset(HASHCODE_OFFSET).load() }
            }
            _ => (),
        }
        let hashcode = self.as_address().as_usize() >> LOG_BYTES_IN_ADDRESS;
        header.set_hash_state(HashState::Hashed);
        hashcode
    }

    pub fn get_field_primitive<T, VM: VirtualMachine, const VOLATILE: bool>(
        &self,
        offset: isize,
    ) -> T
    where
        T: Copy + bytemuck::NoUninit + bytemuck::Pod,
    {
        unsafe {
            debug_assert!(
                offset < self.bytes_used::<VM>() as isize,
                "attempt to access field out of bounds"
            );
            let ordering = if !VOLATILE {
                return self.as_address().offset(offset).load::<T>();
            } else {
                atomic::Ordering::SeqCst
            };
            self.as_address()
                .offset(offset)
                .as_ref::<Atomic<T>>()
                .load(ordering)
        }
    }

    pub fn set_field_primitive<T, VM: VirtualMachine, const VOLATILE: bool>(
        &self,
        offset: isize,
        value: T,
    ) where
        T: Copy + bytemuck::NoUninit,
    {
        debug_assert!(
            offset < self.bytes_used::<VM>() as isize,
            "attempt to access field out of bounds"
        );
        unsafe {
            let ordering = if !VOLATILE {
                self.as_address().offset(offset).store(value);
                return;
            } else {
                atomic::Ordering::SeqCst
            };
            self.as_address()
                .offset(offset)
                .as_ref::<Atomic<T>>()
                .store(value, ordering);
        }
    }
    pub fn get_field_bool<VM: VirtualMachine>(&self, offset: isize) -> bool {
        self.get_field_primitive::<u8, VM, false>(offset) != 0
    }

    pub fn set_field_bool<VM: VirtualMachine>(&self, offset: isize, value: bool) {
        self.set_field_primitive::<u8, VM, false>(offset, if value { 1 } else { 0 });
    }

    pub fn get_field_u8<VM: VirtualMachine>(&self, offset: isize) -> u8 {
        self.get_field_primitive::<u8, VM, false>(offset)
    }

    pub fn set_field_u8<VM: VirtualMachine>(&self, offset: isize, value: u8) {
        self.set_field_primitive::<u8, VM, false>(offset, value);
    }

    pub fn get_field_u16<VM: VirtualMachine>(&self, offset: isize) -> u16 {
        self.get_field_primitive::<u16, VM, false>(offset)
    }

    pub fn set_field_u16<VM: VirtualMachine>(&self, offset: isize, value: u16) {
        self.set_field_primitive::<u16, VM, false>(offset, value);
    }
    
    pub fn get_field_u32<VM: VirtualMachine>(&self, offset: isize) -> u32 {
        self.get_field_primitive::<u32, VM, false>(offset)
    }

    pub fn set_field_u32<VM: VirtualMachine>(&self, offset: isize, value: u32) {
        self.set_field_primitive::<u32, VM, false>(offset, value);
    }

    pub fn get_field_u64<VM: VirtualMachine>(&self, offset: isize) -> u64 {
        self.get_field_primitive::<u64, VM, false>(offset)
    }

    pub fn set_field_u64<VM: VirtualMachine>(&self, offset: isize, value: u64) {
        self.set_field_primitive::<u64, VM, false>(offset, value);
    }

    pub fn get_field_i8<VM: VirtualMachine>(&self, offset: isize) -> i8 {
        self.get_field_primitive::<i8, VM, false>(offset)
    }

    pub fn set_field_i8<VM: VirtualMachine>(&self, offset: isize, value: i8) {
        self.set_field_primitive::<i8, VM, false>(offset, value);
    }

    pub fn get_field_i16<VM: VirtualMachine>(&self, offset: isize) -> i16 {
        self.get_field_primitive::<i16, VM, false>(offset)
    }

    pub fn set_field_i16<VM: VirtualMachine>(&self, offset: isize, value: i16) {
        self.set_field_primitive::<i16, VM, false>(offset, value);
    }

    pub fn get_field_i32<VM: VirtualMachine>(&self, offset: isize) -> i32 {
        self.get_field_primitive::<i32, VM, false>(offset)
    }

    pub fn set_field_i32<VM: VirtualMachine>(&self, offset: isize, value: i32) {
        self.set_field_primitive::<i32, VM, false>(offset, value);
    }

    pub fn get_field_i64<VM: VirtualMachine>(&self, offset: isize) -> i64 {
        self.get_field_primitive::<i64, VM, false>(offset)
    }

    pub fn set_field_i64<VM: VirtualMachine>(&self, offset: isize, value: i64) {
        self.set_field_primitive::<i64, VM, false>(offset, value);
    }

    pub fn get_field_f32<VM: VirtualMachine>(&self, offset: isize) -> f32 {
        self.get_field_primitive::<f32, VM, false>(offset)
    }

    pub fn set_field_f32<VM: VirtualMachine>(&self, offset: isize, value: f32) {
        self.set_field_primitive::<f32, VM, false>(offset, value);
    }

    pub fn get_field_f64<VM: VirtualMachine>(&self, offset: isize) -> f64 {
        self.get_field_primitive::<f64, VM, false>(offset)
    }

    pub fn set_field_f64<VM: VirtualMachine>(&self, offset: isize, value: f64) {
        self.set_field_primitive::<f64, VM, false>(offset, value);
    }

    pub fn get_field_isize<VM: VirtualMachine>(&self, offset: isize) -> isize {
        self.get_field_primitive::<isize, VM, false>(offset)
    }

    pub fn set_field_isize<VM: VirtualMachine>(&self, offset: isize, value: isize) {
        self.set_field_primitive::<isize, VM, false>(offset, value);
    }

    pub fn get_field_usize<VM: VirtualMachine>(&self, offset: isize) -> usize {
        self.get_field_primitive::<usize, VM, false>(offset)
    }

    pub fn set_field_usize<VM: VirtualMachine>(&self, offset: isize, value: usize) {
        self.set_field_primitive::<usize, VM, false>(offset, value);
    }

    pub unsafe fn set_field_object_no_write_barrier<VM: VirtualMachine, const VOLATILE: bool>(
        &self,
        offset: isize,
        value: VMKitObject,
    ) {
        self.set_field_primitive::<usize, VM, VOLATILE>(offset, value.as_address().as_usize());
    }

    pub fn slot_at<VM: VirtualMachine>(&self, offset: isize) -> VM::Slot {
        VM::Slot::from_address(self.as_address() + offset as usize)
    }

    pub fn set_field_object<VM: VirtualMachine, const VOLATILE: bool>(
        self,
        offset: isize,
        value: VMKitObject,
    ) {
        let tls = Thread::<VM>::current();
        MemoryManager::object_reference_write_pre(tls, self, self.slot_at::<VM>(offset), value);
        unsafe {
            self.set_field_object_no_write_barrier::<VM, VOLATILE>(offset, value);
        }
        MemoryManager::object_reference_write_post(tls, self, self.slot_at::<VM>(offset), value);
    }

    /// Same as [`set_field_object`](Self::set_field_object) but sets
    /// tagged value instead of object address. Accepts object address as a last
    /// parameter to perform write barrier.
    pub fn set_field_object_tagged<VM: VirtualMachine, const VOLATILE: bool>(
        self,
        offset: isize,
        value_to_set: usize,
        object: VMKitObject,
    ) {
        let tls = Thread::<VM>::current();
        MemoryManager::object_reference_write_pre(tls, self, self.slot_at::<VM>(offset), object);

        self.set_field_primitive::<usize, VM, VOLATILE>(offset, value_to_set);

        MemoryManager::object_reference_write_post(tls, self, self.slot_at::<VM>(offset), object);
    }

    pub fn get_field_object<VM: VirtualMachine, const VOLATILE: bool>(
        self,
        offset: isize,
    ) -> VMKitObject {
        unsafe {
            let addr = Address::from_usize(self.get_field_primitive::<usize, VM, VOLATILE>(offset));
            VMKitObject::from_address(addr)
        }
    }

    pub fn get_field_narrow<VM: VirtualMachine, const VOLATILE: bool>(
        self,
        offset: isize,
    ) -> VMKitNarrow {
        unsafe { VMKitNarrow::from_raw(self.get_field_primitive::<u32, VM, VOLATILE>(offset)) }
    }

    pub fn set_field_narrow<VM: VirtualMachine, const VOLATILE: bool>(
        self,
        offset: isize,
        value: VMKitNarrow,
    ) {
        let tls = Thread::<VM>::current();
        MemoryManager::object_reference_write_pre(
            tls,
            self,
            self.slot_at::<VM>(offset),
            value.to_object(),
        );
        self.set_field_primitive::<u32, VM, VOLATILE>(offset, value.raw());
        MemoryManager::object_reference_write_post(
            tls,
            self,
            self.slot_at::<VM>(offset),
            value.to_object(),
        );
    }

    pub unsafe fn set_field_narrow_no_write_barrier<VM: VirtualMachine, const VOLATILE: bool>(
        self,
        offset: isize,
        value: VMKitNarrow,
    ) {
        self.set_field_primitive::<u32, VM, VOLATILE>(offset, value.raw());
    }

    pub fn set_field_narrow_tagged<VM: VirtualMachine, const VOLATILE: bool>(
        self,
        offset: isize,
        value_to_set: u32,
        object: VMKitNarrow,
    ) {
        let tls = Thread::<VM>::current();
        MemoryManager::object_reference_write_pre(
            tls,
            self,
            self.slot_at::<VM>(offset),
            object.to_object(),
        );
        self.set_field_primitive::<u32, VM, VOLATILE>(offset, value_to_set);
        MemoryManager::object_reference_write_post(
            tls,
            self,
            self.slot_at::<VM>(offset),
            object.to_object(),
        );
    }
}

/// Used as a parameter of `move_object` to specify where to move an object to.
pub enum MoveTarget {
    /// Move an object to the address returned from `alloc_copy`.
    ToAddress(Address),
    /// Move an object to an `VMKitObject` pointing to an object previously computed from
    /// `get_reference_when_copied_to`.
    ToObject(VMKitObject),
}

impl fmt::Display for MoveTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MoveTarget::ToAddress(addr) => write!(f, "ToAddress({})", addr),
            MoveTarget::ToObject(obj) => write!(f, "ToObject({})", obj.as_address()),
        }
    }
}

/// Narrow pointer to an object. This is used when pointer compression
/// is enabled.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VMKitNarrow(u32);

impl VMKitNarrow {
    pub const NULL: Self = Self(0);

    /// Return the raw value of the narrow pointer.
    pub const fn raw(self) -> u32 {
        self.0
    }

    pub fn from_object(object: VMKitObject) -> Self {
        Self::encode(object)
    }

    pub fn encode(object: VMKitObject) -> Self {
        CompressedOps::encode(object)
    }

    pub fn decode(self) -> VMKitObject {
        CompressedOps::decode(self)
    }

    /// Create a new `VMKitNarrow` from a raw value.
    ///
    /// # Safety
    ///
    /// This function is unsafe because it assumes that the raw value is a valid narrow pointer.
    pub unsafe fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn is_null(self) -> bool {
        self.raw() == 0
    }

    pub fn to_address(self) -> Address {
        CompressedOps::decode(self).as_address()
    }

    pub fn to_object(self) -> VMKitObject {
        CompressedOps::decode(self)
    }

    pub fn header<'a, VM: VirtualMachine>(&'a self) -> &'a HeapObjectHeader<VM> {
        self.to_object().header::<VM>()
    }

    pub fn hashcode<VM: VirtualMachine>(&self) -> usize {
        self.to_object().hashcode::<VM>()
    }

    pub fn object_start<VM: VirtualMachine>(&self) -> Address {
        self.to_object().object_start::<VM>()
    }
}

pub struct SimpleMemorySlice<SL: Slot = SimpleSlot> {
    range: Range<SL>,
}

impl<SL: SlotExtra> SimpleMemorySlice<SL> {
    pub fn from(value: Range<SL>) -> Self {
        Self { range: value }
    }
}

impl<SL: SlotExtra> fmt::Debug for SimpleMemorySlice<SL> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SimpleMemorySlice({:?})", self.range)
    }
}

impl<SL: SlotExtra> Hash for SimpleMemorySlice<SL> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.range.start.as_address().hash(state);
        self.range.end.as_address().hash(state);
    }
}

impl<SL: SlotExtra> PartialEq for SimpleMemorySlice<SL> {
    fn eq(&self, other: &Self) -> bool {
        self.range == other.range
    }
}

impl<SL: SlotExtra> Eq for SimpleMemorySlice<SL> {}

impl<SL: SlotExtra> Clone for SimpleMemorySlice<SL> {
    fn clone(&self) -> Self {
        Self {
            range: self.range.clone(),
        }
    }
}

pub struct SimpleMemorySliceRangeIterator<SL: SlotExtra = SimpleSlot> {
    cursor: Address,
    end: Address,
    marker: PhantomData<SL>,
}

impl<SL: SlotExtra> Iterator for SimpleMemorySliceRangeIterator<SL> {
    type Item = SL;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor < self.end {
            let res = self.cursor;
            self.cursor = self.cursor + size_of::<SL>();
            Some(SL::from_address(res))
        } else {
            None
        }
    }
}

impl<SL: SlotExtra> From<SimpleMemorySlice<SL>> for SimpleMemorySliceRangeIterator<SL> {
    fn from(value: SimpleMemorySlice<SL>) -> Self {
        let start = value.range.start.as_address();
        let end = value.range.end.as_address();
        Self {
            cursor: start,
            end,
            marker: PhantomData,
        }
    }
}

impl<SL: SlotExtra> MemorySlice for SimpleMemorySlice<SL> {
    type SlotType = SL;
    type SlotIterator = SimpleMemorySliceRangeIterator<SL>;

    fn iter_slots(&self) -> Self::SlotIterator {
        SimpleMemorySliceRangeIterator {
            cursor: self.range.start.as_address(),
            end: self.range.end.as_address(),
            marker: PhantomData,
        }
    }

    fn object(&self) -> Option<ObjectReference> {
        None
    }

    fn start(&self) -> Address {
        self.range.start.as_address()
    }

    fn bytes(&self) -> usize {
        self.range.end.as_address() - self.range.start.as_address()
    }

    fn copy(src: &Self, tgt: &Self) {
        unsafe {
            let bytes = tgt.bytes();
            let src = src.start().to_ptr::<u8>();
            let dst = tgt.start().to_mut_ptr::<u8>();
            std::ptr::copy(src, dst, bytes);
        }
    }
}

pub struct GcPtr<T> {
    ptr: NonNull<T>,
}

unsafe impl<T: Send> Send for GcPtr<T> {}
unsafe impl<T: Sync> Sync for GcPtr<T> {}

impl<T> GcPtr<T> {
    pub fn new(ptr: NonNull<T>) -> Self {
        Self { ptr }
    }

    pub fn as_ptr(&self) -> *mut T {
        self.ptr.as_ptr()
    }

    pub fn as_ref(&self) -> &T {
        unsafe { self.ptr.as_ref() }
    }

    pub fn as_mut(&mut self) -> &mut T {
        unsafe { self.ptr.as_mut() }
    }

    pub fn as_object(&self) -> VMKitObject {
        VMKitObject::from_address(self.as_address())
    }

    pub fn as_address(&self) -> Address {
        Address::from_mut_ptr(self.ptr.as_ptr())
    }

    pub fn from_address(address: Address) -> Self {
        assert!(!address.is_zero());
        Self {
            ptr: NonNull::new(address.to_mut_ptr()).unwrap(),
        }
    }

    pub fn from_ptr(ptr: *mut T) -> Self {
        assert!(!ptr.is_null());
        Self {
            ptr: NonNull::new(ptr).unwrap(),
        }
    }
}

impl<T> GcPtr<MaybeUninit<T>> {
    pub unsafe fn assume_init(self) -> GcPtr<T> {
        GcPtr::new(self.ptr.cast())
    }
}

impl<T> std::ops::Deref for GcPtr<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

impl<T> std::ops::DerefMut for GcPtr<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.as_mut()
    }
}

impl<T: fmt::Debug> fmt::Debug for GcPtr<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", **self)
    }
}

impl<T: Hash> Hash for GcPtr<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

impl<T: PartialEq> PartialEq for GcPtr<T> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl<T: Eq> Eq for GcPtr<T> {}

impl<T: PartialOrd> PartialOrd for GcPtr<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        (**self).partial_cmp(&**other)
    }
}

impl<T: Ord> Ord for GcPtr<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (**self).cmp(&**other)
    }
}

impl<T> Clone for GcPtr<T> {
    fn clone(&self) -> Self {
        Self::new(self.ptr)
    }
}

impl<T> Copy for GcPtr<T> {}

impl<T> Trace for GcPtr<T> {
    fn trace_object(&mut self, tracer: &mut dyn mmtk::vm::ObjectTracer) {
        unsafe {
            let object = self.as_object().as_object_unchecked();
            let new = tracer.trace_object(object);
            if new != object {
                self.ptr = NonNull::new(new.to_raw_address().to_mut_ptr()).unwrap();
            }
        }
    }
}
