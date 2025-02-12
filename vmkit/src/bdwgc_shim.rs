//! # BDWGC shim
//!
//! This file provides a shim for BDWGC APIs. It is used to provide a compatibility layer between MMTk and BDWGC.
//!
//! # Notes
//!
//! This shim is highly experimental and not all BDWGC APIs are implemented.
#![allow(non_upper_case_globals)]
use std::{
    collections::HashSet,
    ffi::CStr,
    mem::transmute,
    sync::{Arc, Barrier, LazyLock, OnceLock},
};

use crate::{
    mm::{conservative_roots::ConservativeRoots, traits::ToSlot, MemoryManager},
    object_model::{
        metadata::{GCMetadata, Metadata, TraceCallback},
        object::VMKitObject,
    },
    threading::{GCBlockAdapter, Thread, ThreadContext},
    VMKit, VirtualMachine,
};
use easy_bitfield::*;
use mmtk::{util::Address, vm::slot::SimpleSlot, AllocationSemantics, MMTKBuilder};
use parking_lot::{Mutex, Once};
use sysinfo::{MemoryRefreshKind, RefreshKind};

/// A BDWGC type that implements VirtualMachine.
pub struct BDWGC {
    vmkit: VMKit<Self>,
    roots: Mutex<HashSet<(Address, Address)>>,
}

pub struct BDWGCThreadContext;

impl ThreadContext<BDWGC> for BDWGCThreadContext {
    fn new(_: bool) -> Self {
        Self
    }

    fn save_thread_state(&self) {}

    fn scan_conservative_roots(
        &self,
        croots: &mut crate::mm::conservative_roots::ConservativeRoots,
    ) {
        let _ = croots;
    }

    fn scan_roots(
        &self,
        factory: impl mmtk::vm::RootsWorkFactory<<BDWGC as crate::VirtualMachine>::Slot>,
    ) {
        let _ = factory;
    }
}

static BDWGC_VM: OnceLock<BDWGC> = OnceLock::new();

impl VirtualMachine for BDWGC {
    type ThreadContext = BDWGCThreadContext;
    type BlockAdapterList = (GCBlockAdapter, ());
    type Metadata = BDWGCMetadata;
    type Slot = SimpleSlot;

    fn get() -> &'static Self {
        BDWGC_VM.get().expect("GC is not initialized")
    }

    fn vmkit(&self) -> &VMKit<Self> {
        &self.vmkit
    }

    fn vm_live_bytes() -> usize {
        0
    }

    fn prepare_for_roots_re_scanning() {}

    fn forward_weak_refs(
        _worker: &mut mmtk::scheduler::GCWorker<crate::mm::MemoryManager<Self>>,
        _tracer_context: impl mmtk::vm::ObjectTracerContext<crate::mm::MemoryManager<Self>>,
    ) {
    }

    fn scan_roots_in_mutator_thread(
        tls: mmtk::util::VMWorkerThread,
        mutator: &'static mut mmtk::Mutator<crate::mm::MemoryManager<Self>>,
        factory: impl mmtk::vm::RootsWorkFactory<
            <crate::mm::MemoryManager<Self> as mmtk::vm::VMBinding>::VMSlot,
        >,
    ) {
        let _ = tls;
        let _ = mutator;
        let _ = factory;
    }

    fn scan_vm_specific_roots(
        tls: mmtk::util::VMWorkerThread,
        mut factory: impl mmtk::vm::RootsWorkFactory<
            <crate::mm::MemoryManager<Self> as mmtk::vm::VMBinding>::VMSlot,
        >,
    ) {
        let _ = tls;
        let mut croots = ConservativeRoots::new();

        unsafe {
            croots.add_span(gc_data_start(), gc_data_end());
        }

        for (low, high) in BDWGC::get().roots.lock().iter() {
            unsafe {
                croots.add_span(*low, *high);
            }
        }

        croots.add_to_factory(&mut factory);
    }

    fn notify_initial_thread_scan_complete(_partial_scan: bool, _tls: mmtk::util::VMWorkerThread) {}

    fn post_forwarding(_tls: mmtk::util::VMWorkerThread) {}

    fn out_of_memory(tls: mmtk::util::VMThread, err_kind: mmtk::util::alloc::AllocationError) {
        let _ = tls;
        let _ = err_kind;

        unsafe {
            if let Some(oom_func) = OOM_FUNC {
                oom_func(0);
            } else {
                eprintln!("Out of memory: {:?}", err_kind);
                std::process::exit(1);
            }
        }
    }
}

type VTableAddress = BitField<usize, usize, 0, 48, false>;
type IsAtomic = BitField<usize, bool, { VTableAddress::NEXT_BIT }, 1, false>;
#[allow(dead_code)]
type HasVTable = BitField<usize, bool, { IsAtomic::NEXT_BIT }, 1, false>;
/// Object size in words, overrides [VTableAddress] as if vtable is present, object size must be available
/// through it.
type ObjectSize = BitField<usize, usize, 0, 48, false>;

/// An object metadata. This allows GC to scan object fields. When you don't use `gcj` API and don't provide vtable
/// this type simply stores object size and whether it is ATOMIC or no.
pub struct BDWGCMetadata {
    meta: usize,
}

impl ToSlot<SimpleSlot> for BDWGCMetadata {
    fn to_slot(&self) -> Option<SimpleSlot> {
        None
    }
}

static CONSERVATIVE_METADATA: GCMetadata<BDWGC> = GCMetadata {
    alignment: 8,
    instance_size: 0,
    compute_size: Some(|object| {
        let header = object.header::<BDWGC>().metadata();
        ObjectSize::decode(header.meta) * BDWGC::MIN_ALIGNMENT
    }),

    trace: TraceCallback::TraceObject(|object, tracer| unsafe {
        let is_atomic = IsAtomic::decode(object.header::<BDWGC>().metadata().meta);
        if is_atomic {
            return;
        }
        println!("trace {:?}", object.object_start::<BDWGC>());
        let size = object.bytes_used::<BDWGC>();

        let mut cursor = object.object_start::<BDWGC>();
        let end = cursor + size;

        while cursor < end {
            let word = cursor.load::<Address>();
            if let Some(object) = mmtk::memory_manager::find_object_from_internal_pointer(word, 128)
            {
                tracer.trace_object(object);
            }

            cursor += BDWGC::MIN_ALIGNMENT;
        }
    }),
};

impl FromBitfield<u64> for BDWGCMetadata {
    fn from_bitfield(value: u64) -> Self {
        Self {
            meta: value as usize,
        }
    }

    fn from_i64(value: i64) -> Self {
        Self::from_bitfield(value as u64)
    }
}

impl ToBitfield<u64> for BDWGCMetadata {
    fn to_bitfield(self) -> u64 {
        self.meta as u64
    }

    fn one() -> Self {
        Self::from_bitfield(1)
    }

    fn zero() -> Self {
        Self::from_bitfield(0)
    }
}

impl Metadata<BDWGC> for BDWGCMetadata {
    const METADATA_BIT_SIZE: usize = 58;
    fn from_object_reference(_reference: mmtk::util::ObjectReference) -> Self {
        todo!("GCJ-style metadata")
    }

    fn to_object_reference(&self) -> Option<mmtk::util::ObjectReference> {
        todo!("GCJ-style metadata")
    }

    fn is_object(&self) -> bool {
        false
    }

    fn gc_metadata(&self) -> &'static crate::object_model::metadata::GCMetadata<BDWGC> {
        &CONSERVATIVE_METADATA
    }
}

static INIT: Once = Once::new();

#[no_mangle]
pub static mut GC_VERBOSE: i32 = 0;

static BUILDER: LazyLock<Mutex<MMTKBuilder>> = LazyLock::new(|| Mutex::new(MMTKBuilder::new()));

#[no_mangle]
pub extern "C-unwind" fn GC_get_parallel() -> libc::c_int {
    *BUILDER.lock().options.threads as _
}

#[no_mangle]
pub extern "C-unwind" fn GC_set_markers_count(count: libc::c_int) {
    BUILDER.lock().options.threads.set(count as _);
}

static mut OOM_FUNC: Option<extern "C" fn(usize) -> *mut libc::c_void> = None;

#[no_mangle]
pub extern "C-unwind" fn GC_set_oom_fn(func: extern "C" fn(usize) -> *mut libc::c_void) {
    unsafe { OOM_FUNC = Some(func) };
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_oom_fn() -> extern "C" fn(usize) -> *mut libc::c_void {
    unsafe { OOM_FUNC.unwrap() }
}

#[no_mangle]
pub extern "C-unwind" fn GC_init() {
    INIT.call_once(|| unsafe {
        env_logger::init_from_env("GC_VERBOSE");

        let mut builder = BUILDER.lock();
        builder
            .options
            .plan
            .set(mmtk::util::options::PlanSelector::Immix);
        if GC_use_entire_heap != 0 {
            let mem = sysinfo::System::new_with_specifics(
                RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
            );
            builder
                .options
                .gc_trigger
                .set(mmtk::util::options::GCTriggerSelector::FixedHeapSize(
                    (mem.total_memory() as f64 * 0.5f64) as usize,
                ));
        }

        let vm = BDWGC {
            vmkit: VMKit::new(&mut builder),
            roots: Mutex::new(HashSet::new()),
        };

        BDWGC_VM.set(vm).unwrap_or_else(|_| {
            eprintln!("GC already initialized");
            std::process::exit(1);
        });

        Thread::<BDWGC>::register_mutator_manual();
        mmtk::memory_manager::initialize_collection(
            &BDWGC::get().vmkit().mmtk,
            transmute(Thread::<BDWGC>::current()),
        )
    });
}

#[no_mangle]
pub extern "C-unwind" fn GC_register_mutator() {
    unsafe { Thread::<BDWGC>::register_mutator_manual() };
}

#[no_mangle]
pub extern "C-unwind" fn GC_unregister_mutator() {
    unsafe { Thread::<BDWGC>::unregister_mutator_manual() };
}

#[no_mangle]
pub extern "C-unwind" fn GC_pthread_create(
    thread_ptr: &mut libc::pthread_t,
    _: &libc::pthread_attr_t,
    start_routine: extern "C" fn(*mut libc::c_void),
    arg: *mut libc::c_void,
) -> libc::c_int {
    let barrier = Arc::new(Barrier::new(1));
    let barrier2 = barrier.clone();

    let thread = Thread::<BDWGC>::for_mutator(BDWGCThreadContext);
    let addr = Address::from_ref(thread_ptr);
    let arg = Address::from_mut_ptr(arg);
    thread.start(move || unsafe {
        barrier2.wait();
        let thread = Thread::<BDWGC>::current();
        addr.store(thread.platform_handle());
        start_routine(arg.to_mut_ptr());
    });

    barrier.wait();

    0
}

#[no_mangle]
pub extern "C-unwind" fn GC_pthread_exit(retval: *mut libc::c_void) {
    let thread = Thread::<BDWGC>::current();
    unsafe {
        thread.terminate();
        libc::pthread_exit(retval);
    }
}

#[no_mangle]
pub extern "C-unwind" fn GC_pthread_join(
    thread: libc::pthread_t,
    retval: *mut *mut libc::c_void,
) -> libc::c_int {
    unsafe { libc::pthread_join(thread, retval) }
}

#[no_mangle]
pub extern "C-unwind" fn GC_gcollect() {
    MemoryManager::<BDWGC>::request_gc();
}

#[no_mangle]
pub extern "C-unwind" fn GC_set_find_leak(_: libc::c_int) {}

#[no_mangle]
pub extern "C-unwind" fn GC_get_find_leak() -> libc::c_int {
    0
}

#[no_mangle]
pub extern "C-unwind" fn GC_set_all_interior_pointers(_: libc::c_int) {}

#[no_mangle]
pub extern "C-unwind" fn GC_get_all_interior_pointers() -> libc::c_int {
    1
}

#[no_mangle]
pub extern "C-unwind" fn GC_set_finalize_on_demand(_: libc::c_int) {}

#[no_mangle]
pub extern "C-unwind" fn GC_get_finalize_on_demand() -> libc::c_int {
    0
}

#[no_mangle]
pub static mut GC_use_entire_heap: libc::c_int = 0;

#[no_mangle]
pub extern "C-unwind" fn GC_set_full_freq(freq: libc::c_int) {
    let _ = freq;
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_full_freq() -> libc::c_int {
    0
}

#[no_mangle]
pub extern "C-unwind" fn GC_set_non_gc_bytes(bytes: libc::c_ulong) {
    let _ = bytes;
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_non_gc_bytes() -> libc::c_ulong {
    0
}

#[no_mangle]
pub extern "C-unwind" fn GC_set_no_dls(_: libc::c_int) {}

#[no_mangle]
pub extern "C-unwind" fn GC_get_no_dls() -> libc::c_int {
    0
}

#[no_mangle]
pub extern "C-unwind" fn GC_set_free_space_divisor(divisor: libc::c_ulong) {
    let _ = divisor;
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_free_space_divisor() -> libc::c_ulong {
    0
}

#[no_mangle]
pub extern "C-unwind" fn GC_set_max_retries(retries: libc::c_ulong) {
    let _ = retries;
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_max_retries() -> libc::c_ulong {
    0
}

#[no_mangle]
pub static mut GC_stackbottom: *mut libc::c_void = std::ptr::null_mut();

#[no_mangle]
pub static mut GC_dont_precollect: libc::c_int = 0;

#[no_mangle]
pub extern "C-unwind" fn GC_set_dont_precollect(dont_precollect: libc::c_int) {
    unsafe { GC_dont_precollect = dont_precollect };
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_dont_precollect() -> libc::c_int {
    unsafe { GC_dont_precollect }
}

#[no_mangle]
pub extern "C-unwind" fn GC_set_time_limit(limit: libc::c_ulong) {
    let _ = limit;
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_time_limit() -> libc::c_ulong {
    0
}

#[repr(C)]
#[allow(non_camel_case_types)]
pub struct GC_timeval_s {
    tv_sec: libc::c_long,
    tv_usec: libc::c_long,
}

#[no_mangle]
pub extern "C-unwind" fn GC_set_time_limit_tv(limit: GC_timeval_s) {
    let _ = limit;
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_time_limit_tv() -> GC_timeval_s {
    GC_timeval_s {
        tv_sec: 0,
        tv_usec: 0,
    }
}

#[no_mangle]
pub extern "C-unwind" fn GC_set_allocd_bytes_per_finalizer(bytes: libc::c_ulong) {
    let _ = bytes;
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_allocd_bytes_per_finalizer() -> libc::c_ulong {
    0
}

#[no_mangle]
pub extern "C-unwind" fn GC_start_performance_measurement() {}

#[no_mangle]
pub extern "C-unwind" fn GC_get_full_gc_total_time() -> libc::c_ulong {
    0
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_stopped_mark_total_time() -> libc::c_ulong {
    0
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_avg_stopped_mark_time_ns() -> libc::c_ulong {
    0
}

#[no_mangle]
pub extern "C-unwind" fn GC_set_pages_executable(executable: libc::c_int) {
    let _ = executable;
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_pages_executable() -> libc::c_int {
    0
}

#[no_mangle]
pub extern "C-unwind" fn GC_set_min_bytes_allocd(bytes: libc::c_ulong) {
    let _ = bytes;
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_min_bytes_allocd() -> libc::c_ulong {
    0
}

#[no_mangle]
pub extern "C-unwind" fn GC_set_max_prior_attempts(attempts: libc::c_int) {
    let _ = attempts;
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_max_prior_attempts() -> libc::c_int {
    0
}

#[no_mangle]
pub extern "C-unwind" fn GC_set_handle_fork(handle: libc::c_int) {
    let _ = handle;
}

#[no_mangle]
pub extern "C-unwind" fn GC_atfork_prepare() {
    BDWGC::get().vmkit().mmtk.prepare_to_fork();
}

#[no_mangle]
pub extern "C-unwind" fn GC_atfork_parent() {
    let thread = Thread::<BDWGC>::current();
    BDWGC::get()
        .vmkit()
        .mmtk
        .after_fork(unsafe { transmute(thread) });
}

#[no_mangle]
pub extern "C-unwind" fn GC_atfork_child() {
    let thread = Thread::<BDWGC>::current();
    BDWGC::get()
        .vmkit()
        .mmtk
        .after_fork(unsafe { transmute(thread) });
}

#[no_mangle]
pub extern "C-unwind" fn GC_is_init_called() -> libc::c_int {
    INIT.state().done() as _
}

#[no_mangle]
pub extern "C-unwind" fn GC_deinit() {}

#[no_mangle]
pub extern "C-unwind" fn GC_malloc(size: usize) -> *mut libc::c_void {
    let vtable = BDWGCMetadata {
        meta: ObjectSize::encode(size / BDWGC::MIN_ALIGNMENT),
    };

    MemoryManager::<BDWGC>::allocate(
        Thread::<BDWGC>::current(),
        size,
        BDWGC::MIN_ALIGNMENT,
        vtable,
        AllocationSemantics::Default,
    )
    .as_address()
    .to_mut_ptr()
}

#[no_mangle]
pub extern "C-unwind" fn GC_malloc_atomic(size: usize) -> *mut libc::c_void {
    let vtable = BDWGCMetadata {
        meta: ObjectSize::encode(size / BDWGC::MIN_ALIGNMENT) | IsAtomic::encode(true),
    };

    MemoryManager::<BDWGC>::allocate(
        Thread::<BDWGC>::current(),
        size,
        BDWGC::MIN_ALIGNMENT,
        vtable,
        AllocationSemantics::Default,
    )
    .as_address()
    .to_mut_ptr()
}

#[no_mangle]
pub extern "C-unwind" fn GC_strdup(s: *const libc::c_char) -> *mut libc::c_char {
    let s = unsafe { CStr::from_ptr(s) };
    let buf = s.to_string_lossy();
    let bytes = buf.as_bytes();
    let ns = GC_malloc_atomic(bytes.len());
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ns as *mut u8, bytes.len());
    }
    ns.cast()
}

#[no_mangle]
pub extern "C-unwind" fn GC_strndup(s: *const libc::c_char, n: usize) -> *mut libc::c_char {
    let ns = GC_malloc_atomic(n);
    unsafe {
        std::ptr::copy_nonoverlapping(s, ns as *mut i8, n);
    }
    ns.cast()
}

#[no_mangle]
pub extern "C-unwind" fn GC_malloc_uncollectable(size: usize) -> *mut libc::c_void {
    let vtable = BDWGCMetadata {
        meta: ObjectSize::encode(size / BDWGC::MIN_ALIGNMENT),
    };

    MemoryManager::<BDWGC>::allocate(
        Thread::<BDWGC>::current(),
        size,
        BDWGC::MIN_ALIGNMENT,
        vtable,
        AllocationSemantics::Immortal,
    )
    .as_address()
    .to_mut_ptr()
}

#[no_mangle]
pub extern "C-unwind" fn GC_free(ptr: *mut libc::c_void) {
    let _ = ptr;
}

#[no_mangle]
pub extern "C-unwind" fn GC_malloc_stubborn(size: usize) -> *mut libc::c_void {
    GC_malloc(size)
}

#[no_mangle]
pub extern "C-unwind" fn GC_base(ptr: *mut libc::c_void) -> *mut libc::c_void {
    match mmtk::memory_manager::find_object_from_internal_pointer(Address::from_mut_ptr(ptr), 128) {
        Some(object) => object.to_raw_address().to_mut_ptr(),
        None => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C-unwind" fn GC_is_heap_ptr(ptr: *mut libc::c_void) -> libc::c_int {
    mmtk::memory_manager::is_mapped_address(Address::from_mut_ptr(ptr)) as _
}

#[no_mangle]
pub extern "C-unwind" fn GC_size(ptr: *mut libc::c_void) -> libc::c_ulong {
    let object =
        mmtk::memory_manager::find_object_from_internal_pointer(Address::from_mut_ptr(ptr), 128);
    match object {
        Some(object) => VMKitObject::from(object).bytes_used::<BDWGC>() as _,
        None => 0,
    }
}

#[no_mangle]
pub extern "C-unwind" fn GC_realloc(old: *mut libc::c_void, size: usize) -> *mut libc::c_void {
    let header = VMKitObject::from_address(Address::from_mut_ptr(old))
        .header::<BDWGC>()
        .metadata();
    let mem = if IsAtomic::decode(header.meta) {
        GC_malloc_atomic(size)
    } else {
        GC_malloc(size)
    };

    unsafe {
        std::ptr::copy_nonoverlapping(old.cast::<u8>(), mem as *mut u8, size);
    }

    mem
}

#[no_mangle]
pub extern "C-unwind" fn GC_exclude_static_roots(low: *mut libc::c_void, high: *mut libc::c_void) {
    let _ = low;
    let _ = high;
}

#[no_mangle]
pub extern "C-unwind" fn GC_clear_exclusion_table() {}

#[no_mangle]
pub extern "C-unwind" fn GC_clear_roots() {
    BDWGC::get().roots.lock().clear();
}

#[no_mangle]
pub extern "C-unwind" fn GC_add_roots(low: *mut libc::c_void, high: *mut libc::c_void) {
    BDWGC::get()
        .roots
        .lock()
        .insert((Address::from_mut_ptr(low), Address::from_mut_ptr(high)));
}

#[no_mangle]
pub extern "C-unwind" fn GC_remove_roots(low: *mut libc::c_void, high: *mut libc::c_void) {
    BDWGC::get()
        .roots
        .lock()
        .remove(&(Address::from_mut_ptr(low), Address::from_mut_ptr(high)));
}

#[no_mangle]
pub extern "C-unwind" fn GC_register_displacement(displacement: usize) {
    let _ = displacement;
}

#[no_mangle]
pub extern "C-unwind" fn GC_debug_register_displacement(displacement: usize) {
    let _ = displacement;
}

#[no_mangle]
pub extern "C-unwind" fn GC_gcollect_and_unmap() {
    GC_gcollect();
}

#[no_mangle]
pub extern "C-unwind" fn GC_try_to_collect() -> libc::c_int {
    MemoryManager::<BDWGC>::request_gc() as _
}

#[no_mangle]
pub extern "C-unwind" fn GC_set_stop_func(func: extern "C" fn() -> libc::c_int) {
    let _ = func;
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_stop_func() -> Option<extern "C" fn() -> libc::c_int> {
    None
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_heap_size() -> libc::size_t {
    mmtk::memory_manager::used_bytes(&BDWGC::get().vmkit().mmtk) as _
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_free_bytes() -> libc::size_t {
    mmtk::memory_manager::free_bytes(&BDWGC::get().vmkit().mmtk) as _
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_unmapped_bytes() -> libc::size_t {
    let mmtk = &BDWGC::get().vmkit().mmtk;
    let total = mmtk::memory_manager::total_bytes(mmtk);
    let used = mmtk::memory_manager::used_bytes(mmtk);
    total - used
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_bytes_since_gc() -> libc::size_t {
    let mmtk = &BDWGC::get().vmkit().mmtk;
    let info = mmtk::memory_manager::live_bytes_in_last_gc(mmtk);
    let total = info.iter().fold(0, |x, (_, stats)| stats.used_bytes + x);
    total as _
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_expl_freed_bytes_since_gc() -> libc::size_t {
    0
}

#[no_mangle]
pub extern "C-unwind" fn GC_get_total_bytes() -> libc::size_t {
    let mmtk = &BDWGC::get().vmkit().mmtk;
    mmtk::memory_manager::total_bytes(mmtk) as _
}

#[no_mangle]
pub extern "C-unwind" fn GC_malloc_ignore_off_page(size: usize) -> *mut libc::c_void {
    GC_malloc(size)
}

#[no_mangle]
pub extern "C-unwind" fn GC_malloc_atomic_ignore_off_page(size: usize) -> *mut libc::c_void {
    GC_malloc_atomic(size)
}

#[no_mangle]
pub extern "C-unwind" fn GC_set_warn_proc(_: *mut libc::c_void) {}

cfg_if::cfg_if! {
    if #[cfg(target_os="linux")] {
        extern "C" {
            static __data_start: *mut usize;
            static __bss_start: *mut usize;
            static _end: *mut usize;
        }

        pub fn gc_data_start() -> Address {
            unsafe {
                println!("GC data start: {:p}", &__data_start);
                Address::from_ptr(__data_start.cast::<libc::c_char>())
            }
        }

        pub fn gc_data_end() -> Address {
            unsafe {
                Address::from_ptr(_end.cast::<libc::c_char>())
            }
        }



    }
}
