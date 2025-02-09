
use mmtk::{util::options::PlanSelector, vm::slot::SimpleSlot, AllocationSemantics, MMTKBuilder};
use std::mem::offset_of;
use std::sync::OnceLock;
use vmkit::{
    mm::{traits::Trace, MemoryManager},
    object_model::{
        metadata::{GCMetadata, TraceCallback},
        object::VMKitObject,
    },
    threading::{GCBlockAdapter, Thread, ThreadContext},
    VMKit, VirtualMachine,
};

const CONSERVATIVE_TRACE_NODE: bool = false;

#[repr(C)]
struct Node {
    left: VMKitObject,
    right: VMKitObject,
    i: usize,
    j: usize,
}

static METADATA: GCMetadata<BenchVM> = GCMetadata {
    trace: TraceCallback::TraceObject(|object, tracer| unsafe {
        let node = object.as_address().as_mut_ref::<Node>();
        node.left.trace_object(tracer);
        node.right.trace_object(tracer);
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

fn make_node(
    thread: &Thread<BenchVM>,
    left: VMKitObject,
    right: VMKitObject,
    i: usize,
    j: usize,
) -> VMKitObject {
    let node = MemoryManager::allocate(
        thread,
        size_of::<Node>(),
        16,
        &METADATA,
        AllocationSemantics::Default,
    );
   
    unsafe {
        node.set_field_object_no_write_barrier::<BenchVM, false>(offset_of!(Node, left), left);
        node.set_field_object_no_write_barrier::<BenchVM, false>(offset_of!(Node, right), right);
        node.set_field_usize::<BenchVM>(offset_of!(Node, i), i);
        node.set_field_usize::<BenchVM>(offset_of!(Node, j), j);
    }
    node
}

fn tree_size(i: usize) -> usize {
    (1 << (i + 1)) - 1
}

fn num_iters(stretch_tree_depth: usize, i: usize) -> usize {
    4 + tree_size(stretch_tree_depth) / tree_size(i)
}

fn populate(thread: &Thread<BenchVM>, depth: usize, this_node: VMKitObject) {
    let mut depth = depth;
    if depth <= 0 {
        return;
    }

    depth -= 1;
    this_node.set_field_object::<BenchVM, false>(
        offset_of!(Node, left),
        make_node(thread, VMKitObject::NULL, VMKitObject::NULL, 0, 0),
    );
    let left = this_node.get_field_object::<BenchVM, false>(offset_of!(Node, left));
    this_node.set_field_object::<BenchVM, false>(
        offset_of!(Node, right),
        make_node(thread, VMKitObject::NULL, VMKitObject::NULL, 0, 0),
    );

    

    populate(
        thread,
        depth,
        this_node.get_field_object::<BenchVM, false>(offset_of!(Node, left)),
    );
    populate(
        thread,
        depth,
        this_node.get_field_object::<BenchVM, false>(offset_of!(Node, right)),
    );
}

fn make_tree(thread: &Thread<BenchVM>, depth: usize) -> VMKitObject {
    if depth <= 0 {
        return make_node(thread, VMKitObject::NULL, VMKitObject::NULL, 0, 0);
    }

    let left = make_tree(thread, depth - 1);
    let right = make_tree(thread, depth - 1);
    make_node(thread, left, right, 0, 0)
}

fn time_construction(thread: &Thread<BenchVM>, stretch_tree_depth: usize, depth: usize) {
    let i_num_iters = num_iters(stretch_tree_depth, depth);
    println!("creating {} trees of depth {}", i_num_iters, depth);
    let start = std::time::Instant::now();
    
    let mut i = 0;
    while i < i_num_iters {
        let temp_tree = make_node(thread, VMKitObject::NULL, VMKitObject::NULL, 0, 0);
        populate(thread, depth, temp_tree);
        i += 1;
    }

    let finish = std::time::Instant::now();
    println!("\tTop down construction took: {:04}ms", finish.duration_since(start).as_micros() as f64 / 1000.0);
    

    let duration = start.elapsed();
    println!("time_construction: {:?}", duration);
}

fn main() {
    env_logger::init();
    let mut options = MMTKBuilder::new();
    options.options.plan.set(PlanSelector::StickyImmix);
    options.options.gc_trigger.set(mmtk::util::options::GCTriggerSelector::DynamicHeapSize(64*1024*1024, 8*1024*1024*1024));
    let vm = BenchVM {
        vmkit: VMKit::new(options)
    };

    VM.set(vm).unwrap_or_else(|_| panic!("Failed to set VM"));

    Thread::<BenchVM>::main(ThreadBenchContext, || {
        let tls= Thread::<BenchVM>::current();
        
        let depth = std::env::var("DEPTH").unwrap_or("18".to_string()).parse::<usize>().unwrap();
        let long_lived_tree_depth = depth;
        
        let stretch_tree_depth = depth + 1;
        
        println!("stretching memory with tree of depth: {}", stretch_tree_depth);
        let start = std::time::Instant::now();
        make_tree(tls, stretch_tree_depth as _);

        println!("creating long-lived tree of depth: {}", long_lived_tree_depth);
        let long_lived_tree = make_node(tls, VMKitObject::NULL, VMKitObject::NULL, 0, 0);
        populate(tls, long_lived_tree_depth as _, long_lived_tree);


        let mut d = 4;

        while d <= depth {
            time_construction(tls, stretch_tree_depth, d);
            d += 2;
        }

        let finish = std::time::Instant::now();
        println!("total execution time: {:04}ms", finish.duration_since(start).as_micros() as f64 / 1000.0);
        
    });
}