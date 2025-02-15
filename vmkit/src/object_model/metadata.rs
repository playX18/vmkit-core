pub use easy_bitfield::{FromBitfield, ToBitfield};
use mmtk::{
    util::ObjectReference,
    vm::{ObjectTracer, SlotVisitor},
};

use crate::{mm::traits::ToSlot, VirtualMachine};

use super::object::VMKitObject;

#[repr(C)]
pub struct GCMetadata<VM: VirtualMachine> {
    pub trace: TraceCallback<VM>,
    pub instance_size: usize,
    pub compute_size: Option<fn(VMKitObject) -> usize>,
    pub alignment: usize,
    pub compute_alignment: Option<fn(VMKitObject) -> usize>,
}

#[derive(Debug)]
pub enum TraceCallback<VM: VirtualMachine> {
    ScanSlots(fn(VMKitObject, &mut dyn SlotVisitor<VM::Slot>)),
    TraceObject(fn(VMKitObject, &mut dyn ObjectTracer)),
    None,
}

/// Metadata for objects in VM which uses VMKit.
///
/// Main purpose of this trait is to provide VMKit a way to get GC metadata from the object.
/// It is also used to convert object references to metadata and vice versa if you store
/// metadata as an object field.
///
/// Types which implement `Metadata` must also be convertible to and from bitfields.
pub trait Metadata<VM: VirtualMachine>:
    ToBitfield<usize> + FromBitfield<usize> + ToSlot<VM::Slot>
{
    /// Size of the metadata in bits. Must be `<= 62`.
    const METADATA_BIT_SIZE: usize;

    fn gc_metadata(&self) -> &'static GCMetadata<VM>;

    fn is_object(&self) -> bool;

    fn to_object_reference(&self) -> Option<ObjectReference>;
    fn from_object_reference(reference: ObjectReference) -> Self;
}

/// Maximum object size that can be handled by uncooperative GC (64GiB default).
///
/// This is due to the fact that we store the object size in the metadata
/// and it needs to fit into 58 bits we give to runtime. By limiting the size
/// we get some free space for runtime to use as well.
pub const MAX_UNCOOPERATIVE_OBJECT_SIZE: usize = 1usize << 36;

#[macro_export]
macro_rules! make_uncooperative_metadata {
    ($max_obj_size: expr, $name: ident) => {
        pub struct $name {
            pub wsize: usize,
        }

        const _: () = {
            assert!($max_obj_size <= $crate::object_model::metadata::MAX_UNCOOPERATIVE_OBJECT_SIZE);
        };

        impl<VM: VirtualMachine> Metadata<VM> for $name {
            const METADATA_BIT_SIZE: usize = 36;

            fn gc_metadata(&self) -> &'static GCMetadata<VM> {
                &UNCOOPERATIVE_GC_META
            }

            fn is_object(&self) -> bool {
                false
            }

            fn to_object_reference(&self) -> Option<ObjectReference> {
                None
            }

            fn from_object_reference(_: ObjectReference) -> Self {
                unreachable!()
            }
        }
        impl<VM: $crate::VirtualMachine> $crate::object_model::metadata::ToBitfield<usize> for $name {
            fn to_bitfield(self) -> usize {
                self.wsize as usize
            }
        }

        impl<VM: $crate::VirtualMachine> $crate::object_model::metadata::FromBitfield<usize>
            for $name
        {
            fn from_bitfield(value: usize) -> Self {
                Self {
                    wsize: value as usize,
                }
            }
        }
    };
}

impl<VM: VirtualMachine> ToBitfield<usize> for &'static GCMetadata<VM> {
    fn to_bitfield(self) -> usize {
        let res = self as *const GCMetadata<VM> as usize as usize;
        res
    }

    fn one() -> Self {
        unreachable!()
    }

    fn zero() -> Self {
        unreachable!()
    }
}

impl<VM: VirtualMachine> FromBitfield<usize> for &'static GCMetadata<VM> {
    fn from_bitfield(value: usize) -> Self {
        unsafe { &*(value as usize as *const GCMetadata<VM>) }
    }

    fn from_i64(_value: i64) -> Self {
        unreachable!()
    }
}

impl<VM: VirtualMachine> Metadata<VM> for &'static GCMetadata<VM> {
    const METADATA_BIT_SIZE: usize = 58;

    fn gc_metadata(&self) -> &'static GCMetadata<VM> {
        *self
    }

    fn is_object(&self) -> bool {
        false
    }

    fn to_object_reference(&self) -> Option<ObjectReference> {
        None
    }

    fn from_object_reference(_: ObjectReference) -> Self {
        unreachable!()
    }
}

impl<VM: VirtualMachine> ToSlot<VM::Slot> for &'static GCMetadata<VM> {
    fn to_slot(&self) -> Option<VM::Slot> {
        None
    }
}
