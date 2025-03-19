# Using VMKit as BDWGC replacement

VMKit provides a BDWGC API shim which allows you to use VMKit as a drop-in replacement for BDWGC. In most of the
cases it runs out of the box without any changes and provides 2-3x performance improvement. 

# How to use

- Build VMKit with `--features uncooperative` and then install `libvmkit.so` from `target/release` or `target/debug`
in a way that pkg-config or build system of your project can find it.
- Swap `-lgc` with `-lvmkit` in your build command.
- Done.

Once your runtime runs with BDWGC shim (or sometims fails with some unimplemented APIs...) it's probably time
to start working on porting from BDWGC API to VMKit. This can be done by carefully replacing all `GC_malloc` calls
with calls to `MemoryManager::allocate` and also adding necessary vtables for the types you wish to allocate on VMKit heap.

Here's a small example of how this can be done:

```c

// old, BDWGC code

typedef struct {
    Scm car;
    Scm cdr;
} Pair;

Scm makeCons(Scm car, Scm cdr) {
    Pair* pair = GC_malloc(sizeof(Pair));
    pair->car = car;
    pair->cdr = cdr;

    return (Scm)pair;
}

// new, VMKit based code (note that C API bindings atm are on user side, vmkit does not provide them yet)
typedef struct {
    Scm car;
    Scm cdr;
} Pair;

void tracePair(void* object, ObjectTracer tracer) {
    vmkit_trace((Scm*)object + offsetof(car), tracer);
    vmkit_trace((Scm*)object + offsetof(cdr), tracer);
}

GCMetadata pairMetadata = {
    .trace = TRACE_CALLBACK_TRACE(tracePair),
    .instance_size = sizeof(Pair),
    .alignment = sizeof(uintptr_t),
    .compute_size = NULL,
    .compute_alignment = NULL
};

Scm makeCons(Scm car, Scm cdr) {
    Pair* pair = vmkit_memorymanager_alloc(&pairMetadata, sizeof(Pair), sizeof(uintptr_t), AllocationSemantics_Default);

    pair->car = car;
    pair->cdr = cdr;

    return pair;
}

```





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