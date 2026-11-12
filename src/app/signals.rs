//! Scoped ownership of the interactive session's HUP and TERM dispositions.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use anyhow::{Result, bail};

const HANGUP: u8 = 1;
const TERMINATE: u8 = 2;
static PENDING: AtomicU8 = AtomicU8::new(0);
static LEASED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionSignal {
    Hangup,
    Terminate,
}

pub(super) struct SessionSignals {
    previous_hup: libc::sigaction,
    previous_term: libc::sigaction,
}

impl SessionSignals {
    pub(super) fn install() -> Result<Self> {
        if LEASED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            bail!("another interactive session already owns the signal handlers");
        }
        PENDING.store(0, Ordering::Relaxed);
        // sigaction initializes the saved actions. An empty mask leaves unrelated
        // signals alone; SA_RESTART prevents the PTY reader treating EINTR as EOF.
        let result = unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = record as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            let mut previous_hup = std::mem::zeroed();
            let mut previous_term = std::mem::zeroed();
            if libc::sigaction(libc::SIGHUP, &action, &mut previous_hup) != 0 {
                Err(std::io::Error::last_os_error())
            } else if libc::sigaction(libc::SIGTERM, &action, &mut previous_term) != 0 {
                let error = std::io::Error::last_os_error();
                libc::sigaction(libc::SIGHUP, &previous_hup, std::ptr::null_mut());
                Err(error)
            } else {
                Ok(Self {
                    previous_hup,
                    previous_term,
                })
            }
        };
        if result.is_err() {
            LEASED.store(false, Ordering::Release);
        }
        result
            .map_err(|error| anyhow::anyhow!("could not install session signal handlers: {error}"))
    }

    pub(super) fn received(&self) -> Option<SessionSignal> {
        let pending = PENDING.load(Ordering::Relaxed);
        if pending & HANGUP != 0 {
            Some(SessionSignal::Hangup)
        } else if pending & TERMINATE != 0 {
            Some(SessionSignal::Terminate)
        } else {
            None
        }
    }
}

extern "C" fn record(signal: libc::c_int) {
    // AtomicU8 is lock-free on the supported macOS and Linux targets.
    PENDING.fetch_or(
        if signal == libc::SIGHUP {
            HANGUP
        } else {
            TERMINATE
        },
        Ordering::Relaxed,
    );
}

impl Drop for SessionSignals {
    fn drop(&mut self) {
        // Keep pending bits until the next install so an in-flight handler is
        // never raced by a teardown reset.
        unsafe {
            libc::sigaction(libc::SIGTERM, &self.previous_term, std::ptr::null_mut());
            libc::sigaction(libc::SIGHUP, &self.previous_hup, std::ptr::null_mut());
        }
        LEASED.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests;
