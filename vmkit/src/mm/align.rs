use mmtk::util::Address;

use crate::VirtualMachine;

pub const fn round_down(x: isize, align: isize) -> isize {
    x & -align
}

pub const fn round_up(x: isize, align: isize, offset: isize) -> isize {
    round_down(x + align - 1 + offset, align) - offset
}

pub const fn is_aligned(x: isize, alignment: isize, offset: isize) -> bool {
    (x & (alignment - 1)) == offset
}

#[inline(always)]
pub fn align_allocation_inner<VM: VirtualMachine>(
    region: Address,
    alignment: usize,
    offset: usize,
    _known_alignment: usize,
    fill: bool,
) -> Address {
    // No alignment ever required.
    /*if alignment <= known_alignment || VM::MAX_ALIGNMENT <= VM::MIN_ALIGNMENT {
        return region;
    }*/

    // May require alignment
    let mask = (alignment - 1) as isize;
    let neg_off: isize = -(offset as isize);
    let delta = neg_off.wrapping_sub(region.as_usize() as isize) & mask;

    if fill && VM::ALIGNMENT_VALUE != 0 {
        fill_alignment_gap::<VM>(region, region + delta as usize);
    }

    let x = region + delta;

    x
}

/// Fill the specified region with the alignment value.
pub fn fill_alignment_gap<VM: VirtualMachine>(immut_start: Address, end: Address) {
    let mut start = immut_start;

    if VM::MAX_ALIGNMENT - VM::MIN_ALIGNMENT == size_of::<u32>() {
        // At most a single hole
        if end - start != 0 {
            unsafe {
                start.store(VM::ALIGNMENT_VALUE);
            }
        }
    } else {
        while start < end {
            unsafe {
                start.store(VM::ALIGNMENT_VALUE);
            }
            start += size_of::<u32>();
        }
    }
}

pub fn align_allocation_no_fill<VM: VirtualMachine>(
    region: Address,
    alignment: usize,
    offset: usize,
) -> Address {
    align_allocation_inner::<VM>(region, alignment, offset, VM::MIN_ALIGNMENT, false)
}

pub fn align_allocation<VM: VirtualMachine>(
    region: Address,
    alignment: usize,
    offset: usize,
) -> Address {
    align_allocation_inner::<VM>(region, alignment, offset, VM::MIN_ALIGNMENT, true)
}
