use mmtk::util::Address;
use mmtk::{util::options::PlanSelector, vm::slot::SimpleSlot, AllocationSemantics, MMTKBuilder};
use std::cell::RefCell;
use std::mem::offset_of;
use std::sync::Arc;
use std::sync::OnceLock;
use vmkit::threading::parked_scope;
use vmkit::{
    mm::{traits::Trace, MemoryManager},
    object_model::{
        metadata::{GCMetadata, TraceCallback},
        object::VMKitObject,
    },
    sync::Monitor,
    threading::{GCBlockAdapter, Thread, ThreadContext},
    VMKit, VirtualMachine,
};

#[repr(C)]
struct Node {
    left: NodeRef,
    right: NodeRef,
}

static METADATA: GCMetadata<BenchVM> = GCMetadata {
    trace: TraceCallback::TraceObject(|object, tracer| unsafe {
        let node = object.as_address().as_mut_ref::<Node>();
        node.left.0.trace_object(tracer);
        node.right.0.trace_object(tracer);
    }),
    instance_size: size_of::<Node>(),
    compute_size: None,
    alignment: 16,
};

struct BenchVM {
    vmkit: VMKit<Self>,
}

static VM: OnceLock<BenchVM> = OnceLock::new();

struct ThreadBenchContext;

impl ThreadContext<BenchVM> for ThreadBenchContext {
    fn new(_: bool) -> Self {
        Self
    }
    fn save_thread_state(&self) {}

    fn scan_roots(
        &self,
        _factory: impl mmtk::vm::RootsWorkFactory<<BenchVM as VirtualMachine>::Slot>,
    ) {
    }

    fn scan_conservative_roots(
        &self,
        _croots: &mut vmkit::mm::conservative_roots::ConservativeRoots,
    ) {
    }
}

impl VirtualMachine for BenchVM {
    type BlockAdapterList = (GCBlockAdapter, ());
    type Metadata = &'static GCMetadata<Self>;
    type Slot = SimpleSlot;
    type ThreadContext = ThreadBenchContext;
    fn get() -> &'static Self {
        VM.get().unwrap()
    }

    fn vmkit(&self) -> &VMKit<Self> {
        &self.vmkit
    }

    fn prepare_for_roots_re_scanning() {}

    fn notify_initial_thread_scan_complete(partial_scan: bool, tls: mmtk::util::VMWorkerThread) {
        let _ = partial_scan;
        let _ = tls;
    }

    fn forward_weak_refs(
        _worker: &mut mmtk::scheduler::GCWorker<vmkit::mm::MemoryManager<Self>>,
        _tracer_context: impl mmtk::vm::ObjectTracerContext<vmkit::mm::MemoryManager<Self>>,
    ) {
    }

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
}

#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct NodeRef(VMKitObject);

impl NodeRef {
    pub fn new(thread: &Thread<BenchVM>, left: NodeRef, right: NodeRef) -> Self {
        let node = MemoryManager::<BenchVM>::allocate(
            thread,
            size_of::<Node>(),
            16,
            &METADATA,
            AllocationSemantics::Default,
        );

        node.set_field_object::<BenchVM, false>(offset_of!(Node, left), left.0);
        node.set_field_object::<BenchVM, false>(offset_of!(Node, right), right.0);

        Self(node)
    }

    pub fn left(self) -> NodeRef {
        unsafe {
            let node = self.0.as_address().as_ref::<Node>();
            node.left
        }
    }

    pub fn right(self) -> NodeRef {
        unsafe {
            let node = self.0.as_address().as_ref::<Node>();
            node.right
        }
    }

    pub fn null() -> Self {
        Self(VMKitObject::NULL)
    }

    pub fn item_check(&self) -> usize {
        if self.left() == NodeRef::null() {
            1
        } else {
            1 + self.left().item_check() + self.right().item_check()
        }
    }

    pub fn leaf(thread: &Thread<BenchVM>) -> Self {
        Self::new(thread, NodeRef::null(), NodeRef::null())
    }
}

fn bottom_up_tree(thread: &Thread<BenchVM>, depth: usize) -> NodeRef {
    if thread.take_yieldpoint() != 0 {
        Thread::<BenchVM>::yieldpoint(0, Address::ZERO);
    }
    if depth > 0 {
        NodeRef::new(
            thread,
            bottom_up_tree(thread, depth - 1),
            bottom_up_tree(thread, depth - 1),
        )
    } else {
        NodeRef::leaf(thread)
    }
}

const MIN_DEPTH: usize = 4;

fn main() {
    env_logger::init();
    let nthreads = std::env::var("THREADS")
        .unwrap_or("4".to_string())
        .parse::<usize>()
        .unwrap();
    let mut builder = MMTKBuilder::new();
    builder.options.plan.set(PlanSelector::Immix);
    builder.options.threads.set(nthreads);
    builder
        .options
        .gc_trigger
        .set(mmtk::util::options::GCTriggerSelector::DynamicHeapSize(
            4 * 1024 * 1024 * 1024,
            16 * 1024 * 1024 * 1024,
        ));
    VM.set(BenchVM {
        vmkit: VMKit::new(&mut builder),
    })
    .unwrap_or_else(|_| panic!());

    Thread::<BenchVM>::main(ThreadBenchContext, || {
        let thread = Thread::<BenchVM>::current();
        let start = std::time::Instant::now();
        let n = std::env::var("DEPTH")
            .unwrap_or("18".to_string())
            .parse::<usize>()
            .unwrap();
        let max_depth = if n < MIN_DEPTH + 2 { MIN_DEPTH + 2 } else { n };

        let stretch_depth = max_depth + 1;

        println!("stretch tree of depth {stretch_depth}");

        let _ = bottom_up_tree(&thread, stretch_depth);
        let duration = start.elapsed();
        println!("time: {duration:?}");

        let results = Arc::new(Monitor::new(vec![
            RefCell::new(String::new());
            (max_depth - MIN_DEPTH) / 2 + 1
        ]));

        let mut handles = Vec::new();

        for d in (MIN_DEPTH..=max_depth).step_by(2) {
            let depth = d;

            let thread = Thread::<BenchVM>::for_mutator(ThreadBenchContext);
            let results = results.clone();
            let handle = thread.start(move || {
                let thread = Thread::<BenchVM>::current();
                let mut check = 0;

                let iterations = 1 << (max_depth - depth + MIN_DEPTH);
                for _ in 1..=iterations {
                    let tree_node = bottom_up_tree(&thread, depth);
                    check += tree_node.item_check();
                }

                *results.lock_with_handshake::<BenchVM>()[(depth - MIN_DEPTH) / 2].borrow_mut() =
                    format!("{iterations}\t trees of depth {depth}\t check: {check}");
            });
            handles.push(handle);
        }
        println!("created {} threads", handles.len());

        parked_scope::<(), BenchVM>(|| {
            while let Some(handle) = handles.pop() {
                handle.join().unwrap();
            }
        });

        for result in results.lock_with_handshake::<BenchVM>().iter() {
            println!("{}", result.borrow());
        }

        println!(
            "long lived tree of depth {max_depth}\t check: {}",
            bottom_up_tree(&thread, max_depth).item_check()
        );

        let duration = start.elapsed();
        println!("time: {duration:?}");
    });
}
