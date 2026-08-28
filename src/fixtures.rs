use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier};

use crate::geometry::TerminalGeometry;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use crate::runtime::{
    OwnedRun, ProcessId, RunId, RunMode, RunRuntime, RunStartRequest, SpawnCommand, TerminalHandle,
};
use crate::terminal::{
    OwnedTerminalSnapshot, PASTE_LIMIT_BYTES, PasteCompletion, PasteRejection, PasteRequest,
    TerminalEvent,
};

static FIXTURE_RUN_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Start one PTY fixture Run through the same seam that the application uses.
pub(crate) fn start_fixture_run(
    command: SpawnCommand,
    geometry: TerminalGeometry,
    on_output_wake: Option<Box<dyn Fn() + Send + 'static>>,
) -> Result<OwnedRun> {
    let (events, _run_event_log) = mpsc::channel();
    let (output, _output_log) = crate::runtime::output_channel();
    let run_id = FIXTURE_RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    RunRuntime.start(RunStartRequest {
        process_id: ProcessId::new(u32::try_from(run_id).expect("fixture run id fits u32")),
        run_id: RunId::new(run_id),
        command,
        mode: RunMode::Pty {
            initial_geometry: geometry,
        },
        events,
        output,
        ladder: Default::default(),
        metrics_interval: None,
        on_output_wake,
        output_observer: None,
    })
}

const FIXTURE_TIMEOUT: Duration = Duration::from_secs(10);

pub fn run_fixture_round_trip(text: &str) -> Result<()> {
    if text.contains(['\r', '\n']) {
        bail!("fixture text must fit on one line");
    }

    let geometry = TerminalGeometry::DEFAULT;
    let command = SpawnCommand::new("/bin/sh").arg("-c").arg(
        "printf 'fixture-ready\\r\\n'; IFS= read -r line; printf 'fixture-echo:%s\\r\\n' \"$line\"; set -- $(stty size); printf 'fixture-size:%sx%s\\r\\n' \"$2\" \"$1\"",
    );
    let mut run = start_fixture_run(command, geometry, None)?;
    let session = run.terminal().expect("PTY fixture");

    let resized_geometry =
        TerminalGeometry::new(42, 12).expect("fixture geometry is always non-zero");
    let _ = session.resize(resized_geometry);

    for character in text.chars() {
        let _ = session.send_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
    }
    let _ = session.send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

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

    run.shutdown()?;

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
    let mut run = start_fixture_run(command, geometry, None)?;
    let session = run.terminal().expect("PTY fixture");

    wait_for_fixture_text(&session, "input-normal-ready")?;
    let _ = session.send_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    let _ = session.send_raw(vec![b'\n']);
    let second_phase = wait_for_fixture_text(&session, "input-ready")?;
    ensure!(
        second_phase
            .replace('\n', "")
            .contains("normal-bytes:1b5b41"),
        "normal cursor mode did not produce CSI A: {second_phase:?}"
    );
    for event in events {
        let _ = session.send_key(event);
    }
    let _ = session.send_focus(true);
    let _ = session.send_focus(false);
    let _ = session.send_raw(vec![b'\n']);

    let marker = format!("input-bytes:{expected_hex}");
    let output = wait_for_fixture_text(&session, &marker)?;
    run.shutdown()?;

    println!("{output}");
    Ok(())
}

pub fn run_fixture_paste() -> Result<()> {
    let geometry = TerminalGeometry::DEFAULT;
    let command = SpawnCommand::new("/bin/sh").arg("-c").arg(
        r#"stty raw -echo min 1 time 0
printf '\033[?2004lfixture-normal-ready\r\n'
normal=$(dd bs=1 count=12 2>/dev/null | od -An -tx1 | tr -d ' \n')
printf '\r\nnormal-bytes:%s\r\n' "$normal"
printf '\033[?2004hfixture-bracketed-ready\r\n'
bracketed=$(dd bs=1 count=27 2>/dev/null | od -An -tx1 | tr -d ' \n')
printf '\r\nbracketed-bytes:%s\r\n' "$bracketed"
printf 'fixture-oversized-ready\r\n'
safe=$(dd bs=1 count=4 2>/dev/null | od -An -tx1 | tr -d ' \n')
printf 'safe-bytes:%s\r\n' "$safe"
sleep 3"#,
    );
    let mut run = start_fixture_run(command, geometry, None)?;
    let session = run.terminal().expect("PTY fixture");

    let fixture_result = (|| {
        wait_for_fixture_text(&session, "fixture-normal-ready")?;
        let mut normal_request = session
            .send_paste("normal-paste")
            .map_err(|error| anyhow::anyhow!("normal paste was rejected: {error}"))?;
        wait_for_paste_delivery(&mut normal_request)?;
        let normal = wait_for_fixture_text(&session, "normal-bytes:")?;
        ensure!(
            normal.contains("normal-bytes:6e6f726d616c2d7061737465"),
            "normal paste bytes were changed: {normal:?}"
        );

        wait_for_fixture_text(&session, "fixture-bracketed-ready")?;
        let mut bracketed_request = session
            .send_paste("bracketed-paste")
            .map_err(|error| anyhow::anyhow!("bracketed paste was rejected: {error}"))?;
        wait_for_paste_delivery(&mut bracketed_request)?;
        let bracketed = wait_for_fixture_text(&session, "bracketed-bytes:")?;
        ensure!(
            bracketed
                .contains("bracketed-bytes:1b5b3230307e627261636b657465642d70617374651b5b3230317e"),
            "bracketed paste markers or bytes were changed: {bracketed:?}"
        );

        wait_for_fixture_text(&session, "fixture-oversized-ready")?;
        let oversized = "x".repeat(PASTE_LIMIT_BYTES + 1);
        ensure!(
            matches!(
                session.send_paste(&oversized),
                Err(PasteRejection::TooLarge { .. })
            ),
            "oversized paste was accepted"
        );
        let _ = session.send_raw(b"SAFE".to_vec());
        let safe = wait_for_fixture_text(&session, "safe-bytes:")?;
        ensure!(
            safe.contains("safe-bytes:53414645"),
            "oversized paste was partly delivered: {safe:?}"
        );

        let blocked = "b".repeat(PASTE_LIMIT_BYTES);
        let mut rejected = false;
        let mut blocked_requests = Vec::new();
        for _ in 0..8 {
            match session.send_paste(&blocked) {
                Ok(request) => blocked_requests.push(request),
                Err(PasteRejection::Busy { .. }) => {
                    rejected = true;
                    break;
                }
                Err(error) => bail!("blocked paste was rejected unexpectedly: {error}"),
            }
        }
        ensure!(rejected, "blocked paste burst did not reach its bound");
        let event = wait_for_input_backpressure(&session)?;
        println!(
            "paste-fixture: normal and bracketed bytes preserved; oversized paste rejected atomically; blocked paste warning visible ({event})"
        );
        Ok::<_, anyhow::Error>(())
    })();

    fixture_result?;
    run.shutdown()?;
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

fn wait_for_fixture_text(session: &TerminalHandle<'_>, expected: &str) -> Result<String> {
    let deadline = Instant::now() + FIXTURE_TIMEOUT;
    let mut output = session.snapshot().text();
    if output.contains(expected) || output.replace('\n', "").contains(expected) {
        return Ok(output);
    }
    while Instant::now() < deadline {
        while let Some(event) = session.poll_event() {
            match event {
                TerminalEvent::Failed(error) => bail!("terminal owner failed: {error}"),
                TerminalEvent::InputBackpressure { .. } => {
                    bail!("fixture filled the bounded child-input queue")
                }
                TerminalEvent::Exited
                | TerminalEvent::StateChanged
                | TerminalEvent::OutputTruncated => {}
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

fn wait_for_input_backpressure(session: &TerminalHandle<'_>) -> Result<String> {
    let deadline = Instant::now() + FIXTURE_TIMEOUT;
    while Instant::now() < deadline {
        while let Some(event) = session.poll_event() {
            match event {
                TerminalEvent::InputBackpressure {
                    attempted_bytes,
                    pending_bytes,
                    limit_bytes,
                } => {
                    return Ok(format!(
                        "attempted {attempted_bytes} bytes with {pending_bytes} of {limit_bytes} pending"
                    ));
                }
                TerminalEvent::Failed(error) => bail!("terminal owner failed: {error}"),
                TerminalEvent::Exited
                | TerminalEvent::StateChanged
                | TerminalEvent::OutputTruncated => {}
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    bail!("blocked paste did not produce a bounded-writer warning")
}

fn wait_for_paste_delivery(request: &mut PasteRequest) -> Result<()> {
    let deadline = Instant::now() + FIXTURE_TIMEOUT;
    while Instant::now() < deadline {
        match request.poll() {
            Some(PasteCompletion::Delivered) => return Ok(()),
            Some(PasteCompletion::Failed(error)) => {
                bail!("paste request {} failed: {error}", request.id())
            }
            None => thread::sleep(Duration::from_millis(1)),
        }
    }
    bail!("paste request {} did not complete", request.id())
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
    let mut run = start_fixture_run(command, geometry, None)?;
    let session = run.terminal().expect("PTY fixture");

    let fixture_result = (|| {
        let primary = wait_for_snapshot(&session, "render", |snapshot| {
            snapshot.buffer[(0, 0)].symbol() == "R"
                && snapshot
                    .cursor
                    .is_some_and(|cursor| cursor.position.x == 4 && cursor.position.y == 2)
        })?;
        assert_primary_render(&primary)?;

        send_fixture_enter(&session);
        let alternate = wait_for_snapshot(&session, "render", |snapshot| {
            snapshot.buffer[(0, 0)].symbol() == "A" && snapshot.cursor.is_none()
        })?;
        ensure!(
            alternate.text().starts_with("ALT"),
            "alternate screen did not render"
        );

        send_fixture_enter(&session);
        let restored = wait_for_snapshot(&session, "render", |snapshot| {
            snapshot.buffer[(0, 0)].symbol() == "R" && snapshot.cursor.is_some()
        })?;
        ensure!(
            restored.buffer[(0, 0)].symbol() == "R",
            "primary screen was not restored"
        );

        send_fixture_enter(&session);
        let unwrapped = wait_for_snapshot(&session, "render", |snapshot| {
            snapshot.text().starts_with("abcdefghijklmno")
        })?;
        ensure!(
            unwrapped.buffer.area().width == 16,
            "initial fixture width changed"
        );

        let narrow = TerminalGeometry::new(8, 6).expect("fixture geometry is non-zero");
        let _ = session.resize(narrow);
        let reflowed = wait_for_snapshot(&session, "render", |snapshot| {
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
            let _ = session.resize(TerminalGeometry::new(cols, rows).unwrap());
        }
        let final_snapshot = wait_for_snapshot(&session, "render", |snapshot| {
            snapshot.buffer.area().width == 7 && snapshot.buffer.area().height == 5
        })?;
        ensure!(
            final_snapshot.buffer.area().width > 0 && final_snapshot.buffer.area().height > 0,
            "rapid resize produced invalid geometry"
        );
        Ok::<_, anyhow::Error>(())
    })();

    fixture_result?;
    run.shutdown()?;
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

fn send_fixture_enter(session: &TerminalHandle<'_>) {
    let _ = session.send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
}

pub(crate) fn wait_for_snapshot(
    session: &TerminalHandle<'_>,
    fixture: &str,
    ready: impl Fn(&OwnedTerminalSnapshot) -> bool,
) -> Result<OwnedTerminalSnapshot> {
    let deadline = Instant::now() + FIXTURE_TIMEOUT;
    let mut snapshot = session.snapshot();
    while Instant::now() < deadline {
        while let Some(event) = session.poll_event() {
            match event {
                TerminalEvent::Failed(error) => bail!("terminal owner failed: {error}"),
                TerminalEvent::InputBackpressure { .. } => {
                    bail!("{fixture} fixture filled the child-input queue")
                }
                TerminalEvent::Exited
                | TerminalEvent::StateChanged
                | TerminalEvent::OutputTruncated => {}
            }
        }
        if session.is_dirty() {
            snapshot = session.snapshot();
        }
        if ready(&snapshot) {
            return Ok(snapshot);
        }
        thread::sleep(Duration::from_millis(5));
    }
    bail!(
        "{fixture} fixture timed out; terminal contained: {:?}",
        snapshot.text()
    )
}
