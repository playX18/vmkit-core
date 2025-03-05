cfgenius::cond! {

    if macro(crate::macros::darwin) {
        cfgenius::cond! {
            if cfg(target_os="x86_64") {
                pub type PlatformRegisters = libc::__darwin_x86_thread_state64;
            } else if cfg!(target_os="arm64") {
                pub type PlatformRegisters = libc::__darwin_arm_thread_state64;
            } else {
                compile_error!("Unsupported Apple target");
            }
        }

        pub unsafe fn registers_from_ucontext(ucontext: *const libc::ucontext_t) -> *const PlatformRegisters {
            return &(*ucontext).uc_mcontext.__ss;
        }


    } else if macro(crate::macros::have_machine_context) {

        #[cfg(not(target_os="openbsd"))]
        use libc::mcontext_t;
        #[cfg(target_os="openbsd")]
        use libc::ucontext_t as mcontext_t;

        #[repr(C)]
        #[derive(Clone)]
        pub struct PlatformRegisters {
            pub machine_context: mcontext_t
        }

        pub unsafe fn registers_from_ucontext(ucontext: *const libc::ucontext_t) -> *const PlatformRegisters {
            cfgenius::cond! {
                if cfg(target_os="openbsd")
                {
                    return ucontext.cast();
                }
                else if cfg(target_arch="powerpc")
                {
                    return unsafe { std::mem::transmute(
                        &(*ucontext).uc_mcontext.uc_regs
                    ) }
                } else {
                    return unsafe { std::mem::transmute(
                        &(*ucontext).uc_mcontext
                    ) }
                }

            }
        }


    } else if cfg(windows) {
        use winapi::um::winnt::CONTEXT;

        pub type PlatformRegisters = CONTEXT;
    } else {
        pub struct PlatformRegisters {
            pub stack_pointer: *mut u8
        }
    }
}
