use std::fs::File;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use super::{SessionStop, tty};

pub(crate) const STAGING_LIMIT: usize = 16 * 1024 * 1024;
const POLL_SLICE: Duration = Duration::from_millis(10);

pub(crate) trait OutputSink: Write {
    fn wait_writable(&mut self, timeout: Duration) -> io::Result<()>;
}

impl OutputSink for File {
    fn wait_writable(&mut self, timeout: Duration) -> io::Result<()> {
        tty::poll(self, libc::POLLOUT, timeout)
    }
}

struct Pending {
    bytes: Vec<u8>,
    written: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProtocolDelivery {
    Submitted,
    Incomplete,
}

pub(crate) struct HostOutput<W: OutputSink = File> {
    writer: W,
    stop: SessionStop,
    pending: Option<Pending>,
    live: bool,
}

impl<W: OutputSink> HostOutput<W> {
    pub(crate) fn new(writer: W, stop: SessionStop) -> Self {
        Self {
            writer,
            stop,
            pending: None,
            live: true,
        }
    }

    pub(crate) fn send(&mut self, bytes: Vec<u8>) -> io::Result<()> {
        self.submit(bytes, None)
    }

    pub(crate) fn send_until(&mut self, bytes: Vec<u8>, deadline: Instant) -> io::Result<()> {
        self.submit(bytes, Some(deadline))
    }

    fn submit(&mut self, bytes: Vec<u8>, deadline: Option<Instant>) -> io::Result<()> {
        if !self.live {
            return Err(io::Error::other("host output is quiescent"));
        }
        if bytes.len() > STAGING_LIMIT {
            self.quiesce();
            return Err(io::Error::other(
                "terminal transaction exceeds the 16 MiB staging limit",
            ));
        }
        self.pending = Some(Pending { bytes, written: 0 });
        let result = self.drain(deadline);
        if result.is_err() {
            self.quiesce();
        }
        result
    }

    pub(crate) fn started(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.written > 0)
    }

    pub(crate) fn quiesce(&mut self) {
        self.live = false;
        if !self.started() {
            self.pending = None;
        }
    }

    pub(crate) fn finish_pending(&mut self, deadline: Instant) -> ProtocolDelivery {
        self.quiesce();
        if self.drain(Some(deadline)).is_ok() {
            ProtocolDelivery::Submitted
        } else {
            ProtocolDelivery::Incomplete
        }
    }

    pub(crate) fn finish_batch(&mut self, bytes: Vec<u8>, deadline: Instant) -> ProtocolDelivery {
        if self.pending.is_some() || bytes.len() > STAGING_LIMIT {
            return ProtocolDelivery::Incomplete;
        }
        self.pending = Some(Pending { bytes, written: 0 });
        if self.drain(Some(deadline)).is_ok() {
            ProtocolDelivery::Submitted
        } else {
            ProtocolDelivery::Incomplete
        }
    }

    fn drain(&mut self, deadline: Option<Instant>) -> io::Result<()> {
        let Some(pending) = self.pending.as_mut() else {
            return Ok(());
        };
        while pending.written < pending.bytes.len() {
            if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "terminal output deadline expired",
                    ));
                }
            } else if self.stop.requested() {
                return Err(io::Error::other(
                    "terminal output cancelled by session stop",
                ));
            }
            match self.writer.write(&pending.bytes[pending.written..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "host output closed",
                    ));
                }
                Ok(count) => pending.written += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let wait = deadline.map_or(POLL_SLICE, |end| {
                        POLL_SLICE.min(end.saturating_duration_since(Instant::now()))
                    });
                    self.writer.wait_writable(wait)?;
                }
                Err(error) => return Err(error),
            }
        }
        self.pending = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests;

impl HostOutput<File> {
    pub(crate) fn drain_or_abandon_leader_output(&mut self, deadline: Instant) -> bool {
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;
            // Darwin drains a session leader's controlling tty during exit.
            if unsafe { libc::getsid(0) == libc::getpid() } {
                loop {
                    let mut queued: libc::c_int = 0;
                    if unsafe { libc::ioctl(self.writer.as_raw_fd(), libc::TIOCOUTQ, &mut queued) }
                        != 0
                    {
                        return false;
                    }
                    if queued == 0 {
                        return true;
                    }
                    if Instant::now() >= deadline {
                        let _ = unsafe { libc::tcflush(self.writer.as_raw_fd(), libc::TCOFLUSH) };
                        return false;
                    }
                    std::thread::sleep(
                        Duration::from_millis(1)
                            .min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = deadline;
        true
    }
}
