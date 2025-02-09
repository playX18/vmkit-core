use std::marker::PhantomData;

use mmtk::vm::*;

use crate::{mm::MemoryManager, threading::Thread, VirtualMachine};


pub struct VMKitActivePlan<VM: VirtualMachine>(PhantomData<VM>);

impl<VM: VirtualMachine> ActivePlan<MemoryManager<VM>> for VMKitActivePlan<VM> {
    
    fn is_mutator(tls: mmtk::util::VMThread) -> bool {
        let x = Thread::<VM>::from_vm_thread(tls);
        Thread::<VM>::from_vm_thread(tls).active_mutator_context()
    }

    fn mutator(tls: mmtk::util::VMMutatorThread) -> &'static mut mmtk::Mutator<MemoryManager<VM>> {
        // SAFETY: `mutator()` is invoked by MMTk only when all threads are suspended for GC or 
        // MMTk is in allocation slowpath in *current* thread.
        // At this point no other thread can access this thread's context.
        unsafe { Thread::<VM>::from_vm_mutator_thread(tls).mutator_unchecked() }
    }

    fn mutators<'a>() -> Box<dyn Iterator<Item = &'a mut mmtk::Mutator<MemoryManager<VM>>> + 'a> {
        let vm = VM::get();
        Box::new(vm.vmkit().thread_manager()
            .threads()
            .filter(|t| t.active_mutator_context())
            .map(|t| 
                
                // SAFETY: `mutators()` is invoked by MMTk only when all threads are suspended for GC.
                // At this point no other thread can access this thread's context.
                unsafe { t.mutator_unchecked() }))
    }
    

    fn number_of_mutators() -> usize {
        let vm = VM::get();
        vm.vmkit().thread_manager()
            .threads()
            .filter(|t| t.active_mutator_context())
            .count()
    }

    fn vm_trace_object<Q: mmtk::ObjectQueue>(
            _queue: &mut Q,
            _object: mmtk::util::ObjectReference,
            _worker: &mut mmtk::scheduler::GCWorker<MemoryManager<VM>>,
        ) -> mmtk::util::ObjectReference {
        todo!()
    }
}
