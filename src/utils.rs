use std::sync::atomic::{AtomicBool, Ordering};

static SIGINT_FLAG: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
pub struct SigintHandler {}

impl SigintHandler {
    pub fn new() -> Self {
        SIGINT_FLAG.store(false, Ordering::Relaxed);

        extern "C" fn handler(_: libc::c_int) {
            SIGINT_FLAG.store(true, Ordering::Relaxed);
        }

        unsafe {
            libc::signal(libc::SIGINT, handler as *const () as libc::size_t);
        }
        SigintHandler {}
    }

    pub fn was_triggered(&self) -> bool {
        SIGINT_FLAG.load(Ordering::Relaxed)
    }
}
