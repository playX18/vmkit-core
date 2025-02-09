//! Collection of traits for the memory manager.
//!
//! We provide all the traits to simplify implementation of a VM. You can simply
//! implement `Trace` or `Scan` to get a trace-able object for example.

use mmtk::{
    util::Address,
    vm::{
        slot::{SimpleSlot, Slot},
        ObjectModel, ObjectTracer, SlotVisitor,
    },
};

use crate::{
    object_model::{object::{VMKitNarrow, VMKitObject}, VMKitObjectModel},
    options::OPTIONS,
    VirtualMachine,
};

use super::conservative_roots::{FatInternalPointer, InternalPointer};

pub trait ToSlot<SL: Slot> {
    fn to_slot(&self) -> Option<SL>;
}

pub trait Trace {
    fn trace_object(&mut self, tracer: &mut dyn ObjectTracer);
}

pub trait Scan<SL: Slot> {
    fn scan_object(&self, visitor: &mut dyn SlotVisitor<SL>);
}

impl Trace for VMKitObject {
    fn trace_object(&mut self, tracer: &mut dyn ObjectTracer) {
        if self.is_null() {
            return;
        }
        let new_object = VMKitObject::from(tracer.trace_object((*self).try_into().unwrap()));

        if new_object != *self {
            *self = new_object;
        }
    }
}

impl<T: Trace, VM: VirtualMachine> Trace for InternalPointer<T, VM> {
    fn trace_object(&mut self, tracer: &mut dyn ObjectTracer) {
        #[cfg(feature = "cooperative")]
        {
            assert!(
                VMKitObjectModel::<VM>::NEED_VO_BITS_DURING_TRACING,
                "VO-bits are not enabled during tracing, can't use internal pointers"
            );

            let start = mmtk::memory_manager::find_object_from_internal_pointer(
                self.as_address(),
                OPTIONS.interior_pointer_max_bytes,
            );

            if let Some(start) = start {
                let offset = self.as_address() - start.to_raw_address();
                let new_object = VMKitObject::from(tracer.trace_object(start));
                *self = InternalPointer::new(new_object.as_address() + offset);
            }
        }
        #[cfg(not(feature = "cooperative"))]
        {
            unreachable!("Internal pointers are not supported in precise mode");
        }
    }
}

impl<SL: Slot + SlotExtra> Scan<SL> for VMKitObject {
    fn scan_object(&self, visitor: &mut dyn SlotVisitor<SL>) {
        if let Some(slot) = self.to_slot() {
            visitor.visit_slot(slot);
        }
    }
}

impl<SL: Slot + SlotExtra> ToSlot<SL> for VMKitObject {
    fn to_slot(&self) -> Option<SL> {
        Some(SL::from_vmkit_object(self))
    }
}

/// Extra methods for types implementing `Slot` trait from MMTK.
pub trait SlotExtra: Slot {
    /// Construct a slot from a `VMKitObject`. Must be always implemented
    /// as internally we use `VMKitObject` to represent all objects.
    fn from_vmkit_object(object: &VMKitObject) -> Self;
    fn from_address(address: Address) -> Self;

    /// Construct a slot from an `InternalPointer`. VMs are not required to implement
    /// this as InternalPointer can also be traced. 
    fn from_internal_pointer<T, VM: VirtualMachine>(pointer: &InternalPointer<T, VM>) -> Self {
        let _ = pointer;
        unimplemented!()
    }
    /// Construct a slot from a `FatInternalPointer`. VMs are not required to implement
    /// this as `FatInternalPointer` can also be traced.
    fn from_fat_internal_pointer<T, VM: VirtualMachine>(
        pointer: &FatInternalPointer<T, VM>,
    ) -> Self {
        let _ = pointer;
        unimplemented!()
    }

    fn from_narrow(narrow: &VMKitNarrow) -> Self {
        let _ = narrow;
        unimplemented!()
    }
}

impl SlotExtra for SimpleSlot {
    fn from_vmkit_object(object: &VMKitObject) -> Self {
        Self::from_address(Address::from_ptr(object))
    }

    fn from_address(address: Address) -> Self {
        SimpleSlot::from_address(address)
    }

    fn from_internal_pointer<T, VM: VirtualMachine>(pointer: &InternalPointer<T, VM>) -> Self {
        let _ = pointer;
        unimplemented!("SimpleSlot does not support internal pointers")
    }
}

impl SlotExtra for Address {
    fn from_vmkit_object(object: &VMKitObject) -> Self {
        Address::from_ptr(object)
    }

    fn from_address(address: Address) -> Self {
        address
    }

    fn from_internal_pointer<T, VM: VirtualMachine>(pointer: &InternalPointer<T, VM>) -> Self {
        let _ = pointer;
        unimplemented!("Address does not support internal pointers")
    }
}

/// Trait to check if type can be enqueued as a slot of an object.
///
/// Slot is an address of a field of an object. When field
/// can't be enqueued, we simply trace it using `ObjectTracer`.
pub trait SupportsEnqueuing {
    const VALUE: bool;
}

impl<T, VM: VirtualMachine> SupportsEnqueuing for InternalPointer<T, VM> {
    const VALUE: bool = false;
}

impl SupportsEnqueuing for VMKitObject {
    const VALUE: bool = true;
}

macro_rules! impl_prim {
    ($($t:ty)*) => {
        $(
            impl SupportsEnqueuing for $t {
                const VALUE: bool = true;
            }

            impl<SL: Slot> Scan<SL> for $t {
                fn scan_object(&self, visitor: &mut dyn SlotVisitor<SL>) {
                    let _ = visitor;
                }
            }

            impl<SL: Slot> ToSlot<SL> for $t {
                fn to_slot(&self) -> Option<SL> {
                    None
                }
            }

            impl Trace for $t {
                fn trace_object(&mut self, tracer: &mut dyn ObjectTracer) {
                    let _ = tracer;
                }
            }

        )*
    };
}

impl_prim! {
    u8 u16 u32 u64 u128 usize
    i8 i16 i32 i64 i128 isize
    f32 f64
    bool char
    String
    std::fs::File
}

impl<T: SupportsEnqueuing> SupportsEnqueuing for Vec<T> {
    const VALUE: bool = T::VALUE; // we don't enque vec itself but its elements.
}

impl<T: SupportsEnqueuing> SupportsEnqueuing for Option<T> {
    const VALUE: bool = T::VALUE;
}

impl<T: SupportsEnqueuing, U: SupportsEnqueuing> SupportsEnqueuing for Result<T, U> {
    const VALUE: bool = T::VALUE && U::VALUE;
}

impl<T: Trace> Trace for Option<T> {
    fn trace_object(&mut self, tracer: &mut dyn ObjectTracer) {
        if let Some(value) = self {
            value.trace_object(tracer);
        }
    }
}

impl<T: Trace> Trace for Vec<T> {
    fn trace_object(&mut self, tracer: &mut dyn ObjectTracer) {
        for value in self {
            value.trace_object(tracer);
        }
    }
}

impl<T: Trace, const N: usize> Trace for [T; N] {
    fn trace_object(&mut self, tracer: &mut dyn ObjectTracer) {
        for value in self {
            value.trace_object(tracer);
        }
    }
}

impl<T: Trace> Trace for Box<T> {
    fn trace_object(&mut self, tracer: &mut dyn ObjectTracer) {
        (**self).trace_object(tracer);
    }
}

impl<SL: Slot, T: Scan<SL>> Scan<SL> for Vec<T> {
    fn scan_object(&self, visitor: &mut dyn SlotVisitor<SL>) {
        for value in self {
            value.scan_object(visitor);
        }
    }
}

impl<SL: Slot, T: Scan<SL>, const N: usize> Scan<SL> for [T; N] {
    fn scan_object(&self, visitor: &mut dyn SlotVisitor<SL>) {
        for value in self {
            value.scan_object(visitor);
        }
    }
}

impl<T, VM: VirtualMachine> SupportsEnqueuing for FatInternalPointer<T, VM> {
    const VALUE: bool = true;
}

impl<T, VM: VirtualMachine, SL: Slot + SlotExtra> Scan<SL> for FatInternalPointer<T, VM> {
    fn scan_object(&self, visitor: &mut dyn SlotVisitor<SL>) {
        visitor.visit_slot(self.object().to_slot().expect("never fails"));
    }
}

impl<T, VM: VirtualMachine, SL: Slot + SlotExtra> ToSlot<SL> for FatInternalPointer<T, VM> {
    fn to_slot(&self) -> Option<SL> {
        Some(self.object().to_slot().expect("never fails"))
    }
}

impl<T, VM: VirtualMachine> Trace for FatInternalPointer<T, VM> {
    fn trace_object(&mut self, tracer: &mut dyn ObjectTracer) {
        self.object().trace_object(tracer);
    }
}


impl Trace for VMKitNarrow {
    fn trace_object(&mut self, tracer: &mut dyn ObjectTracer) {
        let mut object = self.to_object();
        object.trace_object(tracer);
        *self = VMKitNarrow::encode(object);
    }
}

impl<SL: SlotExtra> Scan<SL> for VMKitNarrow {
    fn scan_object(&self, visitor: &mut dyn SlotVisitor<SL>) {
        let slot = SL::from_narrow(self);
        visitor.visit_slot(slot);
    }
}

impl<SL: SlotExtra> ToSlot<SL> for VMKitNarrow {
    fn to_slot(&self) -> Option<SL> {
        Some(SL::from_narrow(self))
    }
}

