use super::*;
use std::collections::VecDeque;

enum Step {
    Write(usize),
    Error(io::ErrorKind),
    StopAfter(usize),
}
struct Script {
    steps: VecDeque<Step>,
    bytes: Vec<u8>,
    stop: SessionStop,
    blocked: bool,
}
impl Write for Script {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.blocked {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let count = match self.steps.pop_front() {
            Some(Step::Write(count)) => count.min(bytes.len()),
            Some(Step::Error(kind)) => return Err(kind.into()),
            Some(Step::StopAfter(count)) => {
                self.stop.stop();
                count.min(bytes.len())
            }
            None => bytes.len(),
        };
        self.bytes.extend_from_slice(&bytes[..count]);
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        panic!("OS flush must not be used");
    }
}
impl OutputSink for Script {
    fn wait_writable(&mut self, timeout: Duration) -> io::Result<()> {
        if self.blocked {
            std::thread::sleep(timeout);
        }
        Ok(())
    }
}
fn output(steps: Vec<Step>) -> HostOutput<Script> {
    let stop = SessionStop::default();
    HostOutput::new(
        Script {
            steps: steps.into(),
            bytes: Vec::new(),
            stop: stop.clone(),
            blocked: false,
        },
        stop,
    )
}

#[test]
fn short_writes_wouldblock_and_eintr_preserve_exact_transaction_bytes() {
    let mut output = output(vec![
        Step::Write(2),
        Step::Error(io::ErrorKind::Interrupted),
        Step::Write(4),
        Step::Error(io::ErrorKind::WouldBlock),
        Step::Write(1),
    ]);
    let bytes = "\x1b[31mé\x1b[0m".as_bytes().to_vec();
    output.send(bytes.clone()).unwrap();
    assert_eq!(output.writer.bytes, bytes);
    output.send(b"next".to_vec()).unwrap();
    assert_eq!(output.writer.bytes, [bytes, b"next".to_vec()].concat());
}

#[test]
fn cancellation_retains_suffix_and_only_finalization_can_follow_it() {
    for written in 0.."\x1b[31mé\x1b[0m".len() {
        let bytes = "\x1b[31mé\x1b[0m".as_bytes().to_vec();
        let mut output = output(vec![Step::StopAfter(written)]);
        let error = output.send(bytes.clone()).unwrap_err();
        assert_ne!(error.kind(), io::ErrorKind::Interrupted);
        assert!(output.send(b"FORBIDDEN".to_vec()).is_err());
        let deadline = Instant::now() + Duration::from_millis(50);
        assert_eq!(output.finish_pending(deadline), ProtocolDelivery::Submitted);
        assert_eq!(
            output.finish_batch(b"RESTORE\r\nREPORT".to_vec(), deadline),
            ProtocolDelivery::Submitted
        );
        let expected = if written == 0 {
            b"RESTORE\r\nREPORT".to_vec()
        } else {
            [bytes, b"RESTORE\r\nREPORT".to_vec()].concat()
        };
        assert_eq!(output.writer.bytes, expected);
    }
}

#[test]
fn expired_finalization_keeps_partial_transaction_without_injecting_restore() {
    let mut output = output(vec![Step::StopAfter(3)]);
    assert!(output.send(b"unfinished".to_vec()).is_err());
    output.writer.blocked = true;
    let deadline = Instant::now() + Duration::from_millis(20);
    assert_eq!(
        output.finish_pending(deadline),
        ProtocolDelivery::Incomplete
    );
    assert_eq!(
        output.finish_batch(b"RESTORE".to_vec(), deadline),
        ProtocolDelivery::Incomplete
    );
    assert_eq!(output.writer.bytes, b"unf");
    assert!(output.send(b"next".to_vec()).is_err());
}

#[test]
fn oversized_batch_fails_before_any_os_write_and_cannot_resume() {
    let mut output = output(vec![]);
    assert!(output.send(vec![0; STAGING_LIMIT + 1]).is_err());
    assert!(output.writer.bytes.is_empty());
    assert!(output.send(b"next".to_vec()).is_err());
}

#[test]
fn private_terminal_output_cancels_a_real_stall_without_changing_inherited_flags() {
    use std::os::fd::{AsRawFd, FromRawFd};
    let (mut master, mut slave) = (0, 0);
    unsafe {
        assert_eq!(
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            0
        );
    }
    let master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    let flags = unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_GETFL) };
    let input = crate::host::open_terminal(slave.as_raw_fd(), true).unwrap();
    let writer = crate::host::open_terminal(slave.as_raw_fd(), false).unwrap();
    let mut raw = crate::host::RawMode::enter(&input).unwrap();
    let stop = SessionStop::default();
    let cancel = stop.clone();
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        cancel.stop();
    });
    let mut output = HostOutput::new(writer, stop);
    let start = Instant::now();
    assert!(output.send(vec![b'x'; 2 * 1024 * 1024]).is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(output.started());
    raw.restore().unwrap();
    assert_eq!(
        output.finish_pending(Instant::now() + Duration::from_millis(20)),
        ProtocolDelivery::Incomplete
    );
    assert_eq!(
        unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_GETFL) },
        flags
    );
    assert_ne!(
        unsafe { libc::fcntl(input.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
        0
    );
    thread.join().unwrap();
    drop(master);
}
