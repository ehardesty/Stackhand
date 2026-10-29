use std::cell::Cell;

use super::*;

#[test]
fn effect_collector_stops_at_its_byte_limit_and_reports_overflow() {
    let mut effects = Effects::new();
    effects.push(&vec![1; EFFECT_BUFFER_BYTES]);
    effects.push(&[2]);

    assert_eq!(effects.bytes, EFFECT_BUFFER_BYTES);
    assert_eq!(effects.queue.len(), 1);
    assert!(effects.overflowed);
}

#[test]
fn child_clipboard_reads_are_ignored_and_writes_reach_the_denial_boundary() {
    let writes = Cell::new(0_u8);
    let mut terminal = crate::terminal::new_terminal(10, 2, 1024).unwrap();
    terminal
        .on_clipboard_write(|_, write| {
            writes.set(writes.get() + 1);
            write.reply(Err(ClipboardWriteError::Denied), false);
        })
        .unwrap();

    terminal.vt_write(b"\x1b]52;c;?\x07");
    assert_eq!(writes.get(), 0);
    terminal.vt_write(b"\x1b]52;c;aGk=\x07");
    assert_eq!(writes.get(), 1);
}
