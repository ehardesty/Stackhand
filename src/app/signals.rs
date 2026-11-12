use std::cell::Cell;
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
    first_observed: Cell<Option<SessionSignal>>,
}

impl SessionSignals {
    pub(super) fn install() -> Result<Self> {
        if LEASED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            bail!("another interactive session already owns the signal handlers");
        }
        discard_signals_from_previous_session();
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
                    first_observed: Cell::new(None),
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
        let signal = if pending & HANGUP != 0 {
            Some(SessionSignal::Hangup)
        } else if pending & TERMINATE != 0 {
            Some(SessionSignal::Terminate)
        } else {
            None
        };
        self.first_observed
            .set(self.first_observed.get().or(signal));
        signal
    }

    pub(super) fn first_observed(&self) -> Option<SessionSignal> {
        self.received();
        self.first_observed.get()
    }
}

fn discard_signals_from_previous_session() {
    PENDING.store(0, Ordering::Relaxed);
}

extern "C" fn record(signal: libc::c_int) {
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
        unsafe {
            libc::sigaction(libc::SIGTERM, &self.previous_term, std::ptr::null_mut());
            libc::sigaction(libc::SIGHUP, &self.previous_hup, std::ptr::null_mut());
        }
        LEASED.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests;
