use std::thread;
use std::time::Duration;

use anyhow::{Result, ensure};

use crate::fixtures::{start_fixture_run, wait_for_snapshot};
use crate::geometry::TerminalGeometry;
use crate::runtime::{SpawnCommand, TerminalHandle};

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
    let mut run = start_fixture_run(command, geometry, None)?;
    let session = run.terminal().expect("scrollback fixture is PTY-mode");

    let fixture_result = run_steps(&session);
    let shutdown_result = run.shutdown();
    fixture_result?;
    shutdown_result?;
    println!("scrollback-fixture: bounded history continued draining while scrolled and unfocused");
    Ok(())
}

fn run_steps(session: &TerminalHandle<'_>) -> Result<()> {
    wait_for_snapshot(session, "scrollback", |snapshot| {
        snapshot.text().contains("scroll-ready")
    })?;
    session.scroll_lines(-1_000_000);
    let history = wait_for_snapshot(session, "scrollback", |snapshot| {
        snapshot.text().contains("before-") && !snapshot.text().contains("scroll-ready")
    })?;
    ensure!(
        !history.text().contains("before-0000"),
        "the configured scrollback bound retained the first discarded line"
    );

    let _ = session.send_focus(false);
    let _ = session.send_raw(vec![b'\n']);
    // Do not request a snapshot while the producer writes much more data
    // than a PTY can buffer. The terminal owner must keep draining output
    // even when the UI is unfocused and does not redraw.
    thread::sleep(Duration::from_millis(1_250));
    session.follow_live();
    wait_for_snapshot(session, "scrollback", |snapshot| {
        snapshot.text().contains("producer-complete")
    })?;
    Ok(())
}
