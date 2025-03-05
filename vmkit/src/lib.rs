use std::{
    marker::PhantomData,
    sync::atomic::{AtomicBool, AtomicUsize},
};

use mm::{aslr::aslr_vm_layout, traits::SlotExtra, MemoryManager};
use mmtk::{vm::slot::MemorySlice, MMTKBuilder, MMTK};
use object_model::object::VMKitObject;
use threading::{initialize_threading, Thread, ThreadManager};

pub mod machine_context;
pub mod mm;
pub mod object_model;
pub mod options;
pub mod platform;
pub mod sync;
pub mod threading;
pub mod macros;
#[cfg(feature = "uncooperative")]
pub mod bdwgc_shim;

pub use mmtk;

pub trait VirtualMachine: Sized + 'static + Send + Sync {
    type ThreadContext: threading::ThreadContext<Self>;
    type BlockAdapterList: threading::BlockAdapterList<Self>;
    type Metadata: object_model::metadata::Metadata<Self>;
    type Slot: SlotExtra;
    type MemorySlice: MemorySlice<SlotType = Self::Slot>;


    /// Should we always trace objects? If `true` then `support_slot_enqueing` will always return 
    /// false to MMTk and we will always work through `ObjectTracer` to trace objects.
    const ALWAYS_TRACE: bool = false;

    /*#[cfg(feature = "address_based_hashing")]
    const HASH_STATE_SPEC: VMLocalHashStateSpec = VMLocalHashStateSpec::in_header(61);*/
    /// 1-word local metadata for spaces that may copy objects.
    /// This metadata has to be stored in the header.
    /// This metadata can be defined at a position within the object payload.
    /// As a forwarding pointer is only stored in dead objects which is not
    /// accessible by the language, it is okay that store a forwarding pointer overwrites object payload
    ///
    #[cfg(feature = "vmside_forwarding")]
    const LOCAL_FORWARDING_POINTER_SPEC: mmtk::vm::VMLocalForwardingPointerSpec;
    /// 2-bit local metadata for spaces that store a forwarding state for objects.
    /// If this spec is defined in the header, it can be defined with a position of the lowest 2 bits in the forwarding pointer.
    /// If this spec is defined on the side it must be defined after the [`MARK_BIT_SPEC`](crate::object_model::MARK_BIT_SPEC).
    #[cfg(feature = "vmside_forwarding")]
    const LOCAL_FORWARDING_BITS_SPEC: mmtk::vm::VMLocalForwardingBitsSpec;

    const ALIGNMENT_VALUE: u32 = 0xdead_beef;
    const MAX_ALIGNMENT: usize = 32;
    const MIN_ALIGNMENT: usize = 8;

    /// Does this VM use conservative tracing? If `true` then VM can
    /// query VO-bit (valid-object bit) to check if an object is live
    /// during tracing work.
    ///
    /// Note that this is distinct from conservative stack scanning. When
    /// collecting roots VO-bits are always available.
    ///
    /// Read more: [ObjectModel::NEED_VO_BITS_DURING_TRACING](mmtk::vm::ObjectModel::NEED_VO_BITS_DURING_TRACING).
    ///
    /// # Note
    ///
    /// - [`InternalPointer`](mm::conservative_roots::InternalPointer) can only be used when this is `true`.
    #[cfg(feature = "cooperative")]
    const CONSERVATIVE_TRACING: bool = false;

    /// Get currently active VM instance.
    ///
    /// # Notes
    ///
    /// At the moment we assume only one active VM per process. This can be changed in the future once MMTk supports
    /// instances. In that case this function can return active VM for current thread instead of one global instance.
    fn get() -> &'static Self;

    fn vmkit(&self) -> &VMKit<Self>;

    /// Prepare for another round of root scanning in the same GC.
    ///
    /// For details: [Scanning::prepare_for_roots_re_scanning](mmtk::vm::Scanning::prepare_for_roots_re_scanning)
    fn prepare_for_roots_re_scanning();

    /// MMTk calls this method at the first time during a collection that thread's stacks have been scanned. This can be used (for example) to clean up obsolete compiled methods that are no longer being executed.
    fn notify_initial_thread_scan_complete(partial_scan: bool, tls: mmtk::util::VMWorkerThread);
    /// Process weak references.
    ///
    /// This function is called after a transitive closure is completed.
    ///
    /// For details: [Scanning::process_weak_refs](mmtk::vm::Scanning::process_weak_refs)
    fn process_weak_refs(
        _worker: &mut mmtk::scheduler::GCWorker<MemoryManager<Self>>,
        _tracer_context: impl mmtk::vm::ObjectTracerContext<MemoryManager<Self>>,
    ) -> bool {
        false
    }

    fn forward_weak_refs(
        _worker: &mut mmtk::scheduler::GCWorker<MemoryManager<Self>>,
        _tracer_context: impl mmtk::vm::ObjectTracerContext<MemoryManager<Self>>,
    );
    /// Scan one mutator for stack roots.
    ///
    /// For details: [Scanning::scan_roots_in_mutator_thread](mmtk::vm::Scanning::scan_roots_in_mutator_thread)
    fn scan_roots_in_mutator_thread(
        tls: mmtk::util::VMWorkerThread,
        mutator: &'static mut mmtk::Mutator<MemoryManager<Self>>,
        factory: impl mmtk::vm::RootsWorkFactory<<MemoryManager<Self> as mmtk::vm::VMBinding>::VMSlot>,
    );

    /// Scan VM-specific roots.
    ///
    /// For details: [Scanning::scan_vm_specific_roots](mmtk::vm::Scanning::scan_vm_specific_roots)
    fn scan_vm_specific_roots(
        tls: mmtk::util::VMWorkerThread,
        factory: impl mmtk::vm::RootsWorkFactory<<MemoryManager<Self> as mmtk::vm::VMBinding>::VMSlot>,
    );

    /// A hook for the VM to do work after forwarding objects.
    fn post_forwarding(tls: mmtk::util::VMWorkerThread) {
        let _ = tls;
    }

    fn schedule_finalization(tls: mmtk::util::VMWorkerThread) {
        let _ = tls;
    }

    fn vm_live_bytes() -> usize {
        0
    }

    fn out_of_memory(tls: mmtk::util::VMThread, err_kind: mmtk::util::alloc::AllocationError) {
        let _ = tls;
        let _ = err_kind;
        eprintln!("Out of memory: {:?}", err_kind);
        std::process::exit(1);
    }

    /// Weak and soft references always clear the referent before enqueueing.
    fn clear_referent(new_reference: VMKitObject) {
        let _ = new_reference;
        unimplemented!()
    }

    /// Get the referent from a weak reference object.
    fn get_referent(object: VMKitObject) -> VMKitObject {
        let _ = object;
        unimplemented!()
    }

    /// Set the referent in a weak reference object.
    fn set_referent(reff: VMKitObject, referent: VMKitObject) {
        let _ = reff;
        let _ = referent;
        unimplemented!()
    }

    /// For weak reference types, if the referent is cleared during GC, the reference
    /// will be added to a queue, and MMTk will call this method to inform
    /// the VM about the changes for those references. This method is used
    /// to implement Java's ReferenceQueue.
    /// Note that this method is called for each type of weak references during GC, and
    /// the references slice will be cleared after this call is returned. That means
    /// MMTk will no longer keep these references alive once this method is returned.
    fn enqueue_references(references: impl Iterator<Item = VMKitObject>, tls: &Thread<Self>) {
        let _ = references;
        let _ = tls;
        unimplemented!()
    }

    /// Compute the hashcode of an object. When feature `address_based_hashing` is enabled,
    /// this function is ignored. Otherwise VMKit calls into this function to compute hashcode of an object.
    ///
    /// In case VM uses moving plans it's strongly advised to *not* compute hashcode based on address
    /// as the object's address may change during GC, instead store hashcode as a field or use some bits
    /// in header to store the hashcode. This function must be fast as it's called for each `VMKitObject::hashcode()`
    /// invocation.
    fn compute_hashcode(object: VMKitObject) -> usize {
        let _ = object;
        unimplemented!(
            "VM currently does not support hashcode computation, override this method to do so"
        );
    }
}

pub struct VMKit<VM: VirtualMachine> {
    thread_manager: ThreadManager<VM>,
    pub mmtk: MMTK<MemoryManager<VM>>,
    pub(crate) collector_started: AtomicBool,
    marker: PhantomData<VM>,
    gc_disabled_depth: AtomicUsize,
}

impl<VM: VirtualMachine> VMKit<VM> {
    pub fn new(builder: &mut MMTKBuilder) -> Self {
        initialize_threading::<VM>();
        let vm_layout = aslr_vm_layout(&mut builder.options);
        builder.set_vm_layout(vm_layout);
        VMKit {
            mmtk: builder.build(),
            marker: PhantomData,
            collector_started: AtomicBool::new(false),
            thread_manager: ThreadManager::new(),
            gc_disabled_depth: AtomicUsize::new(0),
        }
    }

    pub(crate) fn are_collector_threads_spawned(&self) -> bool {
        self.collector_started.load(atomic::Ordering::Relaxed)
    }

    pub fn thread_manager(&self) -> &ThreadManager<VM> {
        &self.thread_manager
    }
}

#[cfg(feature="derive")]
pub use vmkit_proc::GCMetadata;
pub mod prelude {
    #[cfg(feature="derive")]
    pub use super::GCMetadata;
    pub use super::mm::traits::*;
    pub use super::object_model::object::*;
    pub use super::object_model::metadata::*;
    pub use mmtk::vm::ObjectTracer;
    pub use mmtk::vm::SlotVisitor;
}