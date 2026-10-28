//! Graceful interruption.
//!
//! The flag is a process-wide atomic so the handler itself does nothing but a
//! single relaxed store — the only thing that is unconditionally safe inside a
//! signal handler (POSIX) or a console control handler (Windows).
//!
//! The pipeline polls it between chunks, stops submitting, drains what is in
//! flight, truncates the half-written file and reports what it managed to do. A
//! second interrupt restores the default behaviour and kills the process, because
//! a stuck I/O should not be un-killable.

use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

/// Set once the user asked us to stop.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);
/// Set on the second interrupt: the next signal terminates the process.
static HARD: AtomicBool = AtomicBool::new(false);

/// The process-wide stop flag.
///
/// Every pipeline thread polls this; the CLI and the GUI set it from their
/// signal handler or Stop button. Shared rather than passed down so a signal
/// handler, which cannot capture anything, can reach it.
#[inline]
#[must_use]
pub fn flag() -> &'static AtomicBool {
    &INTERRUPTED
}

/// True once a stop was requested.
#[inline]
pub fn is_set() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

/// Clear both flags before starting a new job.
pub fn reset() {
    INTERRUPTED.store(false, Ordering::Relaxed);
    HARD.store(false, Ordering::Relaxed);
}

/// Install handlers for `SIGINT`/`SIGTERM` (POSIX) or `Ctrl-C`/`Ctrl-Break`
/// (Windows). Safe to call more than once.
///
/// The first interrupt sets [`flag`] so the pipeline can wind down and keep what
/// it has already written; a second one terminates the process.
///
/// # Errors
///
/// Returns the OS error when the handler could not be installed.
pub fn install() -> io::Result<()> {
    imp::install()
}

#[cfg(unix)]
mod imp {
    use super::INTERRUPTED;
    use super::Ordering;
    use std::io;

    extern "C" fn handler(_sig: libc::c_int) {
        // Only async-signal-safe operations here: one atomic store.
        if INTERRUPTED.swap(true, Ordering::Relaxed) {
            // Second interrupt: restore the default action and re-raise so the
            // process dies immediately instead of draining.
            unsafe {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGTERM, libc::SIG_DFL);
                libc::raise(libc::SIGINT);
            }
        }
    }

    pub fn install() -> io::Result<()> {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = handler as *const () as usize;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(std::ptr::addr_of_mut!(sa.sa_mask));
            for sig in [libc::SIGINT, libc::SIGTERM] {
                if libc::sigaction(sig, std::ptr::addr_of!(sa), std::ptr::null_mut()) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            // A closed stdout should not kill a long copy.
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        }
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use super::INTERRUPTED;
    use super::Ordering;
    use std::io;

    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;

    unsafe extern "system" fn handler(ctrl: u32) -> i32 {
        if ctrl == CTRL_C_EVENT || ctrl == CTRL_BREAK_EVENT {
            if INTERRUPTED.swap(true, Ordering::Relaxed) {
                // Second press: let the default handler terminate the process.
                return 0;
            }
            return 1; // handled: we will unwind gracefully
        }
        0
    }

    pub fn install() -> io::Result<()> {
        let ok = unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }
}

/// Convenience: a plain `bool` that also records the "hard" state for reporting.
pub fn hard_requested() -> bool {
    HARD.load(Ordering::Relaxed)
}
