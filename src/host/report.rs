use std::io::{self, IsTerminal, Write};
use std::os::fd::AsRawFd;
use std::time::Instant;

use super::{HostOutput, ProtocolDelivery, SessionStop, open_terminal, tty};

pub(crate) fn shares_output_terminal() -> bool {
    tty::same_terminal(libc::STDERR_FILENO, libc::STDOUT_FILENO)
}

pub(crate) fn write_redirected_report(text: &str, deadline: Instant) {
    if shares_output_terminal() {
        return;
    }
    if io::stderr().is_terminal() {
        if let Ok(file) = open_terminal(libc::STDERR_FILENO, false) {
            let mut output = HostOutput::new(file, SessionStop::default());
            let _ = output.finish_batch(text.as_bytes().to_vec(), deadline);
        }
        return;
    }
    unsafe {
        let mut stat = std::mem::zeroed();
        if libc::fstat(io::stderr().as_raw_fd(), &mut stat) == 0
            && stat.st_mode & libc::S_IFMT == libc::S_IFREG
        {
            let _ = io::stderr().write_all(text.as_bytes());
        }
    }
    // A redirected pipe has no independently reopened portable nonblocking sink.
    // Omit its interactive report rather than altering inherited descriptor flags.
}

pub(crate) struct RestoreOutcome {
    pub(crate) local_termios: io::Result<()>,
    pub(crate) protocol: ProtocolDelivery,
}
