#![allow(dead_code, unused_imports, unused_variables)]
//! Address space layout randomization for MMTk.

use mmtk::util::{
    conversions::raw_align_down,
    heap::vm_layout::{VMLayout, BYTES_IN_CHUNK},
    options::{GCTriggerSelector, Options},
    Address,
};
use rand::Rng;

fn get_random_mmap_addr() -> Address {
    let mut rng = rand::rng();
    let uniform = rand::distr::Uniform::new(0, usize::MAX).unwrap();
    let mut raw_addr = rng.sample(uniform);

    raw_addr = raw_align_down(raw_addr, BYTES_IN_CHUNK);

    raw_addr &= 0x3FFFFFFFF000;

    unsafe { Address::from_usize(raw_addr) }
}

fn is_address_maybe_unmapped(addr: Address, size: usize) -> bool {
    #[cfg(unix)]
    {
        unsafe {
            let res = libc::msync(addr.to_mut_ptr(), size, libc::MS_ASYNC);
            if res == -1 && errno::errno().0 == libc::ENOMEM {
                return true;
            }
        }
    }
    false
}

pub fn aslr_vm_layout(mmtk_options: &mut Options) -> VMLayout {
    /*let mut vm_layout = VMLayout::default();
    let options = &*OPTIONS;

    if options.compressed_pointers {
        vm_layout.heap_start = unsafe { Address::from_usize(0x4000_0000) };
        vm_layout.heap_end = vm_layout.heap_start + 32usize * 1024 * 1024
    }


    let size = if options.compressed_pointers {
        if options.tag_compressed_pointers {
            32 * 1024 * 1024
        } else {
            16 * 1024 * 1024
        }
    } else {
        options.max_heap_size
    };
    if options.aslr {
        vm_layout.force_use_contiguous_spaces = false;
        loop {
            let start = get_random_mmap_addr().align_down(BYTES_IN_CHUNK);
            if !is_address_maybe_unmapped(start, size) {
                continue;
            }

            let end = (start + size).align_up(BYTES_IN_CHUNK);
            vm_layout.heap_start = start;
            vm_layout.heap_end = end;

            if !options.compressed_pointers {
                vm_layout.log_address_space = if cfg!(target_pointer_width = "32") {
                    31
                } else {
                    47
                };
                vm_layout.log_space_extent = size.trailing_zeros() as usize - 1;
            }

            break;
        }


    }

    if options.compressed_pointers {
        vm_layout.log_address_space = 36;
        vm_layout.log_space_extent = 31;
    }

    mmtk_options.gc_trigger.set(GCTriggerSelector::DynamicHeapSize(
        options.min_heap_size,
        options.max_heap_size,
    ));

    vm_layout*/

    VMLayout::default()
}
