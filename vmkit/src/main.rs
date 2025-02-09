use easy_bitfield::{FromBitfield, ToBitfield};
use mmtk::{util::options::PlanSelector, vm::slot::SimpleSlot, AllocationSemantics, MMTKBuilder};
use std::sync::OnceLock;
use vmkit::{
    mm::{traits::ToSlot, MemoryManager},
    object_model::metadata::{GCMetadata, Metadata, Trace},
    threading::{GCBlockAdapter, Thread, ThreadContext},
    VMKit, VirtualMachine,
};

struct TestContext;

impl ThreadContext<VM> for TestContext {
    fn save_thread_state(&self) {}

    fn scan_roots(&self, _factory: impl mmtk::vm::RootsWorkFactory<<VM as VirtualMachine>::Slot>) {}

    fn scan_conservative_roots(
        &self,
        croots: &mut vmkit::mm::conservative_roots::ConservativeRoots,
    ) {
        let _ = croots;
    }

    fn new(_collector_context: bool) -> Self {
        Self
    }
}
static VM_STORAGE: OnceLock<VM> = OnceLock::new();
impl VirtualMachine for VM {
    const MAX_ALIGNMENT: usize = 16;
    const MIN_ALIGNMENT: usize = 16;
    type ThreadContext = TestContext;
    type BlockAdapterList = (GCBlockAdapter, ());
    type Metadata = &'static GCMetadata<VM>;
    type Slot = SimpleSlot;

    fn get() -> &'static Self {
        VM_STORAGE.get().unwrap()
    }

    fn vmkit(&self) -> &VMKit<Self> {
        &self.vmkit
    }

    fn prepare_for_roots_re_scanning() {}

    fn notify_initial_thread_scan_complete(_partial_scan: bool, _tls: mmtk::util::VMWorkerThread) {}

    fn scan_roots_in_mutator_thread(
        _tls: mmtk::util::VMWorkerThread,
        _mutator: &'static mut mmtk::Mutator<vmkit::mm::MemoryManager<Self>>,
        _factory: impl mmtk::vm::RootsWorkFactory<
            <vmkit::mm::MemoryManager<Self> as mmtk::vm::VMBinding>::VMSlot,
        >,
    ) {
    }

    fn scan_vm_specific_roots(
        _tls: mmtk::util::VMWorkerThread,
        _factory: impl mmtk::vm::RootsWorkFactory<
            <vmkit::mm::MemoryManager<Self> as mmtk::vm::VMBinding>::VMSlot,
        >,
    ) {
    }

    fn forward_weak_refs(
        _worker: &mut mmtk::scheduler::GCWorker<vmkit::mm::MemoryManager<Self>>,
        _tracer_context: impl mmtk::vm::ObjectTracerContext<vmkit::mm::MemoryManager<Self>>,
    ) {
        todo!()
    }
}

struct VM {
    vmkit: VMKit<Self>,
}

static METADATA: GCMetadata<VM> = GCMetadata {
    instance_size: 48,
    compute_size: None,
    trace: Trace::TraceObject(|object, _tracer| {
        println!("tracing {}", object.as_address());
    }),
    alignment: 16,
};

struct FooMeta;

impl Metadata<VM> for FooMeta {
    const METADATA_BIT_SIZE: usize = 56;
    fn gc_metadata(&self) -> &'static GCMetadata<VM> {
        &METADATA
    }

    fn is_object(&self) -> bool {
        false
    }

    fn from_object_reference(_reference: mmtk::util::ObjectReference) -> Self {
        unreachable!()
    }

    fn to_object_reference(&self) -> Option<mmtk::util::ObjectReference> {
        unreachable!()
    }
}

impl ToSlot<SimpleSlot> for FooMeta {
    fn to_slot(&self) -> Option<SimpleSlot> {
        None
    }
}

impl FromBitfield<u64> for FooMeta {
    fn from_bitfield(_bitfield: u64) -> Self {
        FooMeta
    }

    fn from_i64(_value: i64) -> Self {
        FooMeta
    }
}

impl ToBitfield<u64> for FooMeta {
    fn to_bitfield(self) -> u64 {
        0
    }

    fn one() -> Self {
        FooMeta
    }

    fn zero() -> Self {
        FooMeta
    }
}

extern "C-unwind" fn handler(signum: libc::c_int) {
    println!("signal {signum}");
    println!("backtrace:\n{}", std::backtrace::Backtrace::force_capture());
    std::process::exit(1);
}

fn main() {
    unsafe {
        libc::signal(libc::SIGSEGV, handler as usize);
    }

    let mut mmtk = MMTKBuilder::new();
    mmtk.options.plan.set(PlanSelector::Immix);
    mmtk.options.threads.set(1);
    VM_STORAGE.get_or_init(|| VM {
        vmkit: VMKit::new(mmtk),
    });

    Thread::<VM>::main(TestContext, || {
        let tls = Thread::<VM>::current();
        let my_obj = MemoryManager::allocate(tls, 48, 16, &METADATA, AllocationSemantics::Default);

        println!("Allocated object at {}", my_obj.as_address());

        MemoryManager::<VM>::request_gc();

        println!("object {} at {:p}", my_obj.as_address(), &my_obj);
    });
}
