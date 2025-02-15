use std::marker::PhantomData;

use crate::{mm::MemoryManager, VirtualMachine};
use header::{HashState, HASHCODE_OFFSET, OBJECT_HEADER_OFFSET, OBJECT_REF_OFFSET};
use mmtk::{
    util::{alloc::fill_alignment_gap, constants::LOG_BYTES_IN_ADDRESS, ObjectReference},
    vm::*,
};
use object::{MoveTarget, VMKitObject};

pub mod compression;
pub mod finalization;
pub mod header;
pub mod metadata;
pub mod object;

pub struct VMKitObjectModel<VM: VirtualMachine>(PhantomData<VM>);

/*
pub const LOGGING_SIDE_METADATA_SPEC: VMGlobalLogBitSpec = VMGlobalLogBitSpec::side_first();
pub const FORWARDING_POINTER_METADATA_SPEC: VMLocalForwardingPointerSpec =
    VMLocalForwardingPointerSpec::in_header(0);
pub const FORWARDING_BITS_METADATA_SPEC: VMLocalForwardingBitsSpec =
    VMLocalForwardingBitsSpec::in_header(HashStateField::NEXT_BIT as _);
pub const MARKING_METADATA_SPEC: VMLocalMarkBitSpec = VMLocalMarkBitSpec::side_first();
pub const LOS_METADATA_SPEC: VMLocalLOSMarkNurserySpec =
    VMLocalLOSMarkNurserySpec::in_header(HashStateField::NEXT_BIT as _);*/

pub const LOGGING_SIDE_METADATA_SPEC: VMGlobalLogBitSpec = VMGlobalLogBitSpec::side_first();
/// Overwrite first field of the object header
pub const LOCAL_FORWARDING_POINTER_SPEC: VMLocalForwardingPointerSpec =
    VMLocalForwardingPointerSpec::in_header(OBJECT_REF_OFFSET);

impl<VM: VirtualMachine> ObjectModel<MemoryManager<VM>> for VMKitObjectModel<VM> {
    /*const GLOBAL_LOG_BIT_SPEC: mmtk::vm::VMGlobalLogBitSpec = LOGGING_SIDE_METADATA_SPEC;
    const LOCAL_FORWARDING_POINTER_SPEC: mmtk::vm::VMLocalForwardingPointerSpec =
        FORWARDING_POINTER_METADATA_SPEC;
    const LOCAL_FORWARDING_BITS_SPEC: mmtk::vm::VMLocalForwardingBitsSpec =
        FORWARDING_BITS_METADATA_SPEC;
    const LOCAL_MARK_BIT_SPEC: mmtk::vm::VMLocalMarkBitSpec = MARKING_METADATA_SPEC;
    const LOCAL_LOS_MARK_NURSERY_SPEC: mmtk::vm::VMLocalLOSMarkNurserySpec = LOS_METADATA_SPEC;*/

    const GLOBAL_LOG_BIT_SPEC: VMGlobalLogBitSpec = VMGlobalLogBitSpec::side_first();

    #[cfg(not(feature = "vmside_forwarding"))]
    const LOCAL_FORWARDING_BITS_SPEC: VMLocalForwardingBitsSpec =
        VMLocalForwardingBitsSpec::side_first();
    #[cfg(not(feature = "vmside_forwarding"))]
    const LOCAL_FORWARDING_POINTER_SPEC: VMLocalForwardingPointerSpec =
        VMLocalForwardingPointerSpec::in_header(0);

    #[cfg(feature = "vmside_forwarding")]
    const LOCAL_FORWARDING_BITS_SPEC: VMLocalForwardingBitsSpec = VM::LOCAL_FORWARDING_BITS_SPEC;
    #[cfg(feature = "vmside_forwarding")]
    const LOCAL_FORWARDING_POINTER_SPEC: VMLocalForwardingPointerSpec =
        VM::LOCAL_FORWARDING_POINTER_SPEC;
    const LOCAL_MARK_BIT_SPEC: VMLocalMarkBitSpec =
        if Self::LOCAL_FORWARDING_BITS_SPEC.as_spec().is_on_side() {
            VMLocalMarkBitSpec::side_after(&Self::LOCAL_FORWARDING_BITS_SPEC.as_spec())
        } else {
            VMLocalMarkBitSpec::side_first()
        };

    const LOCAL_LOS_MARK_NURSERY_SPEC: VMLocalLOSMarkNurserySpec =
        VMLocalLOSMarkNurserySpec::side_after(&Self::LOCAL_MARK_BIT_SPEC.as_spec());
    const LOCAL_PINNING_BIT_SPEC: VMLocalPinningBitSpec =
        VMLocalPinningBitSpec::side_after(&Self::LOCAL_LOS_MARK_NURSERY_SPEC.as_spec());

    const OBJECT_REF_OFFSET_LOWER_BOUND: isize = OBJECT_REF_OFFSET;
    const UNIFIED_OBJECT_REFERENCE_ADDRESS: bool = false;
    const VM_WORST_CASE_COPY_EXPANSION: f64 = 1.3;

    #[cfg(feature = "cooperative")]
    const NEED_VO_BITS_DURING_TRACING: bool = VM::CONSERVATIVE_TRACING;

    fn copy(
        from: mmtk::util::ObjectReference,
        semantics: mmtk::util::copy::CopySemantics,
        copy_context: &mut mmtk::util::copy::GCWorkerCopyContext<MemoryManager<VM>>,
    ) -> mmtk::util::ObjectReference {
        let vmkit_from = VMKitObject::from(from);

        let bytes = vmkit_from.bytes_required_when_copied::<VM>();
        let align = vmkit_from.alignment::<VM>();
        let offset = vmkit_from.get_offset_for_alignment::<VM>();

        let addr = copy_context.alloc_copy(from, bytes, align, offset, semantics);

        let vmkit_to_obj = Self::move_object(vmkit_from, MoveTarget::ToAddress(addr), bytes);
        let to_obj = ObjectReference::from_raw_address(vmkit_to_obj.as_address()).unwrap();

        copy_context.post_copy(to_obj, bytes, semantics);

        to_obj
    }

    fn copy_to(
        from: mmtk::util::ObjectReference,
        to: mmtk::util::ObjectReference,
        region: mmtk::util::Address,
    ) -> mmtk::util::Address {
        let vmkit_from = VMKitObject::from(from);

        let copy = from != to;
        let bytes = if copy {
            let vmkit_to = VMKitObject::from(to);
            let bytes = vmkit_to.bytes_required_when_copied::<VM>();
            Self::move_object(vmkit_from, MoveTarget::ToObject(vmkit_to), bytes);
            bytes
        } else {
            vmkit_from.bytes_used::<VM>()
        };

        let start = Self::ref_to_object_start(to);
        fill_alignment_gap::<MemoryManager<VM>>(region, start);
        start + bytes
    }

    fn get_reference_when_copied_to(
        from: mmtk::util::ObjectReference,
        to: mmtk::util::Address,
    ) -> mmtk::util::ObjectReference {
        let vmkit_from = VMKitObject::from(from);
        let res_addr = to + OBJECT_REF_OFFSET + vmkit_from.hashcode_overhead::<VM, true>();
        debug_assert!(!res_addr.is_zero());
        // SAFETY: we just checked that the address is not zero
        unsafe { ObjectReference::from_raw_address_unchecked(res_addr) }
    }

    fn get_type_descriptor(_reference: mmtk::util::ObjectReference) -> &'static [i8] {
        unreachable!()
    }

    fn get_align_offset_when_copied(object: mmtk::util::ObjectReference) -> usize {
        VMKitObject::from_objref_nullable(Some(object)).get_offset_for_alignment::<VM>()
    }

    fn get_align_when_copied(object: mmtk::util::ObjectReference) -> usize {
        VMKitObject::from(object).alignment::<VM>()
    }

    fn get_current_size(object: mmtk::util::ObjectReference) -> usize {
        VMKitObject::from_objref_nullable(Some(object)).get_current_size::<VM>()
    }

    fn get_size_when_copied(object: mmtk::util::ObjectReference) -> usize {
        VMKitObject::from_objref_nullable(Some(object)).get_size_when_copied::<VM>()
    }

    fn ref_to_object_start(reference: mmtk::util::ObjectReference) -> mmtk::util::Address {
        VMKitObject::from_objref_nullable(Some(reference)).object_start::<VM>()
    }

    fn ref_to_header(reference: mmtk::util::ObjectReference) -> mmtk::util::Address {
        reference.to_raw_address().offset(OBJECT_HEADER_OFFSET)
    }

    fn dump_object(object: mmtk::util::ObjectReference) {
        let _ = object;
    }
}
impl<VM: VirtualMachine> VMKitObjectModel<VM> {
    fn move_object(from_obj: VMKitObject, mut to: MoveTarget, num_bytes: usize) -> VMKitObject {
        log::trace!(
            "move_object: from_obj: {}, to: {}, bytes={}",
            from_obj.as_address(),
            to,
            num_bytes
        );
        let mut copy_bytes = num_bytes;
        let mut obj_ref_offset = OBJECT_REF_OFFSET;
        let hash_state = from_obj.header::<VM>().hash_state();

        // Adjust copy bytes and object reference offset based on hash state
        match hash_state {
            HashState::Hashed => {
                copy_bytes -= size_of::<usize>(); // Exclude hash code from copy
                if let MoveTarget::ToAddress(ref mut addr) = to {
                    *addr += size_of::<usize>(); // Adjust address for hash code
                }
            }
            HashState::HashedAndMoved => {
                obj_ref_offset += size_of::<usize>() as isize; // Adjust for larger header
            }
            _ => {}
        }

        // Determine target address and object based on MoveTarget
        let (to_address, to_obj) = match to {
            MoveTarget::ToAddress(addr) => {
                let obj = VMKitObject::from_address(addr + obj_ref_offset);
                (addr, obj)
            }
            MoveTarget::ToObject(object) => {
                let addr = object.as_address() + (-obj_ref_offset);
                (addr, object)
            }
        };

        let from_address = from_obj.as_address() + (-obj_ref_offset);

        // Perform the memory copy
        unsafe {
            std::ptr::copy(
                from_address.to_ptr::<u8>(),
                to_address.to_mut_ptr::<u8>(),
                copy_bytes,
            );
        }

        // Update hash state if necessary
        if hash_state == HashState::Hashed {
            unsafe {
                let hash_code = from_obj.as_address().as_usize() >> LOG_BYTES_IN_ADDRESS;
                to_obj
                    .as_address()
                    .offset(HASHCODE_OFFSET)
                    .store(hash_code as u64);
                to_obj
                    .header::<VM>()
                    .set_hash_state(HashState::HashedAndMoved);
            }
        } else {
            to_obj.header::<VM>().set_hash_state(HashState::Hashed);
        }

        to_obj
    }
}
