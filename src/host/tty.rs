use std::ffi::{CStr, OsStr};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::time::Duration;

fn identity(fd: RawFd) -> io::Result<libc::dev_t> {
    unsafe {
        let mut stat = std::mem::zeroed();
        if libc::isatty(fd) != 1 || libc::fstat(fd, &mut stat) != 0 {
            return Err(io::Error::other("interactive terminal descriptor required"));
        }
        Ok(stat.st_rdev)
    }
}

pub(crate) fn same_terminal(a: RawFd, b: RawFd) -> bool {
    identity(a)
        .ok()
        .zip(identity(b).ok())
        .is_some_and(|(a, b)| a == b)
}

pub(crate) fn open_terminal(fd: RawFd, read: bool) -> io::Result<File> {
    let expected = identity(fd)?;
    let mut path = [0; 1024];
    unsafe {
        let error = libc::ttyname_r(fd, path.as_mut_ptr(), path.len());
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
    }
    let path = unsafe { CStr::from_ptr(path.as_ptr()) };
    let file = OpenOptions::new()
        .read(read)
        .write(!read)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY)
        .open(OsStr::from_bytes(path.to_bytes()))?;
    if identity(file.as_raw_fd())? != expected {
        return Err(io::Error::other("reopened terminal identity changed"));
    }
    Ok(file)
}

pub(crate) fn terminal_size(file: &File) -> io::Result<(u16, u16)> {
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(file.as_raw_fd(), libc::TIOCGWINSZ, &mut size) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((size.ws_col.max(1), size.ws_row.max(1)))
    }
}

pub(super) fn poll(file: &File, events: i16, timeout: Duration) -> io::Result<()> {
    let mut fd = libc::pollfd {
        fd: file.as_raw_fd(),
        events,
        revents: 0,
    };
    let result =
        unsafe { libc::poll(&mut fd, 1, timeout.as_millis().min(i32::MAX as u128) as i32) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    if fd.revents & libc::POLLNVAL != 0 {
        return Err(io::Error::other("terminal descriptor closed"));
    }
    Ok(())
}

pub(crate) struct RawMode {
    file: File,
    saved: Option<libc::termios>,
}

impl RawMode {
    pub(crate) fn enter(file: &File) -> io::Result<Self> {
        let file = file.try_clone()?;
        unsafe {
            let mut saved = std::mem::zeroed();
            if libc::tcgetattr(file.as_raw_fd(), &mut saved) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(file.as_raw_fd(), libc::TCSANOW, &raw) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                file,
                saved: Some(saved),
            })
        }
    }

    pub(crate) fn size(&self) -> io::Result<(u16, u16)> {
        terminal_size(&self.file)
    }

    pub(crate) fn restore(&mut self) -> io::Result<()> {
        if let Some(saved) = self.saved.take()
            && unsafe { libc::tcsetattr(self.file.as_raw_fd(), libc::TCSANOW, &saved) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
