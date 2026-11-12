use super::*;

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

#[test]
fn ratatui_drop_is_inert_after_staging_failure_and_quiescence() {
    let mut stager = CommandStager::default();
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(stager.clone()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 2, 2)),
        },
    )
    .unwrap();
    terminal.hide_cursor().unwrap();
    assert!(
        stager
            .write_all(&vec![0; crate::host::output::STAGING_LIMIT + 1])
            .is_err()
    );
    stager.close();
    terminal.show_cursor().unwrap();
    assert!(stager.take().is_err());
    drop(terminal);
}

#[test]
fn staging_limit_supports_large_styled_host_frames_and_fails_atomically() {
    let mut stager = CommandStager::default();
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(stager.clone()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 500, 200)),
        },
    )
    .unwrap();
    terminal
        .draw(|frame| {
            for y in 0..200 {
                for x in 0..500 {
                    frame.buffer_mut()[(x, y)]
                        .set_symbol(if x % 2 == 0 { "é" } else { "界" })
                        .set_fg(ratatui::style::Color::Rgb(
                            (x % 256) as u8,
                            (y % 256) as u8,
                            ((x + y) % 256) as u8,
                        ));
                }
            }
        })
        .unwrap();
    let bytes = stager.take().unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("界"));
    assert!(bytes.len() < crate::host::output::STAGING_LIMIT);
    stager.write_all(b"partial staging").unwrap();
    assert!(
        stager
            .write_all(&vec![0; crate::host::output::STAGING_LIMIT])
            .is_err()
    );
    assert!(stager.take().is_err());
    stager.close();
    terminal.show_cursor().unwrap();
}
