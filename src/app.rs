use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::geometry::TerminalGeometry;
use crate::runtime::{PtyProcess, SpawnCommand};
use crate::terminal::{OwnedTerminalSnapshot, TerminalEvent, TerminalSession};
use crate::tui::{OuterTerminal, console_area, render};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(16);
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

    let run_result = run_event_loop(&mut outer, &session, &dirty, &mut snapshot);
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
) -> Result<()> {
    loop {
        while let Some(session_event) = session.poll_event() {
            match session_event {
                TerminalEvent::Failed(error) => bail!("terminal owner failed: {error}"),
                TerminalEvent::Exited => return Ok(()),
                TerminalEvent::StateChanged => {}
            }
        }

        if dirty.swap(false, Ordering::AcqRel) || session.is_dirty() {
            *snapshot = session.snapshot();
            outer.terminal_mut().draw(|frame| render(frame, snapshot))?;
        }

        if !event::poll(EVENT_POLL_INTERVAL)? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press && is_quit(key) => return Ok(()),
            Event::Key(key) => session.send_key(key),
            Event::Resize(cols, rows) => {
                let geometry = TerminalGeometry::from_pane(console_area(
                    ratatui::layout::Rect::new(0, 0, cols, rows),
                ));
                session.resize(geometry);
                dirty.store(true, Ordering::Release);
            }
            _ => {}
        }
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
