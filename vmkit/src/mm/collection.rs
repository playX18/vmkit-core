use std::marker::PhantomData;

use crate::{
    mm::MemoryManager,
    threading::{GCBlockAdapter, Thread},
    VirtualMachine,
};
use mmtk::{
    util::VMWorkerThread,
    vm::{Collection, GCThreadContext},
};

pub struct VMKitCollection<VM: VirtualMachine>(PhantomData<VM>);

impl<VM: VirtualMachine> Collection<MemoryManager<VM>> for VMKitCollection<VM> {
    fn stop_all_mutators<F>(_tls: mmtk::util::VMWorkerThread, mut mutator_visitor: F)
    where
        F: FnMut(&'static mut mmtk::Mutator<MemoryManager<VM>>),
    {
        let vmkit = VM::get().vmkit();

        let mutators = vmkit.thread_manager().block_all_mutators_for_gc();

        for mutator in mutators {
            unsafe {    
                // reset TLAb if there's any.
                MemoryManager::flush_tlab(&mutator);
                mutator_visitor(mutator.mutator_unchecked());
            }
        }
    }

    fn block_for_gc(tls: mmtk::util::VMMutatorThread) {
        let tls = Thread::<VM>::from_vm_mutator_thread(tls);
        tls.block_unchecked::<GCBlockAdapter>(true);
    }

    fn resume_mutators(_tls: mmtk::util::VMWorkerThread) {
        let vmkit = VM::get().vmkit();
        vmkit.thread_manager().unblock_all_mutators_for_gc();
        VM::notify_stw_complete();
    }

    fn create_gc_trigger() -> Box<dyn mmtk::util::heap::GCTriggerPolicy<MemoryManager<VM>>> {
        todo!()
    }

    fn is_collection_enabled() -> bool {
        true
    }

    fn out_of_memory(_tls: mmtk::util::VMThread, _err_kind: mmtk::util::alloc::AllocationError) {
        todo!()
    }

    fn post_forwarding(_tls: mmtk::util::VMWorkerThread) {
        VM::post_forwarding(_tls);
    }

    fn schedule_finalization(_tls: mmtk::util::VMWorkerThread) {
        
    }

    fn spawn_gc_thread(
        _tls: mmtk::util::VMThread,
        ctx: mmtk::vm::GCThreadContext<MemoryManager<VM>>,
    ) {
        let thread = Thread::<VM>::for_collector();

        match ctx {
            GCThreadContext::Worker(worker_ctx) => {
                thread.start(move || {
                    let vm = VM::get();
                    let tls = Thread::<VM>::current().to_vm_thread();
                    mmtk::memory_manager::start_worker(
                        &vm.vmkit().mmtk,
                        VMWorkerThread(tls),
                        worker_ctx,
                    );
                });
            }
        }
    }

    fn vm_live_bytes() -> usize {
        VM::vm_live_bytes()
    }
}
