//! The integrated Project fixture: a headless proof of the full vertical
//! slice through the production configuration, Supervisor, Run adapter, and
//! console view. It prints observable checkpoints the executable fixture
//! test asserts.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

use crate::supervisor::{Command, Lifecycle};

const STARTUP_WAIT: Duration = Duration::from_secs(15);
const OUTPUT_WAIT: Duration = Duration::from_secs(10);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(20);

pub fn run(config_path: &Path) -> Result<()> {
    let project = crate::config::load(config_path)
        .map_err(|error| anyhow!("configuration error: {error}"))?;
    let (supervisor, consoles) = crate::supervisor::start(project)?;
    supervisor.command(Command::StartAutostart);

    let result = prove_slice(&supervisor, &consoles);
    let shutdown_result = shutdown(supervisor);
    result?;
    shutdown_result?;
    println!("fixture-shutdown-ok");
    Ok(())
}

fn prove_slice(
    supervisor: &crate::supervisor::SupervisorHandle,
    consoles: &crate::supervisor::Consoles,
) -> Result<()> {
    // Every enabled autostart Service reaches its active lifecycle.
    // Starting becomes Running on the Spawned event, so this waits past the
    // spawn window into the active Run.
    let snapshot = wait_for(supervisor, STARTUP_WAIT, |snapshot| {
        snapshot.processes.iter().all(|process| {
            !process.enabled || !process.autostart || process.lifecycle == Lifecycle::Running
        })
    })?;
    for process in &snapshot.processes {
        if process.enabled && process.autostart {
            assert_eq!(
                process.lifecycle,
                Lifecycle::Running,
                "Process {} did not start",
                process.name
            );
        }
    }
    println!("fixture-started-ok");

    // Output flows to the selected Process console without entering the
    // control plane.
    let marker_process = snapshot
        .processes
        .iter()
        .find(|process| process.current_run.is_some())
        .ok_or_else(|| anyhow!("no active Process to inspect"))?;
    let view = consoles
        .view(
            snapshot
                .processes
                .iter()
                .position(|p| p.name == marker_process.name)
                .expect("selected Process exists") as u32,
            marker_process.current_run.expect("active Run"),
        )
        .ok_or_else(|| anyhow!("no live console view"))?;
    let deadline = Instant::now() + OUTPUT_WAIT;
    loop {
        if view
            .snapshot()
            .is_some_and(|snapshot| buffer_text(&snapshot).contains("fixture-marker"))
        {
            break;
        }
        if Instant::now() >= deadline {
            bail!("the fixture marker never reached the console");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("fixture-output-ok");
    Ok(())
}

fn wait_for(
    supervisor: &crate::supervisor::SupervisorHandle,
    limit: Duration,
    done: impl Fn(&crate::supervisor::ProjectSnapshot) -> bool,
) -> Result<crate::supervisor::ProjectSnapshot> {
    let deadline = Instant::now() + limit;
    loop {
        match supervisor.snapshot() {
            Some(snapshot) if done(&snapshot) => return Ok(snapshot),
            Some(_) => {}
            None => bail!("the Supervisor stopped before startup completed"),
        }
        if Instant::now() >= deadline {
            bail!("startup did not finish within its bound");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Flatten a terminal snapshot into plain text for marker assertions.
fn buffer_text(snapshot: &crate::terminal::OwnedTerminalSnapshot) -> String {
    snapshot
        .buffer
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn shutdown(supervisor: crate::supervisor::SupervisorHandle) -> Result<()> {
    supervisor.command(Command::StopAll);
    let snapshot = wait_for(&supervisor, SHUTDOWN_WAIT, |snapshot| {
        snapshot.processes.iter().all(|p| p.current_run.is_none())
    })?;
    for process in &snapshot.processes {
        assert!(
            process.failure.is_none(),
            "Process {} reported a cleanup failure: {:?}",
            process.name,
            process.failure
        );
    }
    supervisor.stop_task();
    Ok(())
}
