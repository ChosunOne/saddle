//! Cancelling a turn. There is one flag per process. The TUI sets it on Esc or Ctrl-C, and in line
//! mode a SIGINT handler sets it. The model stream, the loop and a running command all check it.

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

/// While the guard lives, the first SIGINT cancels the turn and a second one exits with 130.
pub struct SigintGuard {
    previous: libc::sigaction,
}

extern "C" fn on_sigint(_: libc::c_int) {
    // An atomic swap and _exit are both async-signal-safe.
    if FLAG.swap(true, Ordering::SeqCst) {
        unsafe { libc::_exit(130) }
    }
}

impl SigintGuard {
    pub fn install() -> Self {
        // SAFETY: a zeroed sigaction is valid; the handler only touches an atomic and calls _exit.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_sigint as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            let mut previous: libc::sigaction = std::mem::zeroed();
            libc::sigaction(libc::SIGINT, &action, &mut previous);
            Self { previous }
        }
    }
}

impl Drop for SigintGuard {
    fn drop(&mut self) {
        // SAFETY: restores the handler saved in install.
        unsafe {
            libc::sigaction(libc::SIGINT, &self.previous, std::ptr::null_mut());
        }
    }
}
