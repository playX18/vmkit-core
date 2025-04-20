//! Simple finalization mechanism implementation.
//!
//! Implements finalizers which are unordered and can revive objects, also
//! allows registering objects for destruction without finalization.
//!

use std::{cell::RefCell, panic::AssertUnwindSafe};

use mmtk::{
    scheduler::GCWorker,
    util::ObjectReference,
    vm::{ObjectTracer, ObjectTracerContext},
};

use crate::{mm::MemoryManager, sync::Monitor, VirtualMachine};

use super::object::VMKitObject;

pub struct Finalizer {
    pub object: VMKitObject,
    pub finalizer: FinalizerKind,
}

impl Finalizer {
    pub fn run(&mut self) {
        let finalizer = self.finalizer.take();
        match finalizer {
            FinalizerKind::Finalized => {}
            FinalizerKind::Unordered(callback) => {
                let object = self.object;
                callback(object);
            }
            FinalizerKind::Drop(callback) => {
                callback();
            }
        }
    }
}

pub enum FinalizerKind {
    Finalized,
    /// Unordered finalizer: finalizer revives the object and
    /// then executes the provided closure as a finalizer. Closure
    /// can keep object alive if it's required (but that's not recommended).
    Unordered(Box<dyn FnOnce(VMKitObject) + Send>),
    /// Drop finalizer: does not revive the object nor
    /// provides access to it. This can be used to close open FDs
    /// or free off heap data.
    Drop(Box<dyn FnOnce() + Send>),
}

impl FinalizerKind {
    pub fn take(&mut self) -> Self {
        std::mem::replace(self, FinalizerKind::Finalized)
    }

    pub fn is_finalized(&self) -> bool {
        matches!(self, FinalizerKind::Finalized)
    }
}

pub struct FinalizerProcessing;

pub static REGISTERED_FINALIZERS: Monitor<RefCell<Vec<Finalizer>>> =
    Monitor::new(RefCell::new(Vec::new()));
pub static PENDING_FINALIZERS: Monitor<RefCell<Vec<Finalizer>>> =
    Monitor::new(RefCell::new(Vec::new()));

impl FinalizerProcessing {
    pub fn process<VM: VirtualMachine>(
        tls: &mut GCWorker<MemoryManager<VM>>,
        tracer_context: impl ObjectTracerContext<MemoryManager<VM>>,
    ) -> bool {
        let vm = VM::get();
        assert!(
            vm.vmkit().mmtk.gc_in_progress(),
            "Attempt to process finalizers outside of GC"
        );

        let registered = REGISTERED_FINALIZERS.lock_no_handshake();
        let pending = PENDING_FINALIZERS.lock_no_handshake();

        let mut registered = registered.borrow_mut();
        let mut pending = pending.borrow_mut();
        let mut closure_required = false;
        tracer_context.with_tracer(tls, |tracer| {
            registered.retain_mut(|finalizer| {
                let Ok(object) = finalizer.object.try_into() else {
                    return false;
                };
                let object: ObjectReference = object;

                if object.is_reachable() {
                    let new_object = object.get_forwarded_object().unwrap_or(object);
                    finalizer.object = VMKitObject::from(new_object);
                    true
                } else {
                    let new_object = tracer.trace_object(object);
                    pending.push(Finalizer {
                        finalizer: std::mem::replace(
                            &mut finalizer.finalizer,
                            FinalizerKind::Finalized,
                        ),
                        object: VMKitObject::from(new_object),
                    });
                    closure_required = true;

                    false
                }
            });
        });
        closure_required
    }
}

impl<VM: VirtualMachine> MemoryManager<VM> {
    pub fn register_finalizer(object: VMKitObject, callback: Box<dyn FnOnce(VMKitObject) + Send>) {
        let finalizer = Finalizer {
            object,
            finalizer: FinalizerKind::Unordered(callback),
        };

        REGISTERED_FINALIZERS
            .lock_no_handshake()
            .borrow_mut()
            .push(finalizer);
    }

    pub fn run_finalizers() -> usize {
        let mut count = 0;
        while let Some(mut finalizer) = Self::get_finalized_object()
        {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| finalizer.run()));
            count += 1;
        }
        count
    }

    pub fn get_finalized_object() -> Option<Finalizer> {
        PENDING_FINALIZERS.lock_no_handshake().borrow_mut().pop()
    }
}
