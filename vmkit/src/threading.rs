use std::{
    cell::{Cell, OnceCell, RefCell, UnsafeCell},
    mem::{offset_of, MaybeUninit},
    panic::AssertUnwindSafe,
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicI8, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    thread::JoinHandle,
};

use atomic::Atomic;
use mmtk::{
    util::{Address, VMMutatorThread, VMThread},
    vm::RootsWorkFactory,
    BarrierSelector, Mutator,
};

use crate::{
    mm::{
        conservative_roots::ConservativeRoots, stack_bounds::StackBounds, tlab::TLAB, MemoryManager,
    },
    object_model::compression::CompressedOps,
    sync::{Monitor, MonitorGuard},
    VirtualMachine,
};

/// Threads use a state machine to indicate their current state and how they should
/// be treated in case of asynchronous requests like GC. The state machine has
/// several states and legal transitions between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ThreadState {
    /// Thread has not yet started. This state holds right up until just before we
    /// call `thread::spawn()`.
    #[default]
    New,
    /// Thread is executing "normal" Managed code
    InManaged,
    /// A state used by the scheduler to mark that a thread is in privileged code
    /// that does not need to synchronize with the collector. This is a special
    /// state. As well, this state should only be entered unsafe code only. Typically,
    /// this state is entered using a call to `enter_native()` just prior to idling
    /// the thread; though it would not be wrong to enter it prior to any other
    /// long-running activity that does not require interaction with the GC.
    InNative,
    /// Thread is in Managed code but is expected to block. The transition from InManaged
    /// to InManagedToBlock happens as a result of an asynchronous call by the GC
    /// or any other internal VM code that requires this thread to perform an
    /// asynchronous activity (another example is the request to do an isync on RISCV64).
    /// The point is that we're waiting for the thread to reach a safe point and
    /// expect this to happen in bounded time; but if the thread were to escape to
    /// native we want to know about it. Thus, transitions into native code while
    /// in the InManagedToBlock state result in a notification (broadcast on the
    /// thread's monitor) and a state change to BlockedInNative. Observe that it
    /// is always safe to conservatively change InManaged to InManagedToBlock.
    InManagedToBlock,
    /// Thread is in native code, and is to block before returning to Managed code.
    /// The transition from InNative to BlockedInNative happens as a result
    /// of an asynchronous call by the GC or any other internal VM code that
    /// requires this thread to perform an asynchronous activity (another example
    /// is the request to do an isync on RISCV64). As well, code entering privileged
    /// code that would otherwise go from InManaged to InNative will go to
    /// BlockedInNative instead, if the state was InManagedToBlock.
    /// <p>
    /// The point of this state is that the thread is guaranteed not to execute
    /// any Managed code until:
    /// <ol>
    /// <li>The state changes to InNative, and
    /// <li>The thread gets a broadcast on its monitor.
    /// </ol>
    /// Observe that it is always safe to conservatively change InNative to
    /// BlockedInNative.
    BlockedInNative,
    /// Thread has died. As in, it's no longer executing any Managed code and will
    /// never do so in the future. Once this is set, the GC will no longer mark any
    /// part of the thread as live; the thread's stack will be deleted. Note that
    /// this is distinct from the about_to_terminate state.
    Terminated,
}

impl ThreadState {
    pub fn not_running(&self) -> bool {
        matches!(self, ThreadState::New | ThreadState::Terminated)
    }
}

unsafe impl bytemuck::NoUninit for ThreadState {}

pub trait ThreadContext<VM: VirtualMachine> {
    fn save_thread_state(&self);
    /// Scan roots in the thread.
    fn scan_roots(&self, factory: impl RootsWorkFactory<VM::Slot>);
    /// Scan conservative roots in the thread.
    ///
    /// Note that you can also do this in `scan_roots` by constructing
    /// `ConservativeRoots` manually, but this method is provided
    /// to separate precise vs conservative roots code.
    fn scan_conservative_roots(&self, croots: &mut ConservativeRoots);

    fn new(collector_context: bool) -> Self;

    /// Pre-yieldpoint hook. Invoked by the VM before entering [`yieldpoint`](Thread::yieldpoint)
    /// and before locking the thread's monitor.
    fn pre_yieldpoint(&self, where_from: i32, fp: Address) {
        let _ = where_from;
        let _ = fp;
    }

    /// Yieldpoint hook. Invoked by the VM when the thread is at a yieldpoint.
    ///
    /// Thread's monitor is locked when this is called.
    ///
    /// `before_block` is true if this function was invoked before the thread blocks.
    fn at_yieldpoint(&self, where_from: i32, fp: Address, before_block: bool) {
        let _ = where_from;
        let _ = fp;
        let _ = before_block;
    }

    /// Post-yieldpoint hook. Invoked by the VM after exiting [`yieldpoint`](Thread::yieldpoint)
    /// and after unlocking the thread's monitor.
    fn post_yieldpoint(&self, where_from: i32, fp: Address) {
        let _ = where_from;
        let _ = fp;
    }
}

static ID: AtomicU64 = AtomicU64::new(0);

/// A generic thread's execution context.
pub struct Thread<VM: VirtualMachine> {
    /// Should the next executed yieldpoint be taken? Can be true for a variety of
    /// reasons. See [Thread::yieldpoint].
    /// <p>
    /// To support efficient sampling of only prologue/epilogues we also encode
    /// some extra information into this field. 0 means that the yieldpoint should
    /// not be taken. >0 means that the next yieldpoint of any type should be taken
    /// <0 means that the next prologue/epilogue yieldpoint should be taken
    /// <p>
    /// Note the following rules:
    /// <ol>
    /// <li>If take_yieldpoint is set to 0 or -1 it is perfectly safe to set it to
    /// 1; this will have almost no effect on the system. Thus, setting
    /// take_yieldpoint to 1 can always be done without acquiring locks.</li>
    /// <li>Setting take_yieldpoint to any value other than 1 should be done while
    /// holding the thread's monitor().</li>
    /// <li>The exception to rule (2) above is that the yieldpoint itself sets
    /// take_yieldpoint to 0 without holding a lock - but this is done after it
    /// ensures that the yieldpoint is deferred by setting yieldpoint_request_pending
    /// to true.
    /// </ol>
    take_yieldpoint: AtomicI32,

    pub context: VM::ThreadContext,
    pub tlab: UnsafeCell<TLAB>,
    max_non_los_default_alloc_bytes: Cell<usize>,
    barrier: Cell<BarrierSelector>,
    mmtk_mutator: UnsafeCell<MaybeUninit<Mutator<MemoryManager<VM>>>>,
    has_collector_context: AtomicBool,
    exec_status: Atomic<ThreadState>,
    ignore_handshakes_and_gc: AtomicBool,
    /// Is the thread about to terminate? Protected by the thread's monitor. Note
    /// that this field is not set atomically with the entering of the thread onto
    /// the aboutToTerminate array - in fact it happens before that. When this
    /// field is set to true, the thread's stack will no longer be scanned by GC.
    /// Note that this is distinct from the [`Terminated`](ThreadState::Terminated) state.
    is_about_to_terminate: AtomicBool,
    /// Is this thread in the process of blocking?
    is_blocking: AtomicBool,
    /// Is the thread no longer executing user code? Protected by the monitor
    /// associated with the Thread.
    is_joinable: AtomicBool,
    thread_id: u64,
    index_in_manager: AtomicUsize,

    yieldpoints_enabled_count: AtomicI8,
    yieldpoint_request_pending: AtomicBool,
    at_yieldpoint: AtomicBool,
    soft_handshake_requested: AtomicBool,
    active_mutator_context: AtomicBool,

    is_blocked_for_gc: AtomicBool,
    should_block_for_gc: AtomicBool,
    /// The monitor of the thread. Protects access to the thread's state.
    monitor: Monitor<()>,
    communication_lock: Monitor<()>,
    stack_bounds: OnceCell<StackBounds>,
}

unsafe impl<VM: VirtualMachine> Send for Thread<VM> {}
unsafe impl<VM: VirtualMachine> Sync for Thread<VM> {}

impl<VM: VirtualMachine> Thread<VM> {
    /// Create a new thread without checking threa type.
    ///
    /// # Safety
    ///
    /// This function is unsafe because it does not enforce a "thread" type
    /// invariant. You can create only three types of threads:
    /// 1) Collector threads: MMTk workers, they are actually created by VMKit itself.
    /// 2) Mutators: created by the user (apart from main thread)
    /// 3) Concurrent threads: these threads are created by the user and are not mutators nor collector threads.
    ///     These threads do not allow access to heap objects, they can be used to perform blocking tasks.
    ///
    /// If thread type is mismatched, the behavior is undefined: it could lead to deadlock, leak or memory corruption
    /// as we optimize thread creation.
    pub unsafe fn new_unchecked(
        ctx: Option<VM::ThreadContext>,
        collector_context: bool,
        is_mutator: bool,
    ) -> Arc<Self> {
        let vm = VM::get();
        if collector_context {
            assert!(
                !vm.vmkit().are_collector_threads_spawned(),
                "Attempt to create a collector thread after collector threads have been spawned"
            );
            assert!(!is_mutator, "Collector threads cannot be mutators");
        }

        Arc::new(Self {
            tlab: UnsafeCell::new(TLAB::new()),
            stack_bounds: OnceCell::new(),
            barrier: Cell::new(BarrierSelector::NoBarrier),
            max_non_los_default_alloc_bytes: Cell::new(0),
            take_yieldpoint: AtomicI32::new(0),
            context: ctx.unwrap_or_else(|| VM::ThreadContext::new(collector_context)),
            mmtk_mutator: UnsafeCell::new(MaybeUninit::uninit()),
            has_collector_context: AtomicBool::new(collector_context),
            exec_status: Atomic::new(ThreadState::New),
            ignore_handshakes_and_gc: AtomicBool::new(!is_mutator || collector_context),
            is_about_to_terminate: AtomicBool::new(false),
            is_blocking: AtomicBool::new(false),
            is_joinable: AtomicBool::new(false),
            thread_id: ID.fetch_add(1, Ordering::SeqCst),
            index_in_manager: AtomicUsize::new(0),
            yieldpoints_enabled_count: AtomicI8::new(0),
            yieldpoint_request_pending: AtomicBool::new(false),
            at_yieldpoint: AtomicBool::new(false),
            soft_handshake_requested: AtomicBool::new(false),
            active_mutator_context: AtomicBool::new(is_mutator),
            is_blocked_for_gc: AtomicBool::new(false),
            should_block_for_gc: AtomicBool::new(false),
            monitor: Monitor::new(()),
            communication_lock: Monitor::new(()),
        })
    }

    pub fn for_mutator(context: VM::ThreadContext) -> Arc<Self> {
        unsafe { Self::new_unchecked(Some(context), false, true) }
    }

    pub(crate) fn for_collector() -> Arc<Self> {
        unsafe { Self::new_unchecked(None, true, false) }
    }

    pub fn for_concurrent(context: VM::ThreadContext) -> Arc<Self> {
        unsafe {
            let thread = Self::new_unchecked(Some(context), false, false);
            thread.set_ignore_handshakes_and_gc(true);
            thread
        }
    }

    /*pub(crate) fn start_gc(self: &Arc<Self>, ctx: Box<GCWorker<MemoryManager<VM>>>) {
        unsafe {
            self.set_exec_status(ThreadState::InNative);
            let this = self.clone();
            std::thread::spawn(move || {
                let vmkit = VM::get().vmkit();
                init_current_thread(this.clone());
                vmkit.thread_manager().add_thread(this.clone());
                mmtk::memory_manager::start_worker(
                    &vmkit.mmtk,
                    mmtk::util::VMWorkerThread(this.to_vm_thread()),
                    ctx,
                );
            });
        }
    }*/

    /// Start execution of `self` by creating and starting a native thread.
    pub fn start<F, R>(self: &Arc<Self>, f: F) -> JoinHandle<Option<R>>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        unsafe {
            self.set_exec_status(ThreadState::InManaged);
            let this = self.clone();
            std::thread::spawn(move || this.startoff(f))
        }
    }

    pub fn to_vm_thread(&self) -> VMThread {
        unsafe { std::mem::transmute(self) }
    }

    pub fn stack_bounds(&self) -> &StackBounds {
        self.stack_bounds.get().unwrap()
    }

    pub fn barrier(&self) -> BarrierSelector {
        self.barrier.get()
    }

    pub fn max_non_los_default_alloc_bytes(&self) -> usize {
        self.max_non_los_default_alloc_bytes.get()
    }

    /// Begin execution of current thread by calling `run` method
    /// on the provided context.
    fn startoff<F, R>(self: &Arc<Self>, f: F) -> Option<R>
    where
        F: FnOnce() -> R,
        R: Send + 'static,
    {
        init_current_thread(self.clone());
        let constraints = VM::get().vmkit().mmtk.get_plan().constraints();
        self.max_non_los_default_alloc_bytes
            .set(constraints.max_non_los_default_alloc_bytes);
        self.barrier.set(constraints.barrier);
        self.stack_bounds
            .set(StackBounds::current_thread_stack_bounds())
            .unwrap();
        let vmkit = VM::get().vmkit();
        if !self.is_collector_thread() && !self.ignore_handshakes_and_gc() {
            let mutator = mmtk::memory_manager::bind_mutator(
                &vmkit.mmtk,
                VMMutatorThread(Self::current().to_vm_thread()),
            );
            unsafe { self.mmtk_mutator.get().write(MaybeUninit::new(*mutator)) };
            self.enable_yieldpoints();
        }
        vmkit.thread_manager.add_thread(self.clone());

        let _result = std::panic::catch_unwind(AssertUnwindSafe(|| f()));

        self.terminate();

        _result.ok()
    }

    fn terminate(&self) {
        self.is_joinable.store(true, Ordering::Relaxed);
        self.monitor.notify_all();
        self.add_about_to_terminate();
    }

    pub fn main<F, R>(context: VM::ThreadContext, f: F) -> Option<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let this = Thread::for_mutator(context);

        init_current_thread(this.clone());
        unsafe { this.set_exec_status(ThreadState::InManaged) };
        let vmkit = VM::get().vmkit();
        vmkit.thread_manager.add_main_thread(this.clone());
        mmtk::memory_manager::initialize_collection(&vmkit.mmtk, this.to_vm_thread());
        CompressedOps::init::<VM>();
        let _result = this.startoff(f);
        _result
    }

    /// Offset of the `take_yieldpoint` field.
    ///
    /// You can use this offset to emit code to check for the yieldpoint
    /// inline, without calling `get_take_yieldpoint()`.
    pub const TAKE_YIELDPOINT_OFFSET: usize = offset_of!(Thread<VM>, take_yieldpoint);

    pub fn get_exec_status(&self) -> ThreadState {
        self.exec_status.load(atomic::Ordering::Relaxed)
    }

    pub fn id(&self) -> u64 {
        self.thread_id
    }

    /// Check if the thread is ignoring handshakes and GC.
    pub fn ignore_handshakes_and_gc(&self) -> bool {
        self.ignore_handshakes_and_gc
            .load(atomic::Ordering::Relaxed)
    }

    /// Allow thread to ignore handshakes and GC which allows it to run
    /// concurrently to GC and other runtime functionality.
    ///
    /// # Safety
    ///
    /// This removes thread from root-set. All objects stored by this thread
    /// must be rooted in some other way.
    pub unsafe fn set_ignore_handshakes_and_gc(&self, ignore: bool) {
        self.ignore_handshakes_and_gc
            .store(ignore, atomic::Ordering::Relaxed);
    }

    /// Set the execution status of the thread.
    ///
    /// # SAFETY
    ///
    /// This function is unsafe because it does not enforce the thread's monitor
    /// nor does it notify VM of the state change.
    ///
    /// Use this function only when you are holding [`monitor()`](Self::monitor) lock.
    pub unsafe fn set_exec_status(&self, status: ThreadState) {
        self.exec_status.store(status, atomic::Ordering::Relaxed);
    }

    /// Attempt to transition the execution status of the thread.
    ///
    /// # SAFETY
    ///
    /// This function is unsafe because it does not enforce the thread's monitor
    /// nor does it notify VM of the state change.
    ///
    /// Use this function only when you are holding [`monitor()`](Self::monitor) lock.
    pub unsafe fn attempt_fast_exec_status_transition(
        &self,
        old_state: ThreadState,
        new_state: ThreadState,
    ) -> bool {
        self.exec_status
            .compare_exchange(
                old_state,
                new_state,
                atomic::Ordering::Relaxed,
                atomic::Ordering::Relaxed,
            )
            .is_ok()
    }

    pub fn is_about_to_terminate(&self) -> bool {
        self.is_about_to_terminate.load(atomic::Ordering::Relaxed)
    }

    /// Should the next executed yieldpoint be taken? Can be true for a variety of
    /// reasons. See [Thread::yieldpoint].
    /// <p>
    /// To support efficient sampling of only prologue/epilogues we also encode
    /// some extra information into this field. 0 means that the yieldpoint should
    /// not be taken. >0 means that the next yieldpoint of any type should be taken
    /// <0 means that the next prologue/epilogue yieldpoint should be taken
    /// <p>
    /// Note the following rules:
    /// <ol>
    /// <li>If take_yieldpoint is set to 0 or -1 it is perfectly safe to set it to
    /// 1; this will have almost no effect on the system. Thus, setting
    /// take_yieldpoint to 1 can always be done without acquiring locks.</li>
    /// <li>Setting take_yieldpoint to any value other than 1 should be done while
    /// holding the thread's monitor().</li>
    /// <li>The exception to rule (2) above is that the yieldpoint itself sets
    /// take_yieldpoint to 0 without holding a lock - but this is done after it
    /// ensures that the yieldpoint is deferred by setting yieldpoint_request_pending
    /// to true.
    /// </ol>
    ///
    /// # Returns
    ///
    /// Returns the value of `take_yieldpoint`.
    pub fn take_yieldpoint(&self) -> i32 {
        self.take_yieldpoint.load(atomic::Ordering::Relaxed)
    }

    /// Set the take yieldpoint value.
    ///
    /// # SAFETY
    ///
    /// It is always safe to change `take_yieldpoint` to any value as [`yieldpoint()`](Self::yieldpoint)
    /// will reset it to 0. Any behaviour exeucted inside `yieldpoint` requires one of block
    /// adapters to acknowledge the yieldpoint request and this can't be done by solely calling `set_take_yieldpoint`.
    pub fn set_take_yieldpoint(&self, value: i32) {
        self.take_yieldpoint.store(value, atomic::Ordering::Relaxed);
    }

    pub fn monitor(&self) -> &Monitor<()> {
        &self.monitor
    }

    pub fn communication_lock(&self) -> &Monitor<()> {
        &self.communication_lock
    }

    /// Check if the thread has block requests (for example, for suspension and GC).
    /// If it does, clear the requests and marked the thread as blocked for that request.
    /// If there were any block requests, do a notify_all() on the thread's monitor().
    ///
    /// This is an internal method and should only be called from code that implements
    /// thread blocking. The monitor() lock must be held for this method to work properly.
    fn acknowledge_block_requests(&self) {
        let had_some = VM::BlockAdapterList::acknowledge_block_requests(self);
        if had_some {
            self.monitor.notify_all();
        }
    }

    /// Checks if the thread system has acknowledged that the thread is supposed
    /// to be blocked. This will return true if the thread is actually blocking, or
    /// if the thread is running native code but is guaranteed to block before
    /// returning to Java. Only call this method when already holding the monitor(),
    /// for two reasons:
    ///
    /// 1. This method does not acquire the monitor() lock even though it needs
    ///    to have it acquired given the data structures that it is accessing.
    /// 2. You will typically want to call this method to decide if you need to
    ///    take action under the assumption that the thread is blocked (or not
    ///    blocked). So long as you hold the lock the thread cannot change state from
    ///    blocked to not blocked.
    ///
    /// # Returns
    ///
    /// Returns `true` if the thread is supposed to be blocked.
    pub fn is_blocked(&self) -> bool {
        VM::BlockAdapterList::is_blocked(self)
    }

    /// Checks if the thread is executing managed code. A thread is executing managed
    /// code if its `exec_status` is `InManaged` or `InManagedToBlock`, and if it is not
    /// `about_to_terminate`, and if it is not blocked. Only call this method when already holding
    /// the monitor(), and probably only after calling `set_blocked_exec_status()`, for two reasons:
    /// 1. This method does not acquire the monitor() lock even though it needs
    ///    to have it acquired given the data structures that it is accessing.
    /// 2. You will typically want to call this method to decide if you need to
    ///    take action under the assumption that the thread is running managed code (or not
    ///    running managed code). So long as you hold the lock - and you have called
    ///    `set_blocked_exec_status()` - the thread cannot change state from running-managed
    ///    to not-running-managed.
    ///
    /// # Returns
    ///
    /// Returns `true` if the thread is running managed code.
    pub fn is_in_managed(&self) -> bool {
        !self.is_blocking.load(atomic::Ordering::Relaxed)
            && !self.is_about_to_terminate.load(atomic::Ordering::Relaxed)
            && (self.get_exec_status() == ThreadState::InManaged
                || self.get_exec_status() == ThreadState::InManagedToBlock)
    }

    fn check_block_no_save_context(&self) {
        let mut lock = self.monitor().lock_no_handshake();
        self.is_blocking.store(true, Ordering::Relaxed);

        loop {
            // deal with block requests
            self.acknowledge_block_requests();
            // are we blocked?
            if !self.is_blocked() {
                break;
            }
            // what if a GC request comes while we're here for a suspend()
            // request?
            // answer: we get awoken, reloop, and acknowledge the GC block
            // request.
            lock.wait_no_handshake();
        }

        // SAFETY: We are holding the monitor lock.
        unsafe {
            self.set_exec_status(ThreadState::InManaged);
        }
        self.is_blocking.store(false, Ordering::Relaxed);
        drop(lock);
    }

    /// Check if the thread is supposed to block, and if so, block it. This method
    /// will ensure that soft handshake requests are acknowledged or else
    /// inhibited, that any blocking request is handled, that the execution state
    /// of the thread (`exec_status`) is set to `InManaged`
    /// once all blocking requests are cleared, and that other threads are notified
    /// that this thread is in the middle of blocking by setting the appropriate
    /// flag (`is_blocking`). Note that this thread acquires the
    /// monitor(), though it may release it completely either by calling wait() or
    /// by calling unlock_completely(). Thus, although it isn't generally a problem
    /// to call this method while holding the monitor() lock, you should only do so
    /// if the loss of atomicity is acceptable.
    ///
    /// Generally, this method should be called from the following four places:
    ///
    /// 1. The block() method, if the thread is requesting to block itself.
    ///    Currently such requests only come when a thread calls suspend(). Doing so
    ///    has unclear semantics (other threads may call resume() too early causing
    ///    a race) but must be supported because it's still part of the
    ///    JDK. Why it's safe: the block() method needs to hold the monitor() for the
    ///    time it takes it to make the block request, but does not need to continue
    ///    to hold it when it calls check_block(). Thus, the fact that check_block()
    ///    breaks atomicity is not a concern.
    ///
    /// 2. The yieldpoint. One of the purposes of a yieldpoint is to periodically
    ///    check if the current thread should be blocked. This is accomplished by
    ///    calling check_block(). Why it's safe: the yieldpoint performs several
    ///    distinct actions, all of which individually require the monitor() lock -
    ///    but the monitor() lock does not have to be held contiguously. Thus, the
    ///    loss of atomicity from calling check_block() is fine.
    ///
    /// 3. The "with_handshake" methods of HeavyCondLock. These methods allow you to
    ///    block on a mutex or condition variable while notifying the system that you
    ///    are not executing managed code. When these blocking methods return, they check
    ///    if there had been a request to block, and if so, they call check_block().
    ///    Why it's safe: This is subtle. Two cases exist. The first case is when a
    ///    WithHandshake method is called on a HeavyCondLock instance that is not a thread
    ///    monitor(). In this case, it does not matter that check_block() may acquire
    ///    and then completely release the monitor(), since the user was not holding
    ///    the monitor(). However, this will break if the user is *also* holding
    ///    the monitor() when calling the WithHandshake method on a different lock. This case
    ///    should never happen because no other locks should ever be acquired when the
    ///    monitor() is held. Additionally: there is the concern that some other locks
    ///    should never be held while attempting to acquire the monitor(); the
    ///    HeavyCondLock ensures that checkBlock() is only called when that lock
    ///    itself is released. The other case is when a WithHandshake method is called on the
    ///    monitor() itself. This should only be done when using *your own*
    ///    monitor() - that is the monitor() of the thread your are running on. In
    ///    this case, the WithHandshake methods work because: (i) lock_with_handshake() only calls
    ///    check_block() on the initial lock entry (not on recursive entry), so
    ///    atomicity is not broken, and (ii) wait_with_handshake() and friends only call
    ///    check_block() after wait() returns - at which point it is safe to release
    ///    and reacquire the lock, since there cannot be a race with broadcast() once
    ///    we have committed to not calling wait() again.
    pub fn check_block(&self) {
        self.context.save_thread_state();
        self.check_block_no_save_context();
    }

    fn enter_native_blocked_impl(&self) {
        let lock = self.monitor.lock_no_handshake();

        unsafe {
            self.set_exec_status(ThreadState::BlockedInNative);
        }

        self.acknowledge_block_requests();

        drop(lock);
    }

    fn leave_native_blocked_impl(&self) {
        self.check_block_no_save_context();
    }

    fn enter_native_blocked(&self) {
        self.enter_native_blocked_impl();
    }

    fn leave_native_blocked(&self) {
        self.leave_native_blocked_impl();
    }

    fn set_blocked_exec_status(&self) -> ThreadState {
        let mut old_state;
        loop {
            old_state = self.get_exec_status();
            let new_state = match old_state {
                ThreadState::InManaged => ThreadState::InManagedToBlock,
                ThreadState::InNative => ThreadState::BlockedInNative,
                _ => old_state,
            };

            if unsafe { self.attempt_fast_exec_status_transition(old_state, new_state) } {
                break new_state;
            }
        }
    }
    /// Attempts to block the thread, and returns the state it is in after the attempt.
    ///
    /// If we're blocking ourselves, this will always return `InManaged`. If the thread
    /// signals to us the intention to die as we are trying to block it, this will return
    /// `Terminated`. NOTE: the thread's exec_status will not actually be `Terminated` at
    /// that point yet.
    ///
    /// # Warning
    /// This method is ridiculously dangerous, especially if you pass `asynchronous=false`.
    /// Waiting for another thread to stop is not in itself interruptible - so if you ask
    /// another thread to block and they ask you to block, you might deadlock.
    ///
    /// # Safety
    /// This function is unsafe because it modifies thread state without synchronization.
    /// Other threads might cause a deadlock if current thread blocks itself and other thread
    /// calls `resume()`.
    ///
    /// # Arguments
    /// * `asynchronous` - If true, the request is asynchronous (i.e. the receiver is only notified).
    ///                    If false, the caller waits for the receiver to block.
    ///
    /// # Returns
    /// The new state of the thread after the block attempt.
    pub fn block_unchecked<A: BlockAdapter<VM>>(&self, asynchronous: bool) -> ThreadState {
        let mut result;

        let mut lock = self.monitor.lock_no_handshake();
        let token = A::request_block(self);

        if current_thread::<VM>().thread_id == self.thread_id {
            self.check_block();
            result = self.get_exec_status();
        } else {
            if self.is_about_to_terminate() {
                result = ThreadState::Terminated;
            } else {
                self.take_yieldpoint.store(1, Ordering::Relaxed);
                let new_state = self.set_blocked_exec_status();
                result = new_state;

                self.monitor.notify_all();

                if new_state == ThreadState::InManagedToBlock {
                    if !asynchronous {
                        while A::has_block_request_with_token(self, token)
                            && !A::is_blocked(self)
                            && !self.is_about_to_terminate()
                        {
                            lock.wait_no_handshake();
                        }

                        if self.is_about_to_terminate() {
                            result = ThreadState::Terminated;
                        } else {
                            result = self.get_exec_status();
                        }
                    }
                } else if new_state == ThreadState::BlockedInNative {
                    A::clear_block_request(self);
                    A::set_blocked(self, true);
                }
            }
        }
        drop(lock);
        result
    }
    /// Returns whether this thread is currently in a mutator context.
    ///
    /// A mutator context is one where the thread is actively performing application work,
    /// as opposed to being blocked or performing GC-related activities.
    pub fn active_mutator_context(&self) -> bool {
        self.active_mutator_context.load(atomic::Ordering::Relaxed)
    }

    pub fn current() -> &'static Thread<VM> {
        current_thread::<VM>()
    }

    pub fn begin_pair_with<'a>(
        &'a self,
        other: &'a Thread<VM>,
    ) -> (MonitorGuard<'a, ()>, MonitorGuard<'a, ()>) {
        let guard1 = self.communication_lock.lock_no_handshake();
        let guard2 = other.communication_lock.lock_no_handshake();
        (guard1, guard2)
    }

    pub fn begin_pair_with_current(&self) -> (MonitorGuard<'_, ()>, MonitorGuard<'_, ()>) {
        self.begin_pair_with(current_thread::<VM>())
    }

    fn safe_block_impl<A: BlockAdapter<VM>>(&self, asynchronous: bool) -> ThreadState {
        let (guard1, guard2) = self.begin_pair_with_current();
        // SAFETY: threads are paired, no deadlock can occur.
        let result = self.block_unchecked::<A>(asynchronous);
        drop(guard1);
        drop(guard2);
        result
    }

    /// Safe version of [`block_unchecked`] that blocks the thread synchronously.
    ///
    /// Prevents race with other threads by pairing current thread with the target thread.
    pub fn block<A: BlockAdapter<VM>>(&self) -> ThreadState {
        self.safe_block_impl::<A>(false)
    }

    /// Safe version of [`block_unchecked`] that blocks the thread asynchronously.
    ///
    /// Prevents race with other threads by pairing current thread with the target thread.
    pub fn async_block<A: BlockAdapter<VM>>(&self) -> ThreadState {
        self.safe_block_impl::<A>(true)
    }

    pub fn is_blocked_for<A: BlockAdapter<VM>>(&self) -> bool {
        A::is_blocked(self)
    }

    pub fn enter_native() {
        let t = Self::current();

        let mut old_state;
        loop {
            old_state = t.get_exec_status();
            let new_state = if old_state == ThreadState::InManaged {
                ThreadState::InNative
            } else {
                t.enter_native_blocked();
                return;
            };

            if unsafe { t.attempt_fast_exec_status_transition(old_state, new_state) } {
                break;
            }
        }
    }

    #[must_use = "If thread can't leave native state without blocking, call [leave_native](Thread::leave_native) instead"]
    pub fn attempt_leave_native_no_block() -> bool {
        let t = Self::current();

        let mut old_state;
        loop {
            old_state = t.get_exec_status();
            let new_state = if old_state == ThreadState::InNative {
                ThreadState::InManaged
            } else {
                return false;
            };

            if unsafe { t.attempt_fast_exec_status_transition(old_state, new_state) } {
                break true;
            }
        }
    }

    pub fn leave_native() {
        if !Self::attempt_leave_native_no_block() {
            Self::current().leave_native_blocked();
        }
    }

    pub fn unblock<A: BlockAdapter<VM>>(&self) {
        let lock = self.monitor.lock_no_handshake();
        A::clear_block_request(self);
        A::set_blocked(self, false);
        self.monitor.notify_all();
        drop(lock);
    }

    pub fn yieldpoints_enabled(&self) -> bool {
        self.yieldpoints_enabled_count.load(Ordering::Relaxed) == 1
    }

    pub fn enable_yieldpoints(&self) {
        let val = self
            .yieldpoints_enabled_count
            .fetch_add(1, Ordering::Relaxed);
        debug_assert!(val <= 1);

        if self.yieldpoints_enabled() && self.yieldpoint_request_pending.load(Ordering::Relaxed) {
            self.take_yieldpoint.store(1, Ordering::Relaxed);
            self.yieldpoint_request_pending
                .store(false, Ordering::Relaxed);
        }
    }

    pub fn disable_yieldpoints(&self) {
        self.yieldpoints_enabled_count
            .fetch_sub(1, Ordering::Relaxed);
    }

    pub fn from_vm_thread(vmthread: mmtk::util::VMThread) -> &'static Thread<VM> {
        unsafe { std::mem::transmute(vmthread) }
    }

    pub fn from_vm_mutator_thread(vmthread: mmtk::util::VMMutatorThread) -> &'static Thread<VM> {
        unsafe { std::mem::transmute(vmthread) }
    }

    pub fn from_vm_worker_thread(vmthread: mmtk::util::VMWorkerThread) -> &'static Thread<VM> {
        unsafe { std::mem::transmute(vmthread) }
    }

    pub unsafe fn mutator_unchecked(&self) -> &'static mut Mutator<MemoryManager<VM>> {
        unsafe {
            let ptr = self.mmtk_mutator.get();
            (*ptr).assume_init_mut()
        }
    }

    pub fn mutator(&self) -> &'static mut Mutator<MemoryManager<VM>> {
        unsafe {
            assert!(Thread::<VM>::current().thread_id == self.thread_id);
            self.mutator_unchecked()
        }
    }

    pub fn is_collector_thread(&self) -> bool {
        self.has_collector_context.load(atomic::Ordering::Relaxed)
    }

    fn add_about_to_terminate(&self) {
        let lock = self.monitor.lock_no_handshake();
        self.is_about_to_terminate
            .store(true, atomic::Ordering::Relaxed);
        self.active_mutator_context
            .store(false, atomic::Ordering::Relaxed);
        lock.notify_all();

        mmtk::memory_manager::destroy_mutator(self.mutator());
        // WARNING! DANGER! Since we've set is_about_to_terminate to true, when we
        // release this lock the GC will:
        // 1) No longer scan the thread's stack (though it will *see* the
        // thread's stack and mark the stack itself as live, without scanning
        // it).
        // 2) No longer include the thread in any mutator phases ... hence the
        // need to ensure that the mutator context is flushed above.
        // 3) No longer attempt to block the thread.
        // Moreover, we can no longer do anything that leads to write barriers
        // or allocation.
        drop(lock);

        let vmkit = VM::get().vmkit();
        unsafe {
            self.mutator_unchecked().on_destroy();
            let x = self.mmtk_mutator.get();
            (*x).assume_init_drop();
        }
        vmkit.thread_manager().add_about_to_terminate_current();
    }

    pub fn snapshot_handshake_threads<V>(vm: &VM, visitor: &V) -> usize
    where
        V: SoftHandshakeVisitor<VM>,
    {
        let vmkit = vm.vmkit();
        let inner = vmkit.thread_manager.inner.lock_no_handshake();
        let inner = inner.borrow();
        let mut num_to_handshake = 0;
        let handshake_threads = vmkit.thread_manager.handshake_lock.lock_no_handshake();
        let mut handshake_threads = handshake_threads.borrow_mut();
        for i in 0..inner.threads.len() {
            if let Some(thread) = inner.threads[i].as_ref() {
                if !thread.is_collector_thread() && visitor.include_thread(thread) {
                    num_to_handshake += 1;
                    handshake_threads.push(thread.clone());
                }
            }
        }

        drop(inner);

        num_to_handshake
    }

    /// Tell each thread to take a yieldpoint and wait until all of them have done
    /// so at least once. Additionally, call the visitor on each thread when making
    /// the yieldpoint request; the purpose of the visitor is to set any additional
    /// fields as needed to make specific requests to the threads that yield. Note
    /// that the visitor's <code>visit()</code> method is called with both the
    /// thread's monitor held, and the <code>soft_handshake_data_lock</code> held.
    /// <p>
    pub fn soft_handshake<V>(visitor: V)
    where
        V: SoftHandshakeVisitor<VM>,
    {
        let vm = VM::get();

        let num_to_handshake = Self::snapshot_handshake_threads(vm, &visitor);

        let handshake_lock = vm.vmkit().thread_manager.handshake_lock.lock_no_handshake();
        let mut handshake_lock = handshake_lock.borrow_mut();

        for thread in handshake_lock.drain(..num_to_handshake) {
            let lock = thread.monitor().lock_no_handshake();
            let mut wait_for_this_thread = false;
            if !thread.is_about_to_terminate() && visitor.check_and_signal(&thread) {
                thread.set_blocked_exec_status();
                // Note that at this point if the thread tries to either enter or
                // exit managed code, it will be diverted into either
                // enter_native_blocked() or check_block(), both of which cannot do
                // anything until they acquire the monitor() lock, which we now
                // hold. Thus, the code below can, at its leisure, examine the
                // thread's state and make its decision about what to do, fully
                // confident that the thread's state is blocked from changing.
                if thread.is_in_managed() {
                    // the thread is currently executing managed code, so we must ensure
                    // that it either:
                    // 1) takes the next yieldpoint and rendezvous with this soft
                    // handshake request (see yieldpoint), or
                    // 2) performs the rendezvous when leaving managed code
                    // (see enter_native_blocked, check_block, and add_about_to_terminate)
                    // either way, we will wait for it to get there before exiting
                    // this call, since the caller expects that after softHandshake()
                    // returns, no thread will be running Java code without having
                    // acknowledged.
                    thread
                        .soft_handshake_requested
                        .store(true, atomic::Ordering::Relaxed);
                    thread.take_yieldpoint.store(1, Ordering::Relaxed);
                    wait_for_this_thread = true;
                } else {
                    // the thread is not in managed code (it may be blocked or it may be
                    // in native), so we don't have to wait for it since it will
                    // do the Right Thing before returning to managed code. essentially,
                    // the thread cannot go back to running managed code without doing whatever
                    // was requested because:
                    // A) we've set the execStatus to blocked, and
                    // B) we're holding its lock.
                    visitor.notify_stuck_in_native(&thread);
                }

                drop(lock);

                // NOTE: at this point the thread may already decrement the
                // softHandshakeLeft counter, causing it to potentially go negative.
                // this is unlikely and completely harmless.
                if wait_for_this_thread {
                    let lock = vm
                        .vmkit()
                        .thread_manager
                        .soft_handshake_data_lock
                        .lock_no_handshake();
                    vm.vmkit()
                        .thread_manager
                        .soft_handshake_left
                        .fetch_add(1, atomic::Ordering::Relaxed);
                    drop(lock);
                }
            }

            // wait for all threads to reach the handshake
            let mut lock = vm
                .vmkit()
                .thread_manager
                .soft_handshake_data_lock
                .lock_no_handshake();
            while vm
                .vmkit()
                .thread_manager
                .soft_handshake_left
                .load(atomic::Ordering::Relaxed)
                > 0
            {
                lock = lock.wait_with_handshake::<VM>();
            }
            drop(lock);

            vm.vmkit().thread_manager().process_about_to_terminate();
        }
    }

    /// Process a taken yieldpoint.
    ///
    /// # Parameters
    ///
    /// - `where_from`: The source of the yieldpoint (e.g. backedge)
    /// - `fp`: The frame pointer of the service method that called this method
    ///
    /// Exposed as `extern "C-unwind"` to allow directly invoking it from JIT/AOT code.
    pub extern "C-unwind" fn yieldpoint(where_from: i32, fp: Address) {
        let thread = Thread::<VM>::current();
        let _was_at_yieldpoint = thread.at_yieldpoint.load(atomic::Ordering::Relaxed);
        thread.at_yieldpoint.store(true, atomic::Ordering::Relaxed);

        // If thread is in critical section we can't do anything right now, defer
        // until later
        // we do this without acquiring locks, since part of the point of disabling
        // yieldpoints is to ensure that locks are not "magically" acquired
        // through unexpected yieldpoints. As well, this makes code running with
        // yieldpoints disabled more predictable. Note furthermore that the only
        // race here is setting takeYieldpoint to 0. But this is perfectly safe,
        // since we are guaranteeing that a yieldpoint will run after we emerge from
        // the no-yieldpoints code. At worst, setting takeYieldpoint to 0 will be
        // lost (because some other thread sets it to non-0), but in that case we'll
        // just come back here and reset it to 0 again.
        if !thread.yieldpoints_enabled() {
            thread
                .yieldpoint_request_pending
                .store(true, atomic::Ordering::Relaxed);
            thread.take_yieldpoint.store(0, atomic::Ordering::Relaxed);
            thread.at_yieldpoint.store(false, atomic::Ordering::Relaxed);
            return;
        }

        thread.context.pre_yieldpoint(where_from, fp);

        let lock = thread.monitor().lock_no_handshake();

        if thread.take_yieldpoint() != 0 {
            thread.set_take_yieldpoint(0);
            thread.context.at_yieldpoint(where_from, fp, true);
            // do two things: check if we should be blocking, and act upon
            // handshake requests.
            thread.check_block();
        }

        thread.context.at_yieldpoint(where_from, fp, false);

        drop(lock);
        thread.context.post_yieldpoint(where_from, fp);
        thread.at_yieldpoint.store(false, atomic::Ordering::Relaxed);
    }
}

thread_local! {
    static CURRENT_THREAD: RefCell<Address> = RefCell::new(Address::ZERO);
}

pub fn current_thread<VM: VirtualMachine>() -> &'static Thread<VM> {
    let addr = CURRENT_THREAD.with(|t| *t.borrow());

    assert!(!addr.is_zero());
    unsafe { addr.as_ref() }
}

pub fn try_current_thread<VM: VirtualMachine>() -> Option<&'static Thread<VM>> {
    let addr = CURRENT_THREAD.with(|t| *t.borrow());

    if addr.is_zero() {
        None
    } else {
        Some(unsafe { addr.as_ref() })
    }
}

pub(crate) fn init_current_thread<VM: VirtualMachine>(thread: Arc<Thread<VM>>) {
    let thread = Arc::into_raw(thread);
    CURRENT_THREAD.with(|t| *t.borrow_mut() = Address::from_ptr(thread));
}

pub(crate) fn deinit_current_thread<VM: VirtualMachine>() {
    CURRENT_THREAD.with(|t| {
        let mut threadptr = t.borrow_mut();
        {
            let thread: Arc<Thread<VM>> = unsafe { Arc::from_raw(threadptr.to_ptr()) };
            drop(thread);
        }
        *threadptr = Address::ZERO;
    })
}

pub struct ThreadManager<VM: VirtualMachine> {
    inner: Monitor<RefCell<ThreadManagerInner<VM>>>,
    soft_handshake_left: AtomicUsize,
    soft_handshake_data_lock: Monitor<()>,
    handshake_lock: Monitor<RefCell<Vec<Arc<Thread<VM>>>>>,
}

struct ThreadManagerInner<VM: VirtualMachine> {
    threads: Vec<Option<Arc<Thread<VM>>>>,
    free_thread_indices: Vec<usize>,
    about_to_terminate: Vec<Arc<Thread<VM>>>,
}

impl<VM: VirtualMachine> ThreadManagerInner<VM> {
    pub fn new() -> Self {
        Self {
            threads: Vec::new(),
            free_thread_indices: Vec::new(),
            about_to_terminate: Vec::new(),
        }
    }
}

impl<VM: VirtualMachine> ThreadManager<VM> {
    pub fn new() -> Self {
        Self {
            inner: Monitor::new(RefCell::new(ThreadManagerInner::new())),
            soft_handshake_left: AtomicUsize::new(0),
            soft_handshake_data_lock: Monitor::new(()),
            handshake_lock: Monitor::new(RefCell::new(Vec::new())),
        }
    }

    pub(crate) fn add_thread(&self, thread: Arc<Thread<VM>>) {
        self.process_about_to_terminate();
        parked_scope::<_, VM>(|| {
            let inner = self.inner.lock_no_handshake();
            let mut inner = inner.borrow_mut();

            let idx = inner
                .free_thread_indices
                .pop()
                .unwrap_or(inner.threads.len());
            thread
                .index_in_manager
                .store(idx, atomic::Ordering::Relaxed);
            if idx >= inner.threads.len() {
                inner.threads.push(Some(thread));
            } else {
                inner.threads[idx] = Some(thread);
            }
        });
    }

    pub(crate) fn add_main_thread(&self, thread: Arc<Thread<VM>>) {
        // no need for parked scope, there's no VM activity yet.
        let inner = self.inner.lock_no_handshake();
        let mut inner = inner.borrow_mut();
        assert!(inner.threads.is_empty());
        assert!(inner.free_thread_indices.is_empty());
        assert!(inner.about_to_terminate.is_empty());
        thread.index_in_manager.store(0, atomic::Ordering::Relaxed);
        inner.threads.push(Some(thread));
    }

    pub(crate) fn add_about_to_terminate_current(&self) {
        let inner = self.inner.lock_no_handshake();
        let mut inner = inner.borrow_mut();
        let index = Thread::<VM>::current()
            .index_in_manager
            .load(atomic::Ordering::Relaxed);
        // do not remove thread from threads vector, GC may still
        // scan some of thread state (apart from stack).
        let thread_rc = inner.threads[index].clone().unwrap();
        inner.about_to_terminate.push(thread_rc);
        deinit_current_thread::<VM>();
    }

    pub(crate) fn process_about_to_terminate(&self) {
        'restart: loop {
            let lock = self.inner.lock_no_handshake();
            let mut lock = lock.borrow_mut();
            for i in 0..lock.about_to_terminate.len() {
                let t = lock.threads[i].clone().unwrap();
                if t.get_exec_status() == ThreadState::Terminated {
                    lock.about_to_terminate.swap_remove(i);
                    let index = t.index_in_manager.load(atomic::Ordering::Relaxed);
                    lock.free_thread_indices.push(index);
                    lock.threads[index] = None;

                    drop(lock);
                    continue 'restart;
                }
            }

            drop(lock);

            break;
        }
    }

    pub fn threads(&self) -> impl Iterator<Item = Arc<Thread<VM>>> {
        let inner = self.inner.lock_no_handshake();
        let inner = inner.borrow();
        inner
            .threads
            .clone()
            .into_iter()
            .flat_map(|t| t.into_iter())
    }

    /// Stop all mutator threads. This is currently intended to be run by a single thread.
    ///
    /// Fixpoint until there are no threads that we haven't blocked. Fixpoint is needed to
    /// catch the (unlikely) case that a thread spawns another thread while we are waiting.
    pub fn block_all_mutators_for_gc(&self) -> Vec<Arc<Thread<VM>>> {
        let mut handshake_threads = Vec::with_capacity(4);
        loop {
            let lock = self.inner.lock_no_handshake();
            let lock = lock.borrow();
            // (1) find all threads that need to be blocked for GC

            for i in 0..lock.threads.len() {
                if let Some(t) = lock.threads[i].clone() {
                    if !t.is_collector_thread() && !t.ignore_handshakes_and_gc() {
                        handshake_threads.push(t.clone());
                    }
                }
            }

            drop(lock);
            // (2) Remove any threads that have already been blocked from the list.
            handshake_threads.retain(|t| {
                let lock = t.monitor().lock_no_handshake();
                if t.is_blocked_for::<GCBlockAdapter>()
                    || t.block_unchecked::<GCBlockAdapter>(true).not_running()
                {
                    drop(lock);
                    false
                } else {
                    drop(lock);
                    true
                }
            });

            // (3) Quit trying to block threads if all threads are either blocked
            //     or not running (a thread is "not running" if it is NEW or TERMINATED;
            //     in the former case it means that the thread has not had start()
            //     called on it while in the latter case it means that the thread
            //     is either in the TERMINATED state or is about to be in that state
            //     real soon now, and will not perform any heap-related work before
            //     terminating).
            if handshake_threads.is_empty() {
                break;
            }
            // (4) Request a block for GC from all other threads.
            while let Some(thread) = handshake_threads.pop() {
                let lock = thread.monitor().lock_no_handshake();
                thread.block_unchecked::<GCBlockAdapter>(false);
                drop(lock);
            }
        }
        // Deal with terminating threads to ensure that all threads are either dead to MMTk or stopped above.
        self.process_about_to_terminate();

        self.inner
            .lock_no_handshake()
            .borrow()
            .threads
            .iter()
            .flatten()
            .filter(|t| t.is_blocked_for::<GCBlockAdapter>())
            .cloned()
            .collect::<Vec<_>>()
    }

    /// Unblock all mutators blocked for GC.
    pub fn unblock_all_mutators_for_gc(&self) {
        let mut handshake_threads = Vec::with_capacity(4);
        let lock = self.inner.lock_no_handshake();
        let lock = lock.borrow();

        for thread in lock.threads.iter() {
            if let Some(thread) = thread {
                if !thread.is_collector_thread() {
                    handshake_threads.push(thread.clone());
                }
            }
        }

        drop(lock);

        while let Some(thread) = handshake_threads.pop() {
            let lock = thread.monitor().lock_no_handshake();
            thread.unblock::<GCBlockAdapter>();
            drop(lock);
        }
    }
}

pub trait BlockAdapter<VM: VirtualMachine> {
    type Token: Copy + PartialEq;
    fn is_blocked(thread: &Thread<VM>) -> bool;
    fn set_blocked(thread: &Thread<VM>, value: bool);

    fn request_block(thread: &Thread<VM>) -> Self::Token;
    fn has_block_request(thread: &Thread<VM>) -> bool;
    fn has_block_request_with_token(thread: &Thread<VM>, token: Self::Token) -> bool;
    fn clear_block_request(thread: &Thread<VM>);
}

pub struct GCBlockAdapter;

impl<VM: VirtualMachine> BlockAdapter<VM> for GCBlockAdapter {
    type Token = bool;

    fn is_blocked(thread: &Thread<VM>) -> bool {
        thread.is_blocked_for_gc.load(atomic::Ordering::Relaxed)
    }

    fn set_blocked(thread: &Thread<VM>, value: bool) {
        thread
            .is_blocked_for_gc
            .store(value, atomic::Ordering::Relaxed);
    }

    fn request_block(thread: &Thread<VM>) -> Self::Token {
        thread
            .should_block_for_gc
            .store(true, atomic::Ordering::Relaxed);
        true
    }

    fn has_block_request(thread: &Thread<VM>) -> bool {
        thread.should_block_for_gc.load(atomic::Ordering::Relaxed)
    }

    fn has_block_request_with_token(thread: &Thread<VM>, token: Self::Token) -> bool {
        let _ = token;
        thread.should_block_for_gc.load(atomic::Ordering::Relaxed)
    }

    fn clear_block_request(thread: &Thread<VM>) {
        thread
            .should_block_for_gc
            .store(false, atomic::Ordering::Relaxed);
    }
}

pub trait BlockAdapterList<VM: VirtualMachine>: Sized {
    fn acknowledge_block_requests(thread: &Thread<VM>) -> bool;
    fn is_blocked(thread: &Thread<VM>) -> bool;
}

macro_rules! block_adapter_list {
    ($(($($t: ident),*))*) => {
        $(
            impl<VM: VirtualMachine, $($t: BlockAdapter<VM>),*> BlockAdapterList<VM> for ($($t),*) {
                fn acknowledge_block_requests(thread: &Thread<VM>) -> bool {
                    let mut had_some = false;
                    $(
                        if $t::has_block_request(thread) {
                            $t::set_blocked(thread, true);
                            $t::clear_block_request(thread);
                            had_some = true;
                        }
                    )*

                    had_some
                }

                fn is_blocked(thread: &Thread<VM>) -> bool {
                    let mut is_blocked = false;

                    $(
                        is_blocked |= $t::is_blocked(thread);
                    )*

                    is_blocked
                }
            }

        )*
    };
}

impl<VM: VirtualMachine> BlockAdapter<VM> for () {
    type Token = ();
    fn clear_block_request(thread: &Thread<VM>) {
        let _ = thread;
    }

    fn has_block_request(thread: &Thread<VM>) -> bool {
        let _ = thread;
        false
    }

    fn has_block_request_with_token(thread: &Thread<VM>, token: Self::Token) -> bool {
        let _ = thread;
        let _ = token;
        false
    }

    fn is_blocked(thread: &Thread<VM>) -> bool {
        let _ = thread;
        false
    }

    fn request_block(thread: &Thread<VM>) -> Self::Token {
        let _ = thread;
        ()
    }

    fn set_blocked(thread: &Thread<VM>, value: bool) {
        let _ = thread;
        let _ = value;
    }
}

block_adapter_list!((X0, X1)(X0, X1, X2)(X0, X1, X2, X3)(X0, X1, X2, X3, X4)(
    X0, X1, X2, X3, X4, X5
)(X0, X1, X2, X3, X4, X5, X6)(X0, X1, X2, X3, X4, X5, X6, X7)(
    X0, X1, X2, X3, X4, X5, X6, X7, X8
)(X0, X1, X2, X3, X4, X5, X6, X7, X8, X9)(
    X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10
)(X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11)(
    X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11, X12
)(X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11, X12, X13)(
    X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11, X12, X13, X14
)(
    X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11, X12, X13, X14, X15
)(
    X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11, X12, X13, X14, X15, X16
)(
    X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11, X12, X13, X14, X15, X16, X17
)(
    X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11, X12, X13, X14, X15, X16, X17, X18
)(
    X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11, X12, X13, X14, X15, X16, X17, X18, X19
)(
    X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11, X12, X13, X14, X15, X16, X17, X18, X19, X20
)(
    X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11, X12, X13, X14, X15, X16, X17, X18, X19, X20,
    X21
)(
    X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11, X12, X13, X14, X15, X16, X17, X18, X19, X20,
    X21, X22
)(
    X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11, X12, X13, X14, X15, X16, X17, X18, X19, X20,
    X21, X22, X23
)(
    X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11, X12, X13, X14, X15, X16, X17, X18, X19, X20,
    X21, X22, X23, X24
)(
    X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11, X12, X13, X14, X15, X16, X17, X18, X19, X20,
    X21, X22, X23, X24, X25
));

/// Execute the given function in a parked scope.
///
/// Parked scope puts current thread to `InNative` state and then puts it back to `InManaged` state after the function returns.
/// Code inside `f` must not access any managed objects nor allocate any objects.
pub fn parked_scope<R, VM: VirtualMachine>(f: impl FnOnce() -> R) -> R {
    Thread::<VM>::enter_native();
    let result = f();
    Thread::<VM>::leave_native();
    result
}

/// Provides a skeleton implementation for use in soft handshakes.
/// <p>
/// During a soft handshake, the requesting thread waits for all mutator threads
/// (i.e. non-gc threads) to perform a requested action.
pub trait SoftHandshakeVisitor<VM: VirtualMachine> {
    /// Sets whatever flags need to be set to signal that the given thread should
    /// perform some action when it acknowledges the soft handshake.
    /// <p>
    /// This method is only called for threads for which {@link #includeThread(RVMThread)}
    /// is {@code true}.
    /// <p>
    /// This method is called with the thread's monitor held, but while the
    /// thread may still be running. This method is not called on mutators that
    /// have indicated that they are about to terminate.
    ///
    /// # Returns
    /// `false` if not interested in this thread, `true` otherwise.
    /// Returning `true` will cause a soft handshake request to be put through.
    fn check_and_signal(&self, t: &Arc<Thread<VM>>) -> bool;

    /// Called when it is determined that the thread is stuck in native. While
    /// this method is being called, the thread cannot return to running managed
    /// code. As such, it is safe to perform actions "on the thread's behalf".
    /// <p>
    /// This implementation does nothing.
    fn notify_stuck_in_native(&self, t: &Arc<Thread<VM>>) {
        let _ = t;
    }

    /// Checks whether to include the specified thread in the soft handshake.
    /// <p>
    /// This method will never see any threads from the garbage collector because
    /// those are excluded from the soft handshake by design.
    /// <p>
    /// This implementation always returns `true`.
    ///
    /// # Parameters
    /// * `t` - The thread to check for inclusion
    ///
    /// # Returns
    /// `true` if the thread should be included.
    fn include_thread(&self, t: &Arc<Thread<VM>>) -> bool {
        let _ = t;
        true
    }
}

impl<VM: VirtualMachine, F: Fn(&Arc<Thread<VM>>) -> bool> SoftHandshakeVisitor<VM> for F {
    fn check_and_signal(&self, t: &Arc<Thread<VM>>) -> bool {
        self(t)
    }
}
