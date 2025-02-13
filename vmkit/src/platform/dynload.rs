#![allow(unused_imports)]
//! OS-specific code for dynamic loading of shared libraries.
//!
//! Also provides API to iterate over the sections of a shared library.
//!
//! Note: at the moment only Linux and Android targets are supported, other targets
//! simply do no-op.
use mmtk::util::Address;
use std::{collections::HashSet, mem::offset_of};

cfg_if::cfg_if! {
    if #[cfg(any(target_os="linux", target_os="android"))] {

        #[cfg(target_pointer_width="64")]
        type ElfPhdr = libc::Elf64_Phdr;
        #[cfg(target_pointer_width="32")]
        type ElfPhdr = libc::Elf32_Phdr;

        unsafe extern "C" fn register_dynlib_callback(
            info: *mut libc::dl_phdr_info,
            size: usize,
            ptr: *mut libc::c_void,
        ) -> libc::c_int {
            let mut p: *const ElfPhdr;
            let set = ptr.cast::<HashSet<(Address, Address)>>().as_mut().unwrap();
            if size < offset_of!(libc::dl_phdr_info, dlpi_phnum) + size_of_val(&(*info).dlpi_phnum) {
                return 1; /* stop */
            }

            p = (*info).dlpi_phdr;

            for _ in 0..(*info).dlpi_phnum {
                if (*p).p_type == libc::PT_LOAD {
                    // skip non-writable segments
                    if ((*p).p_flags & libc::PF_W) == 0 {
                        p = p.add(1);
                        continue;
                    }

                    let my_start = Address::from_usize((*p).p_vaddr as usize) + (*info).dlpi_addr as usize;
                    let my_end = my_start + (*p).p_memsz as usize;

                    set.insert((my_start, my_end));
                }
                p = p.add(1);
            }

            p = (*info).dlpi_phdr;

            for _ in 0..(*info).dlpi_phnum {
                if (*p).p_type == libc::PT_GNU_RELRO {
                    /* This entry is known to be constant and will eventually be    */
                    /* remapped as read-only.  However, the address range covered   */
                    /* by this entry is typically a subset of a previously          */
                    /* encountered "LOAD" segment, so we need to exclude it.        */
                    let my_start = Address::from_usize((*p).p_vaddr as usize) + (*info).dlpi_addr as usize;
                    let my_end = my_start + (*p).p_memsz as usize;

                    let _ = my_end;
                    let _ = my_start;
                }
            }

            0
        }

        /// Returns mutable sections of all dynamic libraries loaded in the process.
        pub fn dynamic_libraries_sections() -> HashSet<(Address, Address)> {
            unsafe {
                let mut set = HashSet::new();
                libc::dl_iterate_phdr(
                    Some(register_dynlib_callback),
                    &mut set as *mut _ as *mut libc::c_void,
                );

                set
            }
        }
    } else {
        pub fn dynamic_libraries_sections() -> HashSet<(Address, Address)> {
            HashSet::new()
        }
    }
}
