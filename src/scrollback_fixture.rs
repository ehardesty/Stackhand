use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail, ensure};

use crate::geometry::TerminalGeometry;
use crate::runtime::{PtyProcess, SpawnCommand};
use crate::terminal::{OwnedTerminalSnapshot, TerminalEvent, TerminalSession};

const FIXTURE_TIMEOUT: Duration = Duration::from_secs(10);

pub fn run() -> Result<()> {
    let geometry = TerminalGeometry::new(24, 6).expect("fixture geometry is non-zero");
    let command = SpawnCommand::new("/bin/sh").arg("-c").arg(
        r#"stty -echo
i=0
while [ "$i" -lt 6000 ]; do
    printf 'before-%04d\r\n' "$i"
    i=$((i + 1))
done
printf 'scroll-ready\r\n'
IFS= read -r _
i=0
while [ "$i" -lt 4000 ]; do
    printf 'after-%04d\r\n' "$i"
    i=$((i + 1))
done
printf 'producer-complete\r\n'
sleep 2"#,
    );
    let spawned = PtyProcess::spawn(command, geometry)?;
    let session = TerminalSession::spawn(spawned.io, geometry, || {})?;
    let mut process = spawned.process;

    let fixture_result = run_steps(&session);
    let process_result = process.shutdown();
    let session_result = session.shutdown();
    fixture_result?;
    process_result?;
    session_result?;
    println!("scrollback-fixture: bounded history continued draining while scrolled and unfocused");
    Ok(())
}

fn run_steps(session: &TerminalSession) -> Result<()> {
    wait_for_text(session, "scroll-ready")?;
    session.scroll_lines(-1_000_000);
    let history = wait_for_snapshot(session, |snapshot| {
        snapshot.text().contains("before-") && !snapshot.text().contains("scroll-ready")
    })?;
    ensure!(
        !history.text().contains("before-0000"),
        "the configured scrollback bound retained the first discarded line"
    );

    session.send_focus(false);
    session.send_raw(vec![b'\n']);
    // Do not request a snapshot while the producer writes much more data
    // than a PTY can buffer. The terminal owner must keep draining output
    // even when the UI is unfocused and does not redraw.
    thread::sleep(Duration::from_millis(1_250));
    session.follow_live();
    wait_for_text(session, "producer-complete")?;
    Ok(())
}

fn wait_for_text(session: &TerminalSession, expected: &str) -> Result<OwnedTerminalSnapshot> {
    wait_for_snapshot(session, |snapshot| snapshot.text().contains(expected))
}

fn wait_for_snapshot(
    session: &TerminalSession,
    ready: impl Fn(&OwnedTerminalSnapshot) -> bool,
) -> Result<OwnedTerminalSnapshot> {
    let deadline = Instant::now() + FIXTURE_TIMEOUT;
    let mut snapshot = session.snapshot();
    while Instant::now() < deadline {
        while let Some(event) = session.poll_event() {
            match event {
                TerminalEvent::Failed(error) => bail!("terminal owner failed: {error}"),
                TerminalEvent::InputBackpressure { .. } => {
                    bail!("scrollback fixture filled the child-input queue")
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
        "scrollback fixture timed out; terminal contained: {:?}",
        snapshot.text()
    )
}
