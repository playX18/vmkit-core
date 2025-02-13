# Using VMKit as BDWGC replacement

VMKit provides a BDWGC API shim which allows you to use VMKit as a drop-in replacement for BDWGC. In most of the
cases it runs out of the box without any changes and provides 2-3x performance improvement. 

# How to use

- Build VMKit with `--features uncooperative` and then install `libvmkit.so` from `target/release` or `target/debug`
in a way that pkg-config or build system of your project can find it.
- Swap `-lgc` with `-lvmkit` in your build command.
- Done.

## Notes

- VMKit does not support every feature of BDWGC, some shims are no-ops.
- It is necessary to manually call `GC_init()` on program startup. 
- We do not redirect pthread functions, use `GC_pthread_*` functions directly
or register your threads with `GC_register_my_thread()` and `GC_unregister_my_thread()`. 
- `GC_pthread_create` will wait for thread to be started and registered with GC. 
- Dynamic library support is minimal: we simply invoke `dl_iterate_phdr()` to scan loaded
data segments for GC roots. 
- Finalization is not yet implemented, only disappearing links are provided. 
- Interior pointers are limited to displacement of 128 bytes from base pointer.

## Implementation

BDWGC shim is implemented in `src/bdwgc_shim.rs`. We simply map all APIs to `MemoryManager` methods
or other functions from `mmtk` crate. Since MMTk requires all objects to provide `scan` method,
each allocated object actually stores a vtable with necessary metadata to implement it. This vtable
is stored in the object header, right behind the object payload. 

Thread suspension is implemented using `pthread_kill`, the mechanism is similar to that of BDWGC, but we
rely on `sigsuspend` instead of spin waiting. 