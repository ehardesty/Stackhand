use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use anyhow::{Result, bail};

const HANGUP: u8 = 1;
const TERMINATE: u8 = 2;
static PENDING: AtomicU8 = AtomicU8::new(0);
static LEASED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionSignal {
    Hangup,
    Terminate,
}

pub(crate) struct SessionSignals {
    previous_hup: libc::sigaction,
    previous_term: libc::sigaction,
    first_observed: Cell<Option<SessionSignal>>,
}

impl SessionSignals {
    pub(crate) fn install() -> Result<Self> {
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

    pub(crate) fn received(&self) -> Option<SessionSignal> {
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

    pub(crate) fn first_observed(&self) -> Option<SessionSignal> {
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

#[derive(Clone, Default)]
pub(crate) struct SessionStop {
    fault: Arc<AtomicU8>,
    stopping: Arc<AtomicBool>,
}

#[derive(Clone, Copy)]
pub(crate) enum HostFault {
    UnterminatedPaste = 1,
    SequenceTooLong = 2,
    InputClosed = 3,
    InputFailed = 4,
    UnterminatedColorReply = 5,
}

impl SessionStop {
    pub(crate) fn requested(&self) -> bool {
        PENDING.load(Ordering::Relaxed) != 0
            || self.stopping.load(Ordering::Acquire)
            || self.fault.load(Ordering::Acquire) != 0
    }

    pub(crate) fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
    }

    pub(crate) fn fail(&self, fault: HostFault) {
        let _ = self
            .fault
            .compare_exchange(0, fault as u8, Ordering::AcqRel, Ordering::Acquire);
    }

    pub(crate) fn framing_fault(&self) -> bool {
        matches!(self.fault.load(Ordering::Acquire), 1 | 2 | 5)
    }

    pub(crate) fn error(&self) -> Option<&'static str> {
        match self.fault.load(Ordering::Acquire) {
            0 => None,
            1 => Some("host paste framing exceeded the 30-second limit"),
            2 => Some("host input sequence exceeded its size limit"),
            3 => Some("host terminal input closed"),
            5 => Some("host color reply framing exceeded the 30-second limit"),
            _ => Some("host terminal input failed"),
        }
    }
}
