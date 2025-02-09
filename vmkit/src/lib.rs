use std::{marker::PhantomData, sync::atomic::AtomicBool};

use mm::{aslr::aslr_vm_layout, traits::SlotExtra, MemoryManager};
use mmtk::{ MMTKBuilder, MMTK};
use threading::ThreadManager;

pub mod mm;
pub mod object_model;
pub mod options;
pub mod sync;
pub mod threading;

pub trait VirtualMachine: Sized + 'static + Send + Sync {
    type ThreadContext: threading::ThreadContext<Self>;
    type BlockAdapterList: threading::BlockAdapterList<Self>;
    type Metadata: object_model::metadata::Metadata<Self>;
    type Slot: SlotExtra;

    const MAX_ALIGNMENT: usize = 16;
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
}

pub struct VMKit<VM: VirtualMachine> {
    thread_manager: ThreadManager<VM>,
    pub mmtk: MMTK<MemoryManager<VM>>,
    pub(crate) collector_started: AtomicBool,
    marker: PhantomData<VM>,
}

impl<VM: VirtualMachine> VMKit<VM> {
    pub fn new(mut builder: MMTKBuilder) -> Self {
        let vm_layout = aslr_vm_layout(&mut builder.options);
        builder.set_vm_layout(vm_layout);
        VMKit {
            mmtk: builder.build(),
            marker: PhantomData,
            collector_started: AtomicBool::new(false),
            thread_manager: ThreadManager::new(),
        }
    }

    pub(crate) fn are_collector_threads_spawned(&self) -> bool {
        self.collector_started.load(atomic::Ordering::Relaxed)
    }

    pub fn thread_manager(&self) -> &ThreadManager<VM> {
        &self.thread_manager
    }
}
