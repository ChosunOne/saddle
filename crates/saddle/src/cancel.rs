use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

pub static FLAG: AtomicBool = AtomicBool::new(false);

pub fn request() {
    FLAG.store(true, Ordering::SeqCst);
}

pub fn requested() -> bool {
    FLAG.load(Ordering::SeqCst)
}

pub fn reset() {
    FLAG.store(false, Ordering::SeqCst);
}

pub struct SigintGuard {
    previous: libc::sigaction,
}

extern "C" fn on_sigint(_: libc::c_int) {
    if FLAG.swap(true, Ordering::SeqCst) {
        unsafe { libc::_exit(130) }
    }
}

impl SigintGuard {
    pub fn install() -> Self {
        unsafe {
            let mut action: libc::sigaction = mem::zeroed();
            action.sa_sigaction = on_sigint as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            let mut previous: libc::sigaction = mem::zeroed();
            libc::sigaction(libc::SIGINT, &action, &mut previous);
            Self { previous }
        }
    }
}

impl Drop for SigintGuard {
    fn drop(&mut self) {
        unsafe {
            libc::sigaction(libc::SIGINT, &self.previous, ptr::null_mut());
        }
    }
}
