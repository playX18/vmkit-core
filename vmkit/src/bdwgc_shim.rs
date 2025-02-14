//! # BDWGC shim
//!
//! This file provides a shim for BDWGC APIs. It is used to provide a compatibility layer between MMTk and BDWGC.
//!
//! # Notes
//!
//! This shim is highly experimental and not all BDWGC APIs are implemented.
#![allow(non_upper_case_globals, non_camel_case_types)]
use std::{
    collections::HashSet,
    ffi::CStr,
    hash::Hash,
    mem::transmute,
    ptr::null_mut,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Barrier, LazyLock, OnceLock,
    },
};

use crate::{
    mm::{
        conservative_roots::ConservativeRoots, stack_bounds::StackBounds, traits::ToSlot,
        MemoryManager,
    },
    object_model::{
        metadata::{GCMetadata, Metadata, TraceCallback},
        object::VMKitObject,
    },
    platform::dynload::dynamic_libraries_sections,
    threading::{GCBlockAdapter, Thread, ThreadContext},
    VMKit, VirtualMachine,
};
use easy_bitfield::*;
use mmtk::{
    util::{options::PlanSelector, Address},
    vm::{
        slot::{SimpleSlot, UnimplementedMemorySlice},
        ObjectTracer,
    },
    AllocationSemantics, MMTKBuilder,
};
use parking_lot::{Mutex, Once};
use sysinfo::{MemoryRefreshKind, RefreshKind};

const GC_MAX_MARK_PROCS: usize = 1 << 6;
const MAXOBJKINDS: usize = 24;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(C)]
pub enum ObjectKind {
    Composite,
    WithDescr(u8),
}

impl ToBitfield<usize> for ObjectKind {
    fn to_bitfield(self) -> usize {
        match self {
            Self::Composite => 0,
            Self::WithDescr(kind) => 2 + kind as usize,
        }
    }

    fn one() -> Self {
        unreachable!()
    }

    fn zero() -> Self {
        Self::Composite
    }
}

impl FromBitfield<usize> for ObjectKind {
    fn from_bitfield(value: usize) -> Self {
        match value {
            0 => Self::Composite,
            _ => Self::WithDescr((value - 2) as u8),
        }
    }

    fn from_i64(value: i64) -> Self {
        Self::from_bitfield(value as usize)
    }
}

#[repr(C)]
#[allow(non_camel_case_types)]
pub struct obj_kind {
    pub ok_freelist: *mut *mut libc::c_void,
    pub ok_reclaim_list: *mut *mut libc::c_void,

    pub ok_descriptor: usize,
    pub ok_relocate_descr: bool,
    pub ok_init: bool,
}

/// A BDWGC type that implements VirtualMachine.
pub struct BDWGC {
    vmkit: VMKit<Self>,
    roots: Mutex<HashSet<(Address, Address)>>,
    disappearing_links: Mutex<HashSet<DLink>>,
    mark_procs: Mutex<[Option<GCMarkProc>; GC_MAX_MARK_PROCS]>,
    n_procs: AtomicUsize,
    obj_kinds: Mutex<[Option<obj_kind>; MAXOBJKINDS]>,
    n_kinds: AtomicUsize,
}

unsafe impl Send for BDWGC {}
unsafe impl Sync for BDWGC {}

#[derive(PartialEq, Eq, Clone, Copy)]
struct DLink {
    object: VMKitObject,
    link: Address,
}

impl Hash for DLink {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let hashcode = self.object.hashcode::<BDWGC>();
        hashcode.hash(state);
        self.link.hash(state);
    }
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
    type MemorySlice = UnimplementedMemorySlice<Self::Slot>;

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

    fn process_weak_refs(
        _worker: &mut mmtk::scheduler::GCWorker<MemoryManager<Self>>,
        _tracer_context: impl mmtk::vm::ObjectTracerContext<MemoryManager<Self>>,
    ) -> bool {
        let bdwgc = BDWGC::get();
        let mut disappearing_links = bdwgc.disappearing_links.lock();

        disappearing_links.retain(|link| unsafe {
            let base = link.object.as_object_unchecked();
            if !base.is_reachable() {
                link.link.store(Address::ZERO);
                false
            } else {
                true
            }
        });

        false
    }

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
        let mut croots = ConservativeRoots::new(256);
        for (start, end) in dynamic_libraries_sections() {
            unsafe {
                croots.add_span(start, end);
            }
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

type VTableAddress = BitField<usize, usize, 0, 37, false>;
type ObjectKindField = BitField<usize, ObjectKind, { VTableAddress::NEXT_BIT }, 6, false>;
/// Object size in words, overrides [VTableAddress] as if vtable is present, object size must be available
/// through it.
type ObjectSize = BitField<usize, usize, 0, 37, false>;
type IsAtomic = BitField<usize, bool, { ObjectKindField::NEXT_BIT }, 1, false>;

/// An object metadata. This allows GC to scan object fields. When you don't use `gcj` API and don't provide vtable
/// this type simply stores object size and whether it is ATOMIC or no.
pub struct BDWGCMetadata {
    meta: usize,
}

impl BDWGCMetadata {
    pub fn size(&self) -> usize {
        ObjectSize::decode(self.meta) * BDWGC::MIN_ALIGNMENT
    }

    pub fn kind(&self) -> ObjectKind {
        ObjectKindField::decode(self.meta)
    }

    pub fn is_atomic(&self) -> bool {
        IsAtomic::decode(self.meta)
    }
}

impl ToSlot<SimpleSlot> for BDWGCMetadata {
    fn to_slot(&self) -> Option<SimpleSlot> {
        None
    }
}

const GC_LOG_MAX_MARK_PROCS: usize = 6;
const GC_DS_TAG_BITS: usize = 2;
const GC_DS_TAGS: usize = (1 << GC_DS_TAG_BITS) - 1;
const GC_DS_LENGTH: usize = 0;
const GC_DS_BITMAP: usize = 1;
const GC_DS_PROC: usize = 2;
const GC_DS_PER_OBJECT: usize = 3;
const GC_INDIR_PER_OBJ_BIAS: usize = 0x10;

impl BDWGC {
    unsafe fn proc(&self, descr: usize) -> Option<GCMarkProc> {
        unsafe {
            let guard = self.mark_procs.make_guard_unchecked();
            let proc = guard[(descr >> GC_DS_TAG_BITS) & (GC_MAX_MARK_PROCS - 1)];
            proc
        }
    }

    unsafe fn env(&self, descr: usize) -> usize {
        (descr >> (GC_DS_TAG_BITS + GC_LOG_MAX_MARK_PROCS)) as usize
    }
}

unsafe fn mark_range_conservative(start: Address, end: Address, tracer: &mut dyn ObjectTracer) {
    let mut cursor = start;
    while cursor < end {
        let word = cursor.load::<Address>();
        if let Some(object) = mmtk::memory_manager::find_object_from_internal_pointer(word, 128) {
            tracer.trace_object(object);
        }

        cursor += BDWGC::MIN_ALIGNMENT;
    }
}

const SIGNB: usize = 1 << (usize::BITS - 1);

struct MarkStackMeta<'a> {
    tracer: &'a mut dyn ObjectTracer,
}

static CONSERVATIVE_METADATA: GCMetadata<BDWGC> = GCMetadata {
    alignment: 8,
    instance_size: 0,
    compute_alignment: None,
    compute_size: Some(|object| {
        let header = object.header::<BDWGC>().metadata();
        ObjectSize::decode(header.meta) * BDWGC::MIN_ALIGNMENT
    }),

    trace: TraceCallback::TraceObject(|object, tracer| unsafe {
        let kind = object.header::<BDWGC>().metadata().kind();
        if object.header::<BDWGC>().metadata().is_atomic() {
            return;
        }
        let size = object.bytes_used::<BDWGC>();
        let least_ha = mmtk::memory_manager::starting_heap_address();
        let greatest_ha = mmtk::memory_manager::last_heap_address();
        match kind {
            ObjectKind::Composite => {
                let mut cursor = object.object_start::<BDWGC>();
                let end = cursor + size;

                while cursor < end {
                    let word = cursor.load::<Address>();
                    if let Some(object) =
                        mmtk::memory_manager::find_object_from_internal_pointer(word, 128)
                    {
                        tracer.trace_object(object);
                    }

                    cursor += BDWGC::MIN_ALIGNMENT;
                }
            }

            ObjectKind::WithDescr(kind) => {
                let vm = BDWGC::get();
                'retry: loop {
                    {
                        let lock = vm.obj_kinds.make_guard_unchecked();
                        let Some(kind) = &lock[kind as usize] else {
                            return;
                        };

                        let mut descr = kind.ok_descriptor;

                        let tag = descr & GC_DS_TAGS;

                        match tag {
                            GC_DS_LENGTH => {
                                let start = object.as_address();
                                let end = start + descr as usize;
                                mark_range_conservative(start, end, tracer);
                            }

                            GC_DS_BITMAP => {
                                descr &= !GC_DS_TAGS;
                                let mut curent_p = object.as_address();
                                while descr != 0 {
                                    if descr & SIGNB == 0 {
                                        continue;
                                    }

                                    let q = curent_p.load::<Address>();
                                    if least_ha <= q && q <= greatest_ha {
                                        if let Some(object) =
                                            mmtk::memory_manager::find_object_from_internal_pointer(
                                                q, 128,
                                            )
                                        {
                                            tracer.trace_object(object);
                                        }
                                    }
                                    descr <<= 1;
                                    curent_p += BDWGC::MIN_ALIGNMENT;
                                }
                            }

                            GC_DS_PROC => {
                                let Some(proc) = vm.proc(descr) else {
                                    return;
                                };

                                let mut mark_stack = MarkStackMeta { tracer };

                                proc(
                                    object.as_address().to_mut_ptr(),
                                    Address::from_mut_ptr(&mut mark_stack).to_mut_ptr(),
                                    Address::from_mut_ptr(&mut mark_stack).to_mut_ptr(),
                                    vm.env(descr),
                                );
                            }

                            GC_DS_PER_OBJECT => {
                                if (descr & SIGNB) == 0 {
                                    /* descriptor is in the object */
                                    descr = object
                                        .as_address()
                                        .add(descr - GC_DS_PER_OBJECT)
                                        .load::<usize>();
                                    let _ = descr;
                                    continue 'retry;
                                } else {
                                    /* decsriptor is in the type descriptor pointed to by the first "pointer-sized" word of the object */
                                    let type_descr = object.as_address().load::<Address>();

                                    if type_descr == Address::ZERO {
                                        return;
                                    }

                                    descr = type_descr
                                        .offset(
                                            -(descr as isize
                                                + (GC_INDIR_PER_OBJ_BIAS - GC_DS_PER_OBJECT)
                                                    as isize),
                                        )
                                        .load::<usize>();
                                    if descr == 0 {
                                        return;
                                    }
                                    continue 'retry;
                                }
                            }

                            _ => unreachable!(),
                        }
                    }
                    break;
                }
            }
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

static BUILDER: LazyLock<Mutex<MMTKBuilder>> = LazyLock::new(|| {
    Mutex::new({
        let mut builder = MMTKBuilder::new();
        builder.options.read_env_var_settings();
        if !matches!(*builder.options.plan, PlanSelector::Immix | PlanSelector::MarkSweep) {
            builder.options.plan.set(PlanSelector::Immix);
        }
        builder
    })
});

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
            disappearing_links: Mutex::new(HashSet::new()),
            mark_procs: Mutex::new([None; GC_MAX_MARK_PROCS]),
            n_procs: AtomicUsize::new(0),
            n_kinds: AtomicUsize::new(0),
            obj_kinds: Mutex::new([const { None }; 24]),
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

#[repr(C)]
pub struct GC_stack_base {
    pub mem_base: *mut libc::c_void,
}

#[no_mangle]
pub extern "C-unwind" fn GC_register_my_thread(_stack_base: *mut GC_stack_base) {
    unsafe { Thread::<BDWGC>::register_mutator_manual() };
}

#[no_mangle]
pub extern "C-unwind" fn GC_unregister_my_thread() {
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
        meta: ObjectSize::encode(size / BDWGC::MIN_ALIGNMENT)
            | ObjectKindField::encode(ObjectKind::Composite)
            | IsAtomic::encode(true),
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
    if s.is_null() {
        return null_mut();
    }
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
    let mem = if header.is_atomic() {
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

#[no_mangle]
pub extern "C-unwind" fn GC_enable() {
    MemoryManager::<BDWGC>::enable_gc();
}

#[no_mangle]
pub extern "C-unwind" fn GC_disable() {
    MemoryManager::<BDWGC>::disable_gc();
}

#[no_mangle]
pub extern "C-unwind" fn GC_is_enabled() -> libc::c_int {
    MemoryManager::<BDWGC>::is_gc_enabled() as _
}

#[no_mangle]
pub extern "C-unwind" fn GC_register_disappearing_link(link: *mut libc::c_void) {
    let base = GC_base(link);
    let dlink = DLink {
        object: VMKitObject::from_address(Address::from_mut_ptr(base)),
        link: Address::from_mut_ptr(link),
    };

    BDWGC::get().disappearing_links.lock().insert(dlink);
}

#[no_mangle]
pub extern "C-unwind" fn GC_unregister_disappearing_link(link: *mut libc::c_void) {
    let base = GC_base(link);
    let dlink = DLink {
        object: VMKitObject::from_address(Address::from_mut_ptr(base)),
        link: Address::from_mut_ptr(link),
    };

    BDWGC::get().disappearing_links.lock().remove(&dlink);
}

#[no_mangle]
pub extern "C-unwind" fn GC_general_register_disappearing_link(
    link: *mut libc::c_void,
    obj: *mut libc::c_void,
) -> libc::c_int {
    let dlink = DLink {
        object: VMKitObject::from_address(Address::from_mut_ptr(obj)),
        link: Address::from_mut_ptr(link),
    };

    (!BDWGC::get().disappearing_links.lock().insert(dlink)) as _
}

#[no_mangle]
pub extern "C-unwind" fn GC_register_finalizer(
    obj: *mut libc::c_void,
    finalizer: extern "C-unwind" fn(*mut libc::c_void, *mut libc::c_void),
    cd: *mut libc::c_void,
    ofn: *mut extern "C" fn(*mut libc::c_void),
    ocd: *mut *mut libc::c_void,
) {
    let object = GC_base(obj);
    if object.is_null() {
        return;
    }
    let _ = ofn;
    let _ = ocd;
    let cd = Address::from_mut_ptr(cd);
    let object = VMKitObject::from_address(Address::from_mut_ptr(object));
    MemoryManager::<BDWGC>::register_finalizer(
        object,
        Box::new(move |object| {
            finalizer(object.as_address().to_mut_ptr(), cd.to_mut_ptr());
        }),
    );
}

#[no_mangle]
pub extern "C-unwind" fn GC_register_finalizer_ignore_self(
    obj: *mut libc::c_void,
    finalizer: extern "C" fn(*mut libc::c_void, *mut libc::c_void),
    cd: *mut libc::c_void,
    ofn: *mut extern "C" fn(*mut libc::c_void),
    ocd: *mut *mut libc::c_void,
) {
    let object = GC_base(obj);
    if object.is_null() {
        return;
    }
    let _ = ofn;
    let _ = ocd;
    let cd = Address::from_mut_ptr(cd);
    let object = VMKitObject::from_address(Address::from_mut_ptr(object));
    MemoryManager::<BDWGC>::register_finalizer(
        object,
        Box::new(move |object| {
            finalizer(object.as_address().to_mut_ptr(), cd.to_mut_ptr());
        }),
    );
}

#[no_mangle]
pub extern "C-unwind" fn GC_register_finalizer_no_order(
    obj: *mut libc::c_void,
    finalizer: extern "C" fn(*mut libc::c_void, *mut libc::c_void),
    cd: *mut libc::c_void,
    ofn: *mut extern "C" fn(*mut libc::c_void),
    ocd: *mut *mut libc::c_void,
) {
    let object = GC_base(obj);
    if object.is_null() {
        return;
    }
    let _ = ofn;
    let _ = ocd;
    let cd = Address::from_mut_ptr(cd);
    let object = VMKitObject::from_address(Address::from_mut_ptr(object));
    MemoryManager::<BDWGC>::register_finalizer(
        object,
        Box::new(move |object| {
            finalizer(object.as_address().to_mut_ptr(), cd.to_mut_ptr());
        }),
    );
}

#[no_mangle]
pub extern "C-unwind" fn GC_register_finalizer_unreachable(
    obj: *mut libc::c_void,
    finalizer: extern "C" fn(*mut libc::c_void, *mut libc::c_void),
    cd: *mut libc::c_void,
    ofn: *mut extern "C" fn(*mut libc::c_void),
    ocd: *mut *mut libc::c_void,
) {
    let object = GC_base(obj);
    if object.is_null() {
        return;
    }
    let _ = ofn;
    let _ = ocd;
    let cd = Address::from_mut_ptr(cd);
    let object = VMKitObject::from_address(Address::from_mut_ptr(object));
    MemoryManager::<BDWGC>::register_finalizer(
        object,
        Box::new(move |object| {
            finalizer(object.as_address().to_mut_ptr(), cd.to_mut_ptr());
        }),
    );
}

#[no_mangle]
pub extern "C-unwind" fn GC_collect_a_little() {
    GC_gcollect();
}

#[no_mangle]
pub static mut GC_no_dls: i32 = 0;
#[no_mangle]
pub static mut GC_java_finalization: i32 = 0;

#[no_mangle]
pub static mut GC_all_interior_pointers: i32 = 1;

#[no_mangle]
pub extern "C-unwind" fn GC_dlopen(path: *const libc::c_char, flags: i32) -> *mut libc::c_void {
    unsafe { libc::dlopen(path, flags) }
}

#[repr(C)]
pub struct GC_ms_entry {
    pub opaque: Address,
}

pub type GCMarkProc = extern "C-unwind" fn(
    addr: *mut usize,
    mark_stack_top: *mut GC_ms_entry,
    mark_stack_limit: *mut GC_ms_entry,
    env: usize,
) -> *mut GC_ms_entry;

#[no_mangle]
pub extern "C-unwind" fn GC_new_kind(
    fl: *mut *mut libc::c_void,
    descr: usize,
    adjust: libc::c_int,
    clear: libc::c_int,
) -> usize {
    let vm = BDWGC::get();
    let mut kinds = vm.obj_kinds.lock();
    let result = vm.n_kinds.load(Ordering::Relaxed);

    if result < MAXOBJKINDS {
        vm.n_kinds.fetch_add(1, Ordering::Relaxed);
        let kind = obj_kind {
            ok_freelist: fl,
            ok_reclaim_list: null_mut(),
            ok_descriptor: descr,
            ok_init: clear != 0,
            ok_relocate_descr: adjust != 0,
        };
        kinds[result] = Some(kind);
    } else {
        panic!("too many kinds");
    }

    result
}

#[no_mangle]
pub extern "C-unwind" fn GC_new_proc(proc: GCMarkProc) -> usize {
    let vm = BDWGC::get();
    let mut procs = vm.mark_procs.lock();
    let result = vm.n_procs.load(Ordering::Relaxed);
    if result < GC_MAX_MARK_PROCS {
        vm.n_procs.fetch_add(1, Ordering::Relaxed);
        procs[result] = Some(proc);
    } else {
        panic!("too many mark procedures");
    }

    result
}

#[no_mangle]
pub extern "C-unwind" fn GC_mark_and_push(
    obj: *mut u8,
    mark_stack_top: *mut libc::c_void,
    _mark_stack_limit: *mut libc::c_void,
    _src: *mut libc::c_void,
) -> *mut libc::c_void {
    // we fake mark_stack_top, this function does not push anything but invokes
    // an MMTK tracer
    let stack = Address::from_mut_ptr(mark_stack_top);
    unsafe {
        let x = stack.as_mut_ref::<MarkStackMeta>();
        if let Some(object) =
            mmtk::memory_manager::find_object_from_internal_pointer(Address::from_mut_ptr(obj), 128)
        {
            x.tracer.trace_object(object);
        }
    }

    mark_stack_top
}

#[no_mangle]
pub extern "C-unwind" fn GC_generic_malloc(size: usize, k: libc::c_int) -> *mut libc::c_void {
    let vtable = BDWGCMetadata {
        meta: ObjectSize::encode(size / BDWGC::MIN_ALIGNMENT)
            | ObjectKindField::encode(ObjectKind::WithDescr(k as _)),
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
pub extern "C-unwind" fn GC_generic_malloc_uncollectable(
    size: usize,
    k: libc::c_int,
) -> *mut libc::c_void {
    let vtable = BDWGCMetadata {
        meta: ObjectSize::encode(size / BDWGC::MIN_ALIGNMENT)
            | ObjectKindField::encode(ObjectKind::WithDescr(k as _)),
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
pub extern "C-unwind" fn GC_generic_malloc_ignore_off_page(
    size: usize,
    k: libc::c_int,
) -> *mut libc::c_void {
    let vtable = BDWGCMetadata {
        meta: ObjectSize::encode(size / BDWGC::MIN_ALIGNMENT)
            | ObjectKindField::encode(ObjectKind::WithDescr(k as _)),
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
pub extern "C-unwind" fn GC_expand_hp(_: usize) {}

#[no_mangle]
pub extern "C-unwind" fn GC_is_visible(p: *mut libc::c_void) -> *mut libc::c_void {
    p
}

unsafe fn gc_get_bit(bm: *const usize, i: usize) -> bool {
    bm.add(i / size_of::<usize>()).read() >> (i % size_of::<usize>()) & 1 != 0
}

#[no_mangle]
pub extern "C-unwind" fn GC_call_with_stack_base(
    fun: extern "C-unwind" fn(*mut GC_stack_base, *mut libc::c_void),
    arg: *mut libc::c_void,
) {
    let mut stack = GC_stack_base {
        mem_base: StackBounds::current_thread_stack_bounds()
            .origin()
            .to_mut_ptr(),
    };

    fun(&mut stack, arg);
}

#[no_mangle]
pub extern "C-unwind" fn GC_new_free_list() -> *mut *mut libc::c_void {
    null_mut()
}

const BITMAP_BITS: usize = usize::BITS as usize - GC_DS_TAG_BITS;

#[no_mangle]
pub unsafe extern "C-unwind" fn GC_make_descriptor(bm: *const usize, len: usize) -> usize {
    let mut last_set_bit = len as isize - 1;

    unsafe {
        while last_set_bit >= 0 && !gc_get_bit(bm, last_set_bit as _) {
            last_set_bit -= 1;
        }

        if last_set_bit < 0 {
            return 0;
        }
        let mut i = 0;

        while i < last_set_bit {
            if !gc_get_bit(bm, i as _) {
                break;
            }

            i += 1;
        }

        if i == last_set_bit {
            return (last_set_bit as usize + 1) | GC_DS_LENGTH;
        }

        if last_set_bit < BITMAP_BITS as isize {
            let mut _d = SIGNB;

            i = last_set_bit - 1;
            while i >= 0 {
                _d >>= 1;
                if gc_get_bit(bm, i as _) {
                    _d |= SIGNB;
                }
                i -= 1;
            }
        } else {
            return (last_set_bit as usize + 1) | GC_DS_LENGTH;
        }
    }
    0
}

#[no_mangle]
pub unsafe extern "C-unwind" fn GC_malloc_explicitly_typed(
    size: usize,
    descr: usize,
) -> *mut libc::c_void {
    let _ = descr;
    GC_malloc(size)
}

#[no_mangle]
pub unsafe extern "C-unwind" fn GC_malloc_explicitly_typed_ignore_off_page(
    size: usize,
    descr: usize,
) -> *mut libc::c_void {
    let _ = descr;
    GC_malloc_ignore_off_page(size)
}

#[no_mangle]
pub extern "C-unwind" fn GC_invoke_finalizers() -> usize {
    MemoryManager::<BDWGC>::run_finalizers()
}
