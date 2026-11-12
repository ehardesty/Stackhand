use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

pub(super) fn split(master: &dyn portable_pty::MasterPty) -> io::Result<(PtyReader, File)> {
    let raw = master
        .as_raw_fd()
        .ok_or_else(|| io::Error::other("PTY master has no descriptor"))?;
    // SAFETY: master owns raw and remains borrowed throughout duplication.
    let master_fd = unsafe { BorrowedFd::borrow_raw(raw) };
    set_nonblocking(master_fd)?;
    let reader = File::from(master_fd.try_clone_to_owned()?);
    let writer = File::from(master_fd.try_clone_to_owned()?);
    Ok((PtyReader { file: reader }, writer))
}

pub(super) struct PtyReader {
    file: File,
}

impl Read for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut hung_up = false;
        loop {
            match self.file.read(buf) {
                Ok(count) => return Ok(count),
                Err(error) if error.raw_os_error() == Some(libc::EIO) => return Ok(0),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if hung_up {
                        return Ok(0);
                    }
                    hung_up = wait_readable(self.file.as_fd())?;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn wait_readable(fd: BorrowedFd<'_>) -> io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let result = unsafe { libc::poll(&mut descriptor, 1, -1) };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if descriptor.revents & libc::POLLNVAL != 0 {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        return Ok(descriptor.revents & (libc::POLLHUP | libc::POLLERR) != 0);
    }
}
