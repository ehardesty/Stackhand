use super::*;
use std::cell::RefCell;

fn count(bytes: &[u8], sequence: &[u8]) -> usize {
    bytes
        .windows(sequence.len())
        .filter(|part| *part == sequence)
        .count()
}

#[test]
fn modes_restore_once_in_reverse_order_with_paste_and_keyboard_pop() {
    let mut bytes = Vec::new();
    let mut modes = OuterModes::default();
    modes.enter(&mut bytes, true).unwrap();
    assert_eq!(count(&bytes, b"\x1b[?2004h"), 1);
    let start = bytes.len();
    modes.restore(|mode| mode.disable(&mut bytes));
    let restored = &bytes[start..];
    let mut after = 0;
    for sequence in [
        b"\x1b[<1u".as_slice(),
        b"\x1b[?25h",
        b"\x1b[?1006l",
        b"\x1b[?1004l",
        b"\x1b[?2004l",
        b"\x1b[?1049l",
    ] {
        assert_eq!(count(restored, sequence), 1);
        after += restored[after..]
            .windows(sequence.len())
            .position(|part| part == sequence)
            .unwrap()
            + sequence.len();
    }
    assert!(restored.ends_with(b"\x1b[0 q"));
    let end = bytes.len();
    modes.restore(|mode| mode.disable(&mut bytes));
    assert_eq!(bytes.len(), end);
}

struct FailingOutput {
    bytes: Vec<u8>,
    successful_flushes: usize,
    fail_flush: Option<usize>,
}

impl Write for FailingOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.fail_flush == Some(self.successful_flushes) {
            self.fail_flush = None;
            return Err(io::Error::other("lost terminal"));
        }
        self.successful_flushes += 1;
        Ok(())
    }
}

#[test]
fn failed_entry_restores_attempted_modes_but_never_pops_an_unconfirmed_push() {
    for failure in 0..6 {
        let mut out = FailingOutput {
            bytes: Vec::new(),
            successful_flushes: 0,
            fail_flush: Some(failure),
        };
        let mut modes = OuterModes::default();
        assert!(modes.enter(&mut out, true).is_err());
        let start = out.bytes.len();
        modes.restore(|mode| mode.disable(&mut out));
        let restored = &out.bytes[start..];
        for (index, reset) in [
            b"\x1b[?1049l".as_slice(),
            b"\x1b[?2004l",
            b"\x1b[?1004l",
            b"\x1b[?1006l",
            b"\x1b[?25h",
        ]
        .iter()
        .enumerate()
        {
            assert_eq!(count(restored, reset), usize::from(index <= failure));
        }
        assert_eq!(count(restored, b"\x1b[<1u"), 0);
        let end = out.bytes.len();
        modes.restore(|mode| mode.disable(&mut out));
        assert_eq!(out.bytes.len(), end);
    }
}

#[derive(Default)]
struct OutputState {
    fail: bool,
    writes: usize,
    flushes: usize,
    drops: usize,
}

struct LostOutput(Rc<RefCell<OutputState>>);
impl Write for LostOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self.0.borrow_mut();
        state.writes += 1;
        if state.fail {
            Err(io::Error::other("lost terminal"))
        } else {
            Ok(bytes.len())
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        let mut state = self.0.borrow_mut();
        state.flushes += 1;
        if state.fail {
            Err(io::Error::other("lost terminal"))
        } else {
            Ok(())
        }
    }
}
impl Drop for LostOutput {
    fn drop(&mut self) {
        self.0.borrow_mut().drops += 1;
    }
}

#[test]
fn cleanup_frees_ratatui_after_output_loss_without_second_cursor_restore() {
    let state = Rc::new(RefCell::new(OutputState::default()));
    let ignore_errors = Rc::new(Cell::new(false));
    let mut out = TerminalOutput {
        writer: LostOutput(Rc::clone(&state)),
        ignore_errors: Rc::clone(&ignore_errors),
    };
    let mut modes = OuterModes::default();
    modes.enter(&mut out, true).unwrap();
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(out),
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 2, 2)),
        },
    )
    .unwrap();
    terminal.hide_cursor().unwrap();
    state.borrow_mut().fail = true;
    assert!(terminal.show_cursor().is_err());
    assert!(terminal.backend_mut().flush().is_err());
    ignore_errors.set(true);
    modes.restore_terminal(&mut terminal);
    let writes = state.borrow().writes;
    drop(terminal);
    assert_eq!(
        state.borrow().writes,
        writes,
        "Ratatui restored the cursor twice"
    );
    assert_eq!(state.borrow().drops, 1, "terminal output was leaked");
}
