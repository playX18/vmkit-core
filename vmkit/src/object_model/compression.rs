//! Pointer compression support.

use std::cell::UnsafeCell;

use mmtk::util::Address;

use crate::VirtualMachine;

use super::object::{VMKitNarrow, VMKitObject};

pub const UNSCALED_OP_HEAP_MAX: u64 = u32::MAX as u64 + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionMode {
    /// Use 32-bits oops without encoding when
    /// NarrowOopHeapBaseMin + heap_size < 4Gb
    UnscaledNarrowOp = 0,
    /// Use zero based compressed oops with encoding when
    /// NarrowOopHeapBaseMin + heap_size < 32Gb
    ZeroBasedNarrowOp = 1,
    /// Use compressed oops with disjoint heap base if
    /// base is 32G-aligned and base > 0. This allows certain
    /// optimizations in encoding/decoding.
    /// Disjoint: Bits used in base are disjoint from bits used
    /// for oops ==> oop = (cOop << 3) | base.  One can disjoint
    /// the bits of an oop into base and compressed oop.b
    DisjointBaseNarrowOp = 2,
    /// Use compressed oops with heap base + encoding.
    HeapBasedNarrowOp = 3,
}

pub struct CompressedOps {
    pub base: Address,
    pub shift: u32,
    pub range: (Address, Address),
}

struct CompressedOpsStorage(UnsafeCell<CompressedOps>);

unsafe impl Send for CompressedOpsStorage {}
unsafe impl Sync for CompressedOpsStorage {}

static COMPRESSED_OPS: CompressedOpsStorage =
    CompressedOpsStorage(UnsafeCell::new(CompressedOps {
        base: Address::ZERO,
        shift: 0,
        range: (Address::ZERO, Address::ZERO),
    }));

impl CompressedOps {
    /// Initialize compressed object pointers.
    ///
    /// # Parameters
    ///
    /// - `tagged_pointers`: Forces 16-byte alignment of objects which gives 2 bits for tagging.
    pub fn init<VM: VirtualMachine>(tagged_pointers: bool) {
        let start = mmtk::memory_manager::starting_heap_address();
        let end = mmtk::memory_manager::last_heap_address();

        if tagged_pointers {
            let shift = 2;
            let base = start.sub(4096);

            unsafe {
                COMPRESSED_OPS.0.get().write(CompressedOps {
                    base,
                    shift,
                    range: (start, end),
                });
            }
            return;
        }

        let shift;
        // Subtract a page because something can get allocated at heap base.
        // This also makes implicit null checking work, because the
        // memory+1 page below heap_base needs to cause a signal.
        // See needs_explicit_null_check.
        // Only set the heap base for compressed oops because it indicates
        // compressed oops for pstack code.
        if end > unsafe { Address::from_usize(UNSCALED_OP_HEAP_MAX as usize) } {
            // Didn't reserve heap below 4Gb.  Must shift.
            shift = VM::MIN_ALIGNMENT.trailing_zeros();
        } else {
            shift = 0;
        }
        let max = (u32::MAX as u64 + 1) << VM::MIN_ALIGNMENT.trailing_zeros();
        let base = if end <= unsafe { Address::from_usize(max as usize) } {
            // heap below 32Gb, can use base == 0
            Address::ZERO
        } else {
            start.sub(4096)
        };

        unsafe {
            COMPRESSED_OPS.0.get().write(CompressedOps {
                base,
                shift,
                range: (start, end),
            });
        }
    }

    pub fn base() -> Address {
        unsafe { (*COMPRESSED_OPS.0.get()).base }
    }

    pub fn shift() -> u32 {
        unsafe { (*COMPRESSED_OPS.0.get()).shift }
    }

    pub fn decode_raw(v: VMKitNarrow) -> VMKitObject {
        debug_assert!(!v.is_null());
        let base = Self::base();
        VMKitObject::from_address(base + ((v.raw() as usize) << Self::shift()))
    }

    pub fn decode(v: VMKitNarrow) -> VMKitObject {
        if v.is_null() {
            VMKitObject::NULL
        } else {
            Self::decode_raw(v)
        }
    }

    pub fn encode(v: VMKitObject) -> VMKitNarrow {
        if v.is_null() {
            unsafe { VMKitNarrow::from_raw(0) }
        } else {
            let pd = v.as_address().as_usize() - Self::base().as_usize();
            unsafe { VMKitNarrow::from_raw((pd >> Self::shift()) as u32) }
        }
    }

    pub const fn is_null(v: VMKitNarrow) -> bool {
        v.raw() == 0
    }
}
