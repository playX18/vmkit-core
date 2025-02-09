use std::marker::PhantomData;

use crate::{
    mm::MemoryManager,
    object_model::{
        metadata::{Metadata, Trace},
        object::VMKitObject,
    },
    options::OPTIONS,
    threading::{Thread, ThreadContext},
    VirtualMachine,
};
use mmtk::{
    vm::{slot::Slot, ObjectTracer, Scanning, SlotVisitor},
    MutatorContext,
};

use super::{conservative_roots::ConservativeRoots, traits::ToSlot};

pub struct VMKitScanning<VM: VirtualMachine>(PhantomData<VM>);

impl<VM: VirtualMachine> Scanning<MemoryManager<VM>> for VMKitScanning<VM> {
    fn forward_weak_refs(
        _worker: &mut mmtk::scheduler::GCWorker<MemoryManager<VM>>,
        _tracer_context: impl mmtk::vm::ObjectTracerContext<MemoryManager<VM>>,
    ) {
        VM::forward_weak_refs(_worker, _tracer_context);
    }

    fn notify_initial_thread_scan_complete(partial_scan: bool, tls: mmtk::util::VMWorkerThread) {
        VM::notify_initial_thread_scan_complete(partial_scan, tls);
    }

    fn prepare_for_roots_re_scanning() {
        VM::prepare_for_roots_re_scanning();
    }

    fn process_weak_refs(
        _worker: &mut mmtk::scheduler::GCWorker<MemoryManager<VM>>,
        _tracer_context: impl mmtk::vm::ObjectTracerContext<MemoryManager<VM>>,
    ) -> bool {
        VM::process_weak_refs(_worker, _tracer_context)
    }

    fn scan_object<
        SV: mmtk::vm::SlotVisitor<<MemoryManager<VM> as mmtk::vm::VMBinding>::VMSlot>,
    >(
        _tls: mmtk::util::VMWorkerThread,
        object: mmtk::util::ObjectReference,
        slot_visitor: &mut SV,
    ) {
        let object = VMKitObject::from(object);
        let metadata = object.header::<VM>().metadata();
        if metadata.is_object() {
            slot_visitor.visit_slot(metadata.to_slot().expect("Object is not a slot"));
        }
        let gc_metadata = metadata.gc_metadata();
        let trace = &gc_metadata.trace;
        match trace {
            Trace::ScanSlots(fun) => {
                fun(object, slot_visitor);
            }
            Trace::None => (),
            Trace::TraceObject(_) => {
                unreachable!("TraceObject is not supported for scanning");
            }
        }
    }

    fn scan_object_and_trace_edges<OT: mmtk::vm::ObjectTracer>(
        _tls: mmtk::util::VMWorkerThread,
        object: mmtk::util::ObjectReference,
        object_tracer: &mut OT,
    ) {
        let object = VMKitObject::from(object);
        let metadata = object.header::<VM>().metadata();
        let gc_metadata = metadata.gc_metadata();
        let trace = &gc_metadata.trace;
        match trace {
            Trace::ScanSlots(fun) => {
                // wrap object tracer in a trait that implements SlotVisitor
                // but actually traces the object directly.
                let mut visitor = TraceSlotVisitor::<VM, OT> {
                    ot: object_tracer,
                    marker: PhantomData,
                };
                fun(object, &mut visitor);
            }
            Trace::None => (),
            Trace::TraceObject(fun) => {
                fun(object, object_tracer);
            }
        }
    }

    fn scan_roots_in_mutator_thread(
        _tls: mmtk::util::VMWorkerThread,
        mutator: &'static mut mmtk::Mutator<MemoryManager<VM>>,
        mut factory: impl mmtk::vm::RootsWorkFactory<VM::Slot>,
    ) {
        let tls = Thread::<VM>::from_vm_mutator_thread(mutator.get_tls());
        tls.context.scan_roots(factory.clone());

        if OPTIONS.conservative_stacks {
            let mut croots = ConservativeRoots::new();
            let bounds = *tls.stack_bounds();
            unsafe { croots.add_span(bounds.origin(), bounds.end()) };
            tls.context.scan_conservative_roots(&mut croots);
            croots.add_to_factory(&mut factory);
        }
    }

    fn scan_vm_specific_roots(
        tls: mmtk::util::VMWorkerThread,
        factory: impl mmtk::vm::RootsWorkFactory<VM::Slot>,
    ) {
        VM::scan_vm_specific_roots(tls, factory);
    }

    fn support_slot_enqueuing(
        _tls: mmtk::util::VMWorkerThread,
        object: mmtk::util::ObjectReference,
    ) -> bool {
        let object = VMKitObject::from(object);
        let metadata = object.header::<VM>().metadata();
        matches!(metadata.gc_metadata().trace, Trace::ScanSlots(_))
            && (!metadata.is_object() || metadata.to_slot().is_some())
    }

    fn supports_return_barrier() -> bool {
        false
    }
}

struct TraceSlotVisitor<'a, VM: VirtualMachine, OT: ObjectTracer> {
    ot: &'a mut OT,
    marker: PhantomData<VM>,
}

impl<'a, VM: VirtualMachine, OT: ObjectTracer> SlotVisitor<VM::Slot>
    for TraceSlotVisitor<'a, VM, OT>
{
    fn visit_slot(&mut self, slot: VM::Slot) {
        let value = slot.load();
        match value {
            Some(object) => {
                let object = self.ot.trace_object(object);
                slot.store(object);
            }

            None => (),
        }
    }
}
