use std::ptr::null_mut;

use libc::pthread_attr_destroy;
use mmtk::util::Address;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StackBounds {
    origin: Address,
    bound: Address,
}

impl StackBounds {
    // Default reserved zone size
    pub const DEFAULT_RESERVED_ZONE: usize = 64 * 1024;

    pub const fn empty_bounds() -> Self {
        Self {
            origin: Address::ZERO,
            bound: Address::ZERO,
        }
    }

    pub fn current_thread_stack_bounds() -> Self {
        let result = Self::current_thread_stack_bounds_internal();
        result.check_consistency();
        result
    }

    pub fn origin(&self) -> Address {
        debug_assert!(!self.origin.is_zero());
        self.origin
    }

    pub fn end(&self) -> Address {
        debug_assert!(!self.bound.is_zero());
        self.bound
    }

    pub fn size(&self) -> usize {
        self.origin - self.bound
    }

    pub fn is_empty(&self) -> bool {
        self.origin.is_zero()
    }

    pub fn contains(&self, p: Address) -> bool {
        if self.is_empty() {
            return false;
        }
        self.origin >= p && p > self.bound
    }

    pub fn recursion_limit(&self, min_reserved_zone: usize) -> Address {
        self.check_consistency();
        self.bound + min_reserved_zone
    }

    pub fn recursion_limit_with_params(
        &self,
        start_of_user_stack: Address,
        max_user_stack: usize,
        reserved_zone_size: usize,
    ) -> Address {
        self.check_consistency();

        let reserved_zone_size = reserved_zone_size.min(max_user_stack);
        let mut max_user_stack_with_reserved_zone = max_user_stack - reserved_zone_size;

        let end_of_stack_with_reserved_zone = self.bound.add(reserved_zone_size);
        if start_of_user_stack < end_of_stack_with_reserved_zone {
            return end_of_stack_with_reserved_zone;
        }

        let available_user_stack =
            start_of_user_stack.get_offset(end_of_stack_with_reserved_zone) as usize;
        if max_user_stack_with_reserved_zone > available_user_stack {
            max_user_stack_with_reserved_zone = available_user_stack;
        }

        start_of_user_stack - max_user_stack_with_reserved_zone
    }

    pub fn with_soft_origin(&self, origin: Address) -> Self {
        assert!(self.contains(origin));
        Self {
            origin,
            bound: self.bound,
        }
    }
    fn check_consistency(&self) {
        #[cfg(debug_assertions)]
        {
            let current_position = current_stack_pointer();
            assert_ne!(self.origin, self.bound);
            assert!(current_position < self.origin && current_position > self.bound);
        }
    }

    pub fn is_growing_downwards(&self) -> bool {
        assert!(!self.origin.is_zero() && !self.bound.is_zero());
        self.bound <= self.origin
    }
}

#[cfg(unix)]
impl StackBounds {
    pub unsafe fn new_thread_stack_bounds(handle: libc::pthread_t) -> Self {
        #[cfg(target_vendor = "apple")]
        unsafe {
            let origin = libc::pthread_get_stackaddr_np(handle);
            let size = libc::pthread_get_stacksize_np(handle);
            let bound = origin.byte_sub(size);
            Self {
                origin: Address::from_ptr(origin),
                bound: Address::from_ptr(bound),
            }
        }

        #[cfg(target_os = "openbsd")]
        {
            let mut stack: std::mem::MaybeUninit<libc::stack_t> = std::mem::MaybeUninit::zeroed();
            unsafe {
                libc::pthread_stackseg_np(handle, stack.as_mut_ptr() as _);
            }
            let stack = unsafe { stack.assume_init() };
            let origin = Address::from_ptr(stack.ss_sp);
            let bound = origin - stack.ss_size as usize;
            Self { origin, bound }
        }

        #[cfg(all(unix, not(target_vendor = "apple"), not(target_os = "openbsd")))]
        {
            let mut bound = null_mut();
            let mut stack_size = 0;

            let mut sattr: std::mem::MaybeUninit<libc::pthread_attr_t> =
                std::mem::MaybeUninit::zeroed();

            unsafe {
                libc::pthread_attr_init(sattr.as_mut_ptr() as _);
                #[cfg(target_os = "netbsd")]
                {
                    libc::pthread_attr_get_np(handle, sattr.as_mut_ptr() as _);
                }
                #[cfg(not(target_os = "netbsd"))]
                {
                    libc::pthread_getattr_np(handle, sattr.as_mut_ptr() as _);
                }
                libc::pthread_attr_getstack(sattr.assume_init_mut(), &mut bound, &mut stack_size);
                pthread_attr_destroy(sattr.assume_init_mut());
                let bound = Address::from_ptr(bound);
                let origin = bound + stack_size;
                Self { origin, bound }
            }
        }
    }

    fn current_thread_stack_bounds_internal() -> Self {
        let ret = unsafe { Self::new_thread_stack_bounds(libc::pthread_self()) };

        /*#[cfg(target_os = "linux")]
        unsafe {
            // on glibc, pthread_attr_getstack will generally return the limit size (minus a guard page)
            // for the main thread; this is however not necessarily always true on every libc - for example
            // on musl, it will return the currently reserved size - since the stack bounds are expected to
            // be constant (and they are for every thread except main, which is allowed to grow), check
            // resource limits and use that as the boundary instead (and prevent stack overflows).
            if libc::getpid() == libc::syscall(libc::SYS_gettid) as libc::pid_t {
                let origin = ret.origin();
                let mut limit: std::mem::MaybeUninit<libc::rlimit> =
                    std::mem::MaybeUninit::zeroed();
                libc::getrlimit(libc::RLIMIT_STACK, limit.as_mut_ptr() as _);

                let limit = limit.assume_init();
                let mut size = limit.rlim_cur;
                if size == libc::RLIM_INFINITY {
                    size = 8 * 1024 * 1024;
                }
                size -= libc::sysconf(libc::_SC_PAGE_SIZE) as u64;
                let bound = origin - size as usize;

                return Self { origin, bound };
            }
        }*/

        ret
    }
}

#[inline(never)]
pub fn current_stack_pointer() -> Address {
    #[cfg(target_arch = "x86_64")]
    {
        let current_sp: usize;
        unsafe {
            std::arch::asm!(
                "mov rax, rsp",
                out("rax") current_sp,
            );
            Address::from_usize(current_sp)
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        let mut current_sp = Address::ZERO;
        current_sp = Address::from_ptr(&current_sp);
        current_sp
    }
}

#[cfg(windows)]
impl StackBounds {
    fn current_thread_stack_bounds_internal() -> Self {
        use std::mem::*;
        use winapi::um::winnt::*;
        use winapi::um::memoryapi::*;
        unsafe {
            let mut stack_origin: MaybeUninit<MEMORY_BASIC_INFORMATION> = MaybeUninit::uninit();

            VirtualQuery(
                stack_origin.as_mut_ptr(),
                stack_origin.as_mut_ptr(),
                size_of::<MEMORY_BASIC_INFORMATION>(),
            );

            let stack_origin = stack_origin.assume_init();
            let origin =
                Address::from_ptr(stack_origin.BaseAddress) + stack_origin.RegionSize as usize;
            // The stack on Windows consists out of three parts (uncommitted memory, a guard page and present
            // committed memory). The 3 regions have different BaseAddresses but all have the same AllocationBase
            // since they are all from the same VirtualAlloc. The 3 regions are laid out in memory (from high to
            // low) as follows:
            //
            //    High |-------------------|  -----
            //         | committedMemory   |    ^
            //         |-------------------|    |
            //         | guardPage         | reserved memory for the stack
            //         |-------------------|    |
            //         | uncommittedMemory |    v
            //    Low  |-------------------|  ----- <--- stackOrigin.AllocationBase
            //
            // See http://msdn.microsoft.com/en-us/library/ms686774%28VS.85%29.aspx for more information.

            let mut uncommited_memory: MaybeUninit<MEMORY_BASIC_INFORMATION> = MaybeUninit::uninit();
            VirtualQuery(
                stack_origin.AllocationBase as _,
                uncommited_memory.as_mut_ptr(),
                size_of::<MEMORY_BASIC_INFORMATION>(),
            );
            let uncommited_memory = uncommited_memory.assume_init();
            assert_eq!(uncommited_memory.State, MEM_RESERVE);
            
            let mut guard_page: MaybeUninit<MEMORY_BASIC_INFORMATION> = MaybeUninit::uninit();
            VirtualQuery(
                (uncommited_memory.BaseAddress as usize + uncommited_memory.RegionSize as usize) as _,
                guard_page.as_mut_ptr(),
                size_of::<MEMORY_BASIC_INFORMATION>(),
            );

            let guard_page = guard_page.assume_init();
            assert!(guard_page.Protect & PAGE_GUARD != 0);

            let end_of_stack = Address::from_ptr(stack_origin.AllocationBase);

            let bound = end_of_stack + guard_page.RegionSize as usize;

            Self { origin, bound }
        }
    }
}
