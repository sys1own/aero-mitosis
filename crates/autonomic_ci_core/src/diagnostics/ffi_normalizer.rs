//! Hardware-level crash supervisor for FFI and native code boundaries.
//!
//! On Unix systems this module installs `sigaction` signal handlers on an
//! alternate signal stack (`sigaltstack`) so that crashes such as `SIGSEGV`,
//! `SIGBUS`, `SIGILL`, `SIGFPE`, and `SIGABRT` can be intercepted even when the
//! normal thread stack is corrupted.
//!
//! The captured backtrace is serialized into a unified `NormalizedCrashPayload`
//! JSON schema that can be consumed by downstream causal analysis.

use std::backtrace::Backtrace;
use std::ffi::c_void;
use std::fmt;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// Unified JSON schema for a normalized crash event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedCrashPayload {
    pub signal: String,
    pub signal_number: i32,
    pub fault_address: Option<String>,
    pub backtrace_frames: Vec<String>,
    pub thread_id: u64,
    pub timestamp_seconds: u64,
}

impl fmt::Display for NormalizedCrashPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", serde_json::to_string(self).unwrap_or_default())
    }
}

static CRASH: OnceLock<Mutex<Option<NormalizedCrashPayload>>> = OnceLock::new();
static SHOULD_EXIT: AtomicBool = AtomicBool::new(true);

/// Supervisor that registers OS-level signal handlers.
pub struct SignalSupervisor {
    #[cfg(unix)]
    old_actions: Vec<(i32, libc::sigaction)>,
    #[cfg(unix)]
    alt_stack: Vec<u8>,
}

impl SignalSupervisor {
    /// Install signal handlers for fatal crashes.
    pub fn new() -> Self {
        Self::with_exit(true)
    }

    /// Install signal handlers. If `exit_on_crash` is `false` the handlers will
    /// return instead of calling `_exit`, which is useful for testing with
    /// non-fatal signals.
    pub fn with_exit(exit_on_crash: bool) -> Self {
        SHOULD_EXIT.store(exit_on_crash, Ordering::Relaxed);
        let _ = CRASH.get_or_init(|| Mutex::new(None));

        #[cfg(unix)]
        {
            let mut supervisor = Self {
                old_actions: Vec::new(),
                alt_stack: Vec::new(),
            };
            supervisor.install();
            return supervisor;
        }

        #[cfg(not(unix))]
        {
            Self {}
        }
    }

    /// Serialize a payload to JSON.
    pub fn to_json(payload: &NormalizedCrashPayload) -> String {
        serde_json::to_string(payload).unwrap_or_default()
    }

    /// Return the most recently captured crash, if any.
    pub fn last_crash() -> Option<NormalizedCrashPayload> {
        CRASH
            .get()
            .and_then(|m| m.lock().ok())
            .and_then(|g| g.clone())
    }

    /// Clear any stored crash payload.
    pub fn clear() {
        if let Some(m) = CRASH.get() {
            if let Ok(mut g) = m.lock() {
                *g = None;
            }
        }
    }

    #[cfg(unix)]
    fn install(&mut self) {
        use libc::{sigaction, sigaltstack, sigemptyset, stack_t};

        let stack_size = libc::SIGSTKSZ.max(128 * 1024);
        self.alt_stack = vec![0u8; stack_size];

        let ss = stack_t {
            ss_sp: self.alt_stack.as_mut_ptr().cast::<c_void>(),
            ss_flags: 0,
            ss_size: stack_size,
        };
        let mut old_ss: stack_t = unsafe { std::mem::zeroed() };
        unsafe { sigaltstack(&ss, &mut old_ss) };

        for &signal in &[
            libc::SIGSEGV,
            libc::SIGBUS,
            libc::SIGILL,
            libc::SIGFPE,
            libc::SIGABRT,
            libc::SIGUSR1,
            libc::SIGUSR2,
        ] {
            let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
            sa.sa_sigaction = crash_handler as *const () as usize;
            unsafe { sigemptyset(&mut sa.sa_mask) };
            sa.sa_flags = libc::SA_ONSTACK | libc::SA_SIGINFO;

            let mut old_sa: libc::sigaction = unsafe { std::mem::zeroed() };
            let rc = unsafe { sigaction(signal, &sa, &mut old_sa) };
            if rc == 0 {
                self.old_actions.push((signal, old_sa));
            }
        }

        // Prevent the compiler from dropping `old_ss` too early.
        let _ = old_ss;
    }

    #[cfg(not(unix))]
    fn install(&mut self) {
        // No-op on non-Unix platforms.
    }
}

impl Default for SignalSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(unix)]
impl Drop for SignalSupervisor {
    fn drop(&mut self) {
        for (signal, old_action) in &self.old_actions {
            unsafe {
                libc::sigaction(*signal, old_action, ptr::null_mut());
            }
        }

        let disable = libc::stack_t {
            ss_sp: ptr::null_mut(),
            ss_flags: libc::SS_DISABLE,
            ss_size: 0,
        };
        unsafe {
            libc::sigaltstack(&disable, ptr::null_mut());
        }
    }
}

#[cfg(unix)]
unsafe extern "C" fn crash_handler(signum: i32, info: *mut libc::siginfo_t, _context: *mut c_void) {
    // Capturing a full backtrace inside a signal handler is not strictly
    // async-signal-safe, but it is attempted for fatal crashes. In non-fatal
    // test mode the backtrace is left empty so the handler can return cleanly.
    let frames = if SHOULD_EXIT.load(Ordering::Relaxed) {
        let bt = Backtrace::force_capture();
        bt.to_string().lines().map(|s| s.to_string()).collect()
    } else {
        Vec::new()
    };

    let fault_address = if !info.is_null() {
        let addr = (*info).si_addr();
        if addr.is_null() {
            None
        } else {
            Some(format!("{:p}", addr))
        }
    } else {
        None
    };

    let payload = NormalizedCrashPayload {
        signal: signal_name(signum),
        signal_number: signum,
        fault_address,
        backtrace_frames: frames,
        thread_id: libc::pthread_self() as u64,
        timestamp_seconds: libc::time(ptr::null_mut()) as u64,
    };

    if let Some(lock) = CRASH.get() {
        if let Ok(mut guard) = lock.try_lock() {
            *guard = Some(payload);
        }
    }

    if SHOULD_EXIT.load(Ordering::Relaxed) {
        libc::_exit(signum);
    }
}

#[cfg(unix)]
fn signal_name(signum: i32) -> String {
    match signum {
        libc::SIGSEGV => "SIGSEGV".into(),
        libc::SIGBUS => "SIGBUS".into(),
        libc::SIGILL => "SIGILL".into(),
        libc::SIGFPE => "SIGFPE".into(),
        libc::SIGABRT => "SIGABRT".into(),
        libc::SIGUSR1 => "SIGUSR1".into(),
        libc::SIGUSR2 => "SIGUSR2".into(),
        _ => format!("SIG{signum}"),
    }
}

#[cfg(not(unix))]
fn signal_name(_signum: i32) -> String {
    "UNKNOWN".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn captures_usr1_as_non_fatal_crash() {
        SignalSupervisor::clear();
        let _supervisor = SignalSupervisor::with_exit(false);

        unsafe {
            libc::raise(libc::SIGUSR1);
        }

        let crash = SignalSupervisor::last_crash().expect("expected a captured crash");
        assert_eq!(crash.signal, "SIGUSR1");
        assert!(SignalSupervisor::to_json(&crash).contains("SIGUSR1"));
    }
}
