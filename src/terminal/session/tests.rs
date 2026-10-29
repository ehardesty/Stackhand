use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::super::paste::PasteCompletion;
use super::*;
use crossterm::event::{KeyCode, KeyModifiers};

#[test]
fn extreme_scroll_deltas_are_bounded_before_the_ghostty_call() {
    assert_eq!(bounded_scroll_delta(isize::MIN), -1_000_000);
    assert_eq!(bounded_scroll_delta(isize::MAX), 1_000_000);
    assert_eq!(bounded_scroll_delta(-5), -5);
}

#[test]
fn failed_pty_resize_becomes_a_terminal_failure() {
    let (reader, peer) = UnixStream::pair().unwrap();
    let io = PtyIo {
        reader: Box::new(reader),
        writer: Box::new(io::sink()),
        resizer: Box::new(|_, _| Err("fixture resize failure".into())),
    };
    let session = TerminalSession::spawn(io, TerminalGeometry::DEFAULT, || {}).unwrap();
    let _ = session.resize(TerminalGeometry::new(42, 12).unwrap());

    let deadline = Instant::now() + Duration::from_secs(1);
    let mut failure = None;
    while Instant::now() < deadline {
        if let Some(TerminalEvent::Failed(error)) = session.poll_event() {
            failure = Some(error);
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }

    drop(peer);
    session.shutdown().unwrap();
    assert!(failure.unwrap().contains("PTY resize to 42x12 failed"));
}

struct ActiveOutputReader {
    chunk: Vec<u8>,
    stop: Arc<AtomicBool>,
    produced: Arc<AtomicUsize>,
}

impl Read for ActiveOutputReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.stop.load(Ordering::Acquire) {
            return Ok(0);
        }

        let amount = buffer.len().min(self.chunk.len());
        buffer[..amount].copy_from_slice(&self.chunk[..amount]);
        self.produced.fetch_add(amount, Ordering::Release);
        Ok(amount)
    }
}

struct RecordingWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Write for RecordingWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.bytes.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn scrollbar_reports_and_moves_through_ghostty_rows() {
    let (reader, mut peer) = UnixStream::pair().unwrap();
    let io = PtyIo {
        reader: Box::new(reader),
        writer: Box::new(io::sink()),
        resizer: Box::new(|_, _| Ok(())),
    };
    let geometry = TerminalGeometry::new(20, 4).unwrap();
    let session = TerminalSession::spawn(io, geometry, || {}).unwrap();
    for index in 0..20 {
        writeln!(peer, "line-{index}").unwrap();
    }

    let output_deadline = Instant::now() + Duration::from_secs(1);
    let tail = loop {
        let snapshot = session.snapshot();
        if snapshot.scrollbar.total > snapshot.scrollbar.len {
            break snapshot.scrollbar;
        }
        assert!(
            Instant::now() < output_deadline,
            "terminal scrollback did not become scrollable"
        );
        thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(tail.len, 4);
    assert_eq!(tail.offset.saturating_add(tail.len), tail.total);

    session.scroll_to_row(0);
    let top_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let scrollbar = session.snapshot().scrollbar;
        if scrollbar.offset == 0 {
            assert_eq!(scrollbar.total, tail.total);
            break;
        }
        assert!(
            Instant::now() < top_deadline,
            "absolute scrollbar movement did not reach the terminal head"
        );
        thread::sleep(Duration::from_millis(1));
    }

    drop(peer);
    session.shutdown().unwrap();
}

#[test]
fn mouse_tracking_state_is_available_without_cloning_a_render_snapshot() {
    let (reader, mut peer) = UnixStream::pair().unwrap();
    let io = PtyIo {
        reader: Box::new(reader),
        writer: Box::new(io::sink()),
        resizer: Box::new(|_, _| Ok(())),
    };
    let session = TerminalSession::spawn(io, TerminalGeometry::DEFAULT, || {}).unwrap();

    peer.write_all(b"\x1b[?1000h").unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while !session.mouse_tracking() {
        assert!(
            Instant::now() < deadline,
            "mouse tracking state was not published"
        );
        thread::sleep(Duration::from_millis(1));
    }

    drop(peer);
    session.shutdown().unwrap();
}

#[test]
fn ctrl_c_is_encoded_as_child_input() {
    let (reader, peer) = UnixStream::pair().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let io = PtyIo {
        reader: Box::new(reader),
        writer: Box::new(RecordingWriter {
            bytes: Arc::clone(&captured),
        }),
        resizer: Box::new(|_, _| Ok(())),
    };
    let session = TerminalSession::spawn(io, TerminalGeometry::DEFAULT, || {}).unwrap();
    session
        .send_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(1);
    while !captured.lock().unwrap().contains(&0x03) {
        assert!(
            Instant::now() < deadline,
            "Ctrl-C did not reach the PTY writer"
        );
        thread::sleep(Duration::from_millis(1));
    }

    drop(peer);
    session.shutdown().unwrap();
}

#[test]
fn keyboard_selection_is_visible_and_never_writes_to_the_child() {
    let (reader, mut peer) = UnixStream::pair().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let io = PtyIo {
        reader: Box::new(reader),
        writer: Box::new(RecordingWriter {
            bytes: Arc::clone(&captured),
        }),
        resizer: Box::new(|_, _| Ok(())),
    };
    let geometry = TerminalGeometry::new(8, 3).unwrap();
    let session = TerminalSession::spawn(io, geometry, || {}).unwrap();
    peer.write_all(b"abcd\x1b[1;1H\x1b[?25l").unwrap();

    let output_deadline = Instant::now() + Duration::from_secs(1);
    while session.snapshot().text() != "abcd" {
        assert!(
            Instant::now() < output_deadline,
            "terminal output was not parsed"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(session.snapshot().cursor, None);

    session.start_keyboard_selection();
    let cursor_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let snapshot = session.snapshot();
        if snapshot.cursor.map(|cursor| cursor.position) == Some(Position::new(0, 0)) {
            break;
        }
        assert!(
            Instant::now() < cursor_deadline,
            "keyboard copy cursor did not become visible"
        );
        thread::sleep(Duration::from_millis(1));
    }

    session.move_keyboard_selection(SelectionDirection::Right);
    session.toggle_keyboard_selection();
    session.move_keyboard_selection(SelectionDirection::Right);
    let request = session.request_copy();
    let copy_deadline = Instant::now() + Duration::from_secs(1);
    let copied = loop {
        if let Some(result) = request.poll() {
            break result.unwrap();
        }
        assert!(
            Instant::now() < copy_deadline,
            "copy request did not complete"
        );
        thread::sleep(Duration::from_millis(1));
    };

    assert_eq!(copied.as_deref(), Some("bc"));
    assert!(captured.lock().unwrap().is_empty());

    drop(peer);
    session.shutdown().unwrap();
}

#[test]
fn active_output_does_not_delay_an_admitted_terminal_command() {
    const ACTIVE_OUTPUT_BYTES: usize = 550 * 1_024;
    const COMMAND_LATENCY_BOUND: Duration = Duration::from_secs(1);

    let mut chunk = vec![0; 4_096];
    let line = b"active-output-line-0123456789\r\n";
    for (index, byte) in chunk.iter_mut().enumerate() {
        *byte = line[index % line.len()];
    }
    let stop = Arc::new(AtomicBool::new(false));
    let produced = Arc::new(AtomicUsize::new(0));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let io = PtyIo {
        reader: Box::new(ActiveOutputReader {
            chunk,
            stop: Arc::clone(&stop),
            produced: Arc::clone(&produced),
        }),
        writer: Box::new(RecordingWriter {
            bytes: Arc::clone(&captured),
        }),
        resizer: Box::new(|_, _| Ok(())),
    };
    let session = TerminalSession::spawn(io, TerminalGeometry::DEFAULT, || {}).unwrap();

    let produced_deadline = Instant::now() + Duration::from_secs(10);
    while produced.load(Ordering::Acquire) < ACTIVE_OUTPUT_BYTES {
        while let Some(event) = session.poll_event() {
            if let TerminalEvent::Failed(error) = event {
                panic!("terminal owner failed during the output burst: {error}");
            }
        }
        assert!(
            Instant::now() < produced_deadline,
            "active output fixture did not produce its initial burst"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let output_before_command = {
        let metrics = session.metrics();
        metrics.parsed_bytes
    };
    let command_started = Instant::now();
    let mut request = session.send_paste("fairness-sentinel").unwrap();
    let completion_deadline = command_started + COMMAND_LATENCY_BOUND;
    let completion = loop {
        if let Some(completion) = request.poll() {
            break completion;
        }
        let elapsed = command_started.elapsed();
        assert!(
            Instant::now() < completion_deadline,
            "accepted terminal command exceeded {COMMAND_LATENCY_BOUND:?} while output was active (elapsed {elapsed:?})"
        );
        thread::sleep(Duration::from_millis(1));
    };

    assert_eq!(completion, PasteCompletion::Delivered);
    let output_after_command = {
        let metrics = session.metrics();
        metrics.parsed_bytes
    };
    assert!(
        output_after_command > output_before_command,
        "output parsing did not progress while the command was pending"
    );
    assert!(
        captured
            .lock()
            .unwrap()
            .windows(b"fairness-sentinel".len())
            .any(|window| window == b"fairness-sentinel"),
        "accepted terminal command was not delivered to the PTY writer"
    );

    stop.store(true, Ordering::Release);
    session.shutdown().unwrap();
}
