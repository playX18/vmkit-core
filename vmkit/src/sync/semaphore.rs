pub struct Semaphore {
    platform_sema: libc::sem_t,
}

impl Semaphore {
    pub fn wait(&self) {
        unsafe {
            if libc::sem_wait(&self.platform_sema as *const _ as *mut _) != 0 {
                panic!("sem_wait failed:{}", errno::errno());
            }
        }
    }

    pub fn post(&self) {
        unsafe {
            if libc::sem_post(&self.platform_sema as *const _ as *mut _) != 0 {
                panic!("sem_post failed:{}", errno::errno());
            }
        }
    }

    pub fn new(initial_value: usize) -> Self {
        let mut sema = std::mem::MaybeUninit::uninit();
        unsafe {
            libc::sem_init(sema.as_mut_ptr(), 0, initial_value as u32);
        }

        Self {
            platform_sema: unsafe { sema.assume_init() },
        }
    }
}

impl Drop for Semaphore {
    fn drop(&mut self) {
        unsafe {
            libc::sem_destroy(&self.platform_sema as *const _ as *mut _);
        }
    }
}
