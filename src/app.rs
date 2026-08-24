use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::{Color, Modifier};

use crate::geometry::TerminalGeometry;
use crate::runtime::{PtyProcess, SpawnCommand};
use crate::terminal::{OwnedTerminalSnapshot, TerminalEvent, TerminalSession};
use crate::tui::{OuterTerminal, console_area, render};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(16);
const RESIZE_SETTLE_INTERVAL: Duration = Duration::from_millis(16);
const FIXTURE_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run_interactive() -> Result<()> {
    let mut outer = OuterTerminal::enter()?;
    let size = outer.terminal_mut().size()?;
    let geometry = TerminalGeometry::from_pane(console_area(size.into()));
    let spawned = PtyProcess::spawn(SpawnCommand::shell(), geometry)?;
    let dirty = Arc::new(AtomicBool::new(true));
    let wake_dirty = Arc::clone(&dirty);
    let session = TerminalSession::spawn(spawned.io, geometry, move || {
        wake_dirty.store(true, Ordering::Release);
    })?;
    let mut process = spawned.process;
    let mut snapshot = empty_snapshot(geometry);
    let mut pending_resize = PendingResize::default();

    let run_result = run_event_loop(
        &mut outer,
        &session,
        &dirty,
        &mut snapshot,
        &mut pending_resize,
    );
    let process_result = process.shutdown();
    let session_result = session.shutdown();

    run_result?;
    process_result?;
    session_result
}

fn run_event_loop(
    outer: &mut OuterTerminal,
    session: &TerminalSession,
    dirty: &AtomicBool,
    snapshot: &mut OwnedTerminalSnapshot,
    pending_resize: &mut PendingResize,
) -> Result<()> {
    loop {
        if let Some(geometry) = pending_resize.take_ready(Instant::now()) {
            session.resize(geometry);
            dirty.store(true, Ordering::Release);
        }

        while let Some(session_event) = session.poll_event() {
            match session_event {
                TerminalEvent::Failed(error) => bail!("terminal owner failed: {error}"),
                TerminalEvent::InputBackpressure {
                    attempted_bytes,
                    pending_bytes,
                    limit_bytes,
                } => bail!(
                    "child input was rejected because the bounded writer is full: attempted {attempted_bytes} bytes with {pending_bytes} of {limit_bytes} bytes pending"
                ),
                TerminalEvent::Exited => return Ok(()),
                TerminalEvent::StateChanged => {}
            }
        }

        if dirty.swap(false, Ordering::AcqRel) || session.is_dirty() {
            *snapshot = session.snapshot();
            outer.terminal_mut().draw(|frame| render(frame, snapshot))?;
            outer.set_cursor_shape(snapshot.cursor)?;
        }

        if !event::poll(pending_resize.poll_interval(Instant::now()))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press && is_quit(key) => return Ok(()),
            Event::Key(key) => session.send_key(key),
            Event::FocusGained => session.send_focus(true),
            Event::FocusLost => session.send_focus(false),
            Event::Resize(cols, rows) => {
                let geometry = TerminalGeometry::from_pane(console_area(
                    ratatui::layout::Rect::new(0, 0, cols, rows),
                ));
                pending_resize.update(geometry, Instant::now());
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct PendingResize {
    latest: Option<(TerminalGeometry, Instant)>,
}

impl PendingResize {
    fn update(&mut self, geometry: TerminalGeometry, now: Instant) {
        self.latest = Some((geometry, now + RESIZE_SETTLE_INTERVAL));
    }

    fn take_ready(&mut self, now: Instant) -> Option<TerminalGeometry> {
        let (geometry, ready_at) = self.latest?;
        if now < ready_at {
            return None;
        }
        self.latest = None;
        Some(geometry)
    }

    fn poll_interval(&self, now: Instant) -> Duration {
        self.latest
            .map(|(_, ready_at)| ready_at.saturating_duration_since(now))
            .unwrap_or(EVENT_POLL_INTERVAL)
            .min(EVENT_POLL_INTERVAL)
    }
}

fn is_quit(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL)
}

pub fn run_fixture_round_trip(text: &str) -> Result<()> {
    if text.contains(['\r', '\n']) {
        bail!("fixture text must fit on one line");
    }

    let geometry = TerminalGeometry::DEFAULT;
    let command = SpawnCommand::new("/bin/sh").arg("-c").arg(
        "printf 'fixture-ready\\r\\n'; IFS= read -r line; printf 'fixture-echo:%s\\r\\n' \"$line\"; set -- $(stty size); printf 'fixture-size:%sx%s\\r\\n' \"$2\" \"$1\"",
    );
    let spawned = PtyProcess::spawn(command, geometry)?;
    let session = TerminalSession::spawn(spawned.io, geometry, || {})?;
    let mut process = spawned.process;

    let resized_geometry =
        TerminalGeometry::new(42, 12).expect("fixture geometry is always non-zero");
    session.resize(resized_geometry);

    for character in text.chars() {
        session.send_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
    }
    session.send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let expected = format!("fixture-echo:{text}");
    let expected_size = "fixture-size:42x12";
    let deadline = Instant::now() + FIXTURE_TIMEOUT;
    let mut output = String::new();
    while Instant::now() < deadline {
        if session.is_dirty() {
            output = session.snapshot().text();
            if output.contains(&expected) && output.contains(expected_size) {
                break;
            }
        }
        thread::sleep(Duration::from_millis(5));
    }

    let process_result = process.shutdown();
    let session_result = session.shutdown();
    process_result?;
    session_result?;

    if !output.contains(&expected) || !output.contains(expected_size) {
        bail!("fixture did not produce the expected output; terminal contained: {output:?}");
    }
    println!("{output}");
    Ok(())
}

pub fn run_fixture_input() -> Result<()> {
    let geometry = TerminalGeometry::DEFAULT;
    let events = input_fixture_events();
    let mut expected = b"\x1b[4;1R\r\t\x7f\x1b\x1bOA\x1bOB\x1bOC\x1bOD\x1bOH\x1bOF\x1b[2~\x1b[3~\x1b[5~\x1b[6~\x1bOP\x1b[15~\x1b[24~x\x03\x1b[Z\x1b[1;5A".to_vec();
    expected.extend_from_slice(b"\x1b[I\x1b[O");
    let expected_hex = bytes_to_hex(&expected);
    let command = SpawnCommand::new("/bin/sh").arg("-c").arg(
        "stty raw -echo; printf '\\033[?1linput-normal-ready\\r\\n'; IFS= read -r normal; normal_hex=$(printf '%s' \"$normal\" | od -An -tx1 | tr -d ' \\n'); printf '\\r\\nnormal-bytes:%s\\r\\n' \"$normal_hex\"; printf '\\033[?1h\\033[?1004h\\033[?1036h\\033[6ninput-ready\\r\\n'; IFS= read -r bytes; hex=$(printf '%s' \"$bytes\" | od -An -tx1 | tr -d ' \\n'); printf '\\r\\ninput-bytes:%s\\r\\n' \"$hex\"",
    );
    let spawned = PtyProcess::spawn(command, geometry)?;
    let session = TerminalSession::spawn(spawned.io, geometry, || {})?;
    let mut process = spawned.process;

    wait_for_fixture_text(&session, "input-normal-ready")?;
    session.send_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    session.send_raw(vec![b'\n']);
    let second_phase = wait_for_fixture_text(&session, "input-ready")?;
    ensure!(
        second_phase
            .replace('\n', "")
            .contains("normal-bytes:1b5b41"),
        "normal cursor mode did not produce CSI A: {second_phase:?}"
    );
    for event in events {
        session.send_key(event);
    }
    session.send_focus(true);
    session.send_focus(false);
    session.send_raw(vec![b'\n']);

    let marker = format!("input-bytes:{expected_hex}");
    let output_result = wait_for_fixture_text(&session, &marker);
    let process_result = process.shutdown();
    let session_result = session.shutdown();
    let output = output_result?;
    process_result?;
    session_result?;

    println!("{output}");
    Ok(())
}

fn input_fixture_events() -> Vec<KeyEvent> {
    [
        (KeyCode::Enter, KeyModifiers::NONE),
        (KeyCode::Tab, KeyModifiers::NONE),
        (KeyCode::Backspace, KeyModifiers::NONE),
        (KeyCode::Esc, KeyModifiers::NONE),
        (KeyCode::Up, KeyModifiers::NONE),
        (KeyCode::Down, KeyModifiers::NONE),
        (KeyCode::Right, KeyModifiers::NONE),
        (KeyCode::Left, KeyModifiers::NONE),
        (KeyCode::Home, KeyModifiers::NONE),
        (KeyCode::End, KeyModifiers::NONE),
        (KeyCode::Insert, KeyModifiers::NONE),
        (KeyCode::Delete, KeyModifiers::NONE),
        (KeyCode::PageUp, KeyModifiers::NONE),
        (KeyCode::PageDown, KeyModifiers::NONE),
        (KeyCode::F(1), KeyModifiers::NONE),
        (KeyCode::F(5), KeyModifiers::NONE),
        (KeyCode::F(12), KeyModifiers::NONE),
        (KeyCode::Char('x'), KeyModifiers::ALT),
        (KeyCode::Char('c'), KeyModifiers::CONTROL),
        (KeyCode::BackTab, KeyModifiers::SHIFT),
        (KeyCode::Up, KeyModifiers::CONTROL),
    ]
    .into_iter()
    .map(|(code, modifiers)| KeyEvent::new(code, modifiers))
    .collect()
}

fn wait_for_fixture_text(session: &TerminalSession, expected: &str) -> Result<String> {
    let deadline = Instant::now() + FIXTURE_TIMEOUT;
    let mut output = String::new();
    while Instant::now() < deadline {
        while let Some(event) = session.poll_event() {
            match event {
                TerminalEvent::Failed(error) => bail!("terminal owner failed: {error}"),
                TerminalEvent::InputBackpressure { .. } => {
                    bail!("fixture filled the bounded child-input queue")
                }
                TerminalEvent::Exited | TerminalEvent::StateChanged => {}
            }
        }
        if session.is_dirty() {
            output = session.snapshot().text();
            if output.contains(expected) || output.replace('\n', "").contains(expected) {
                return Ok(output);
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    bail!("fixture did not contain {expected:?}; terminal contained: {output:?}")
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

pub fn run_fixture_rendering() -> Result<()> {
    let geometry = TerminalGeometry::new(16, 6).expect("fixture geometry is non-zero");
    let command = SpawnCommand::new("/bin/sh").arg("-c").arg(
        r#"stty -echo
printf '\033[2J\033[H\033[31mR\033[38;5;202mP\033[38;2;1;2;3mT\033[0m\033[1mB\033[22m\033[2mD\033[22m\033[3mI\033[23m\033[4mU\033[24m\033[7mV\033[27m界é'
printf '\033[3;5H\033[6 q'
IFS= read -r _
printf '\033[?1049h\033[2J\033[HALT\033[?25l'
IFS= read -r _
printf '\033[?25h\033[?1049l'
IFS= read -r _
printf '\033[2J\033[Habcdefghijklmno'
IFS= read -r _"#,
    );
    let spawned = PtyProcess::spawn(command, geometry)?;
    let session = TerminalSession::spawn(spawned.io, geometry, || {})?;
    let mut process = spawned.process;

    let fixture_result = (|| {
        let primary = wait_for_snapshot(&session, |snapshot| {
            snapshot.buffer[(0, 0)].symbol() == "R"
                && snapshot
                    .cursor
                    .is_some_and(|cursor| cursor.position.x == 4 && cursor.position.y == 2)
        })?;
        assert_primary_render(&primary)?;

        send_fixture_enter(&session);
        let alternate = wait_for_snapshot(&session, |snapshot| {
            snapshot.buffer[(0, 0)].symbol() == "A" && snapshot.cursor.is_none()
        })?;
        ensure!(
            alternate.text().starts_with("ALT"),
            "alternate screen did not render"
        );

        send_fixture_enter(&session);
        let restored = wait_for_snapshot(&session, |snapshot| {
            snapshot.buffer[(0, 0)].symbol() == "R" && snapshot.cursor.is_some()
        })?;
        ensure!(
            restored.buffer[(0, 0)].symbol() == "R",
            "primary screen was not restored"
        );

        send_fixture_enter(&session);
        let unwrapped = wait_for_snapshot(&session, |snapshot| {
            snapshot.text().starts_with("abcdefghijklmno")
        })?;
        ensure!(
            unwrapped.buffer.area().width == 16,
            "initial fixture width changed"
        );

        let narrow = TerminalGeometry::new(8, 6).expect("fixture geometry is non-zero");
        session.resize(narrow);
        let reflowed = wait_for_snapshot(&session, |snapshot| {
            snapshot.buffer.area().width == 8
                && snapshot.buffer[(0, 0)].symbol() == "a"
                && snapshot.buffer[(0, 1)].symbol() == "i"
        })?;
        ensure!(
            reflowed.text().starts_with("abcdefgh\nijklmno"),
            "wrapped text did not reflow at the new width: {:?}",
            reflowed.text()
        );

        for (cols, rows) in [(2, 1), (120, 40), (7, 5)] {
            session.resize(TerminalGeometry::new(cols, rows).unwrap());
        }
        let final_snapshot = wait_for_snapshot(&session, |snapshot| {
            snapshot.buffer.area().width == 7 && snapshot.buffer.area().height == 5
        })?;
        ensure!(
            final_snapshot.buffer.area().width > 0 && final_snapshot.buffer.area().height > 0,
            "rapid resize produced invalid geometry"
        );
        Ok::<_, anyhow::Error>(())
    })();

    let process_result = process.shutdown();
    let session_result = session.shutdown();
    fixture_result?;
    process_result?;
    session_result?;
    println!("render-fixture: colors styles unicode cursor alternate-screen reflow resize ok");
    Ok(())
}

fn assert_primary_render(snapshot: &OwnedTerminalSnapshot) -> Result<()> {
    let buffer = &snapshot.buffer;
    ensure!(
        buffer[(0, 0)].fg != Color::Reset && buffer[(0, 0)].fg != buffer[(3, 0)].fg,
        "16-color cell lost its color"
    );
    ensure!(
        buffer[(1, 0)].fg == Color::Rgb(255, 95, 0),
        "256-color cell changed"
    );
    ensure!(
        buffer[(2, 0)].fg == Color::Rgb(1, 2, 3),
        "truecolor cell changed"
    );
    for (x, modifier) in [
        (3, Modifier::BOLD),
        (4, Modifier::DIM),
        (5, Modifier::ITALIC),
        (6, Modifier::UNDERLINED),
        (7, Modifier::REVERSED),
    ] {
        ensure!(
            buffer[(x, 0)].modifier.contains(modifier),
            "style modifier {modifier:?} was not preserved"
        );
    }
    ensure!(
        buffer[(8, 0)].symbol() == "界",
        "wide character head changed"
    );
    ensure!(
        buffer[(9, 0)].symbol() == " ",
        "wide character tail used a visible cell"
    );
    ensure!(
        buffer[(10, 0)].symbol() == "é",
        "combining character was split"
    );
    let cursor = snapshot.cursor.context("visible cursor was missing")?;
    ensure!(cursor.position == (4, 2).into(), "cursor position changed");
    ensure!(
        cursor.shape == crate::terminal::CursorShape::Bar,
        "cursor shape changed"
    );
    ensure!(!cursor.blinking, "steady cursor became blinking");
    Ok(())
}

fn send_fixture_enter(session: &TerminalSession) {
    session.send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
}

fn wait_for_snapshot(
    session: &TerminalSession,
    ready: impl Fn(&OwnedTerminalSnapshot) -> bool,
) -> Result<OwnedTerminalSnapshot> {
    let deadline = Instant::now() + FIXTURE_TIMEOUT;
    let mut snapshot = session.snapshot();
    while Instant::now() < deadline {
        if session.is_dirty() {
            snapshot = session.snapshot();
        }
        if ready(&snapshot) {
            return Ok(snapshot);
        }
        thread::sleep(Duration::from_millis(5));
    }
    bail!(
        "render fixture timed out; terminal contained: {:?}",
        snapshot.text()
    )
}

fn empty_snapshot(geometry: TerminalGeometry) -> OwnedTerminalSnapshot {
    OwnedTerminalSnapshot {
        buffer: ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(
            0,
            0,
            geometry.cols(),
            geometry.rows(),
        )),
        cursor: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rapid_resize_uses_only_the_last_valid_geometry() {
        let started = Instant::now();
        let mut pending = PendingResize::default();
        pending.update(TerminalGeometry::new(120, 40).unwrap(), started);
        pending.update(
            TerminalGeometry::new(1, 1).unwrap(),
            started + Duration::from_millis(2),
        );
        pending.update(
            TerminalGeometry::new(73, 19).unwrap(),
            started + Duration::from_millis(4),
        );

        assert_eq!(
            pending.take_ready(started + Duration::from_millis(19)),
            None
        );
        assert_eq!(
            pending.take_ready(started + Duration::from_millis(20)),
            TerminalGeometry::new(73, 19)
        );
        assert_eq!(
            pending.take_ready(started + Duration::from_millis(21)),
            None
        );
    }
}
