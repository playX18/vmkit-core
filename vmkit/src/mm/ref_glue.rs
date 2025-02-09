#![allow(dead_code, unused_variables)]

use std::marker::PhantomData;

use mmtk::vm::{Finalizable, ReferenceGlue};

use crate::{object_model::object::VMKitObject, VirtualMachine};

use super::MemoryManager;


pub struct VMKitReferenceGlue<VM: VirtualMachine>(PhantomData<VM>);



impl<VM: VirtualMachine> ReferenceGlue<MemoryManager<VM>> for VMKitReferenceGlue<VM> {
    type FinalizableType = VMKitObject;
    
    fn clear_referent(new_reference: mmtk::util::ObjectReference) {
        
    }

    fn enqueue_references(references: &[mmtk::util::ObjectReference], tls: mmtk::util::VMWorkerThread) {
        
    }

    fn get_referent(object: mmtk::util::ObjectReference) -> Option<mmtk::util::ObjectReference> {
        todo!()
    }
    
    fn set_referent(reff: mmtk::util::ObjectReference, referent: mmtk::util::ObjectReference) {
        todo!()
    }
    
}

impl Finalizable for VMKitObject {
    fn get_reference(&self) -> mmtk::util::ObjectReference {
        todo!()
    }

    fn keep_alive<E: mmtk::scheduler::ProcessEdgesWork>(&mut self, trace: &mut E) {
        
    }

    fn set_reference(&mut self, object: mmtk::util::ObjectReference) {
        
    }
}