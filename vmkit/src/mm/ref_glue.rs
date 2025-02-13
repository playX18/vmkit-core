#![allow(dead_code, unused_variables)]

use std::marker::PhantomData;

use mmtk::vm::{Finalizable, ReferenceGlue};

use crate::{object_model::object::VMKitObject, threading::Thread, VirtualMachine};

use super::MemoryManager;

pub struct VMKitReferenceGlue<VM: VirtualMachine>(PhantomData<VM>);

impl<VM: VirtualMachine> ReferenceGlue<MemoryManager<VM>> for VMKitReferenceGlue<VM> {
    type FinalizableType = Finalizer;

    fn clear_referent(new_reference: mmtk::util::ObjectReference) {
        VM::clear_referent(new_reference.into());
    }

    fn enqueue_references(
        references: &[mmtk::util::ObjectReference],
        tls: mmtk::util::VMWorkerThread,
    ) {
        VM::enqueue_references(
            references.iter().copied().map(VMKitObject::from),
            Thread::<VM>::from_vm_worker_thread(tls),
        );
    }

    fn get_referent(object: mmtk::util::ObjectReference) -> Option<mmtk::util::ObjectReference> {
        VM::get_referent(object.into()).try_into().ok()
    }

    fn set_referent(reff: mmtk::util::ObjectReference, referent: mmtk::util::ObjectReference) {
        VM::set_referent(reff.into(), referent.into());
    }
}

impl Finalizable for Finalizer {
    fn get_reference(&self) -> mmtk::util::ObjectReference {
        unsafe { self.object.as_object_unchecked() }
    }

    fn keep_alive<E: mmtk::scheduler::ProcessEdgesWork>(&mut self, trace: &mut E) {
        unsafe {
            let mmtk_obj = self.object.as_object_unchecked();
            let new = trace.trace_object(mmtk_obj);
            self.object = VMKitObject::from(new);
        }
    }

    fn set_reference(&mut self, object: mmtk::util::ObjectReference) {
        self.object = VMKitObject::from(object);
    }
}

pub struct Finalizer {
    pub object: VMKitObject,
    pub callback: Option<Box<dyn FnOnce(VMKitObject) + Send>>,
}

impl Finalizer {
    pub fn run(&mut self) {
        match self.callback.take() {
            Some(callback) => callback(self.object),
            None => {}
        }
    }
}

impl std::fmt::Debug for Finalizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#<finalizer {:x}>", self.object.as_address(),)
    }
}
