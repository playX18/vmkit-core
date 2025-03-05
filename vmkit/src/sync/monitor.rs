
use std::{
    mem::ManuallyDrop,
    num::NonZeroU64,
    ops::Deref,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};

use parking_lot::{Condvar, Mutex, MutexGuard, WaitTimeoutResult};

use crate::{
    threading::{parked_scope, Thread, ThreadContext},
    VirtualMachine,
};

fn get_thread_id() -> NonZeroU64 {
    thread_local! {
        static KEY: u64 = 0;
    }
    KEY.with(|x| {
        NonZeroU64::new(x as *const _ as u64).expect("thread-local variable address is null")
    })
}

/// Implementation of a heavy lock and condition variable implemented using
/// the primitives available from the `parking_lot`.  Currently we use
/// a `Mutex` and `Condvar`.
/// <p>
/// It is perfectly safe to use this throughout the VM for locking.  It is
/// meant to provide roughly the same functionality as ReentrantMutex combined with Condvar,
/// except:
/// <ul>
/// <li>This struct provides a faster slow path than ReentrantMutex.</li>
/// <li>This struct provides a slower fast path than ReentrantMutex.</li>
/// <li>This struct will work in the inner guts of the VM runtime because
///     it gives you the ability to lock and unlock, as well as wait and
///     notify, without using any other VM runtime functionality.</li>
/// <li>This struct allows you to optionally block without letting the thread
///     system know that you are blocked.  The benefit is that you can
///     perform synchronization without depending on VM thread subsystem functionality.
///     However, most of the time, you should use the methods that inform
///     the thread system that you are blocking.  Methods that have the
///     `with_handshake` suffix will inform the thread system if you are blocked,
///     while methods that do not have the suffix will either not block
///     (as is the case with `unlock()` and `broadcast()`)
///     or will block without letting anyone know (like `lock_no_handshake()`
///     and `wait_no_handshake()`). Not letting the threading
///     system know that you are blocked may cause things like GC to stall
///     until you unblock.</li>
/// <li>This struct does not provide mutable access to the protected data as it is unsound,
///     instead use `RefCell` to mutate the protected data.</li>
/// </ul>
pub struct Monitor<T> {
    mutex: Mutex<T>,
    cvar: Condvar,
    rec_count: AtomicUsize,
    holder: AtomicU64,
}

impl<T> Monitor<T> {
    pub const fn new(value: T) -> Self {
        Self {
            mutex: Mutex::new(value),
            cvar: Condvar::new(),
            rec_count: AtomicUsize::new(0),
            holder: AtomicU64::new(0),
        }
    }

    pub fn lock_no_handshake(&self) -> MonitorGuard<T> {
        let my_slot = get_thread_id().get();
        let guard = if self.holder.load(Ordering::Relaxed) != my_slot {
            let guard = self.mutex.lock();
            self.holder.store(my_slot, Ordering::Release);
            MonitorGuard {
                monitor: self,
                guard: ManuallyDrop::new(guard),
            }
        } else {
            MonitorGuard {
                monitor: self,
                guard: unsafe { ManuallyDrop::new(self.mutex.make_guard_unchecked()) },
            }
        };
        self.rec_count.fetch_add(1, Ordering::Relaxed);
        guard
    }

    pub fn lock_with_handshake<VM: VirtualMachine>(&self) -> MonitorGuard<T> {
        let my_slot = get_thread_id().get();
        let guard = if my_slot != self.holder.load(Ordering::Relaxed) {
            let guard = self.lock_with_handshake_no_rec::<VM>();
            self.holder.store(my_slot, Ordering::Release);
            guard
        } else {
            MonitorGuard {
                monitor: self,
                guard: unsafe { ManuallyDrop::new(self.mutex.make_guard_unchecked()) },
            }
        };
        self.rec_count.fetch_add(1, Ordering::Relaxed);
        guard
    }

    fn lock_with_handshake_no_rec<VM: VirtualMachine>(&self) -> MonitorGuard<'_, T> {
        let tls = Thread::<VM>::current();
        tls.context.save_thread_state();

        let mutex_guard = loop {
            Thread::<VM>::enter_native();
            let guard = self.mutex.lock();
            if Thread::<VM>::attempt_leave_native_no_block() {
                break guard;
            } else {
                drop(guard);
                Thread::<VM>::leave_native();
            }
        };

        MonitorGuard {
            monitor: self,
            guard: ManuallyDrop::new(mutex_guard),
        }
    }

    pub fn notify(&self) {
        self.cvar.notify_one();
    }

    pub fn notify_all(&self) {
        self.cvar.notify_all();
    }

    pub unsafe fn relock_with_handshake<VM: VirtualMachine>(
        &self,
        rec_count: usize,
    ) -> MonitorGuard<'_, T> {
        let thread = Thread::<VM>::current();
        thread.context.save_thread_state();
        let guard = loop {
            Thread::<VM>::enter_native();
            let lock = self.mutex.lock();
            if Thread::<VM>::attempt_leave_native_no_block() {
                break lock;
            } else {
                drop(lock);
                Thread::<VM>::leave_native();
            }
        };

        self.holder.store(get_thread_id().get(), Ordering::Relaxed);
        self.rec_count.store(rec_count, Ordering::Relaxed);
        MonitorGuard {
            monitor: self,
            guard: ManuallyDrop::new(guard),
        }
    }
}

pub struct MonitorGuard<'a, T> {
    monitor: &'a Monitor<T>,
    guard: ManuallyDrop<MutexGuard<'a, T>>,
}

impl<T> Deref for MonitorGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<'a, T> MonitorGuard<'a, T> {
    pub fn wait_no_handshake(&mut self) {
        let rec_count = self.monitor.rec_count.swap(0, Ordering::Relaxed);
        let holder = self.monitor.holder.swap(0, Ordering::Relaxed);
        self.monitor.cvar.wait(&mut self.guard);
        self.monitor.rec_count.store(rec_count, Ordering::Relaxed);
        self.monitor.holder.store(holder, Ordering::Relaxed);
    }

    pub fn wait_for_no_handshake(&mut self, timeout: Duration) -> WaitTimeoutResult {
        let rec_count = self.monitor.rec_count.swap(0, Ordering::Relaxed);
        let holder = self.monitor.holder.swap(0, Ordering::Relaxed);
        let result = self.monitor.cvar.wait_for(&mut self.guard, timeout);
        self.monitor.rec_count.store(rec_count, Ordering::Relaxed);
        self.monitor.holder.store(holder, Ordering::Relaxed);
        result
    }

    pub fn notify(&self) {
        self.monitor.cvar.notify_one();
    }

    pub fn notify_all(&self) {
        self.monitor.cvar.notify_all();
    }

    pub fn monitor(&self) -> &Monitor<T> {
        self.monitor
    }

    pub unsafe fn unlock_completely(&mut self) -> usize {
        let result = self.monitor.rec_count.load(Ordering::Relaxed);
        self.monitor.rec_count.store(0, Ordering::Relaxed);
        self.monitor.holder.store(0, Ordering::Relaxed);
        unsafe {
            ManuallyDrop::drop(&mut self.guard);
        }
        result
    }

    pub fn wait_with_handshake<VM: VirtualMachine>(mut self) -> Self {
        let t = Thread::<VM>::current();
        t.context.save_thread_state();
        let rec_count = parked_scope::<usize, VM>(|| {
            self.wait_no_handshake();
            let rec_count = unsafe { self.unlock_completely() };
            rec_count
        });
        unsafe { self.monitor.relock_with_handshake::<VM>(rec_count) }
    }
}

impl<'a, T> Drop for MonitorGuard<'a, T> {
    fn drop(&mut self) {
        if self.monitor.rec_count.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.monitor.holder.store(0, Ordering::Relaxed);
            unsafe { ManuallyDrop::drop(&mut self.guard) };
        }
    }
}

