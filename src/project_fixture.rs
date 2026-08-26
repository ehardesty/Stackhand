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

/// Console text that proves inline environment reached the child. The
/// fixture configuration in `tests/project_fixture.rs` prints it from
/// `$FIXTURE_TOKEN`.
const ENV_PROOF: &str = "fixture-token-stackhand-env-ok";

/// Console text that proves a shell pipeline ran end to end: the first
/// stage's output only appears after the second stage transforms it.
const PIPELINE_PROOF: &str = "FIXTURE-PIPELINE-LOWER";

/// One expected console proof per configured Process. This harness and the
/// integration-test configurations in `tests/project_fixture.rs` form one
/// contract.
const CONSOLE_PROOFS: &[(&str, &str)] = &[
    // A direct command reaches the child with its documented meaning.
    ("hello", "fixture-marker"),
    // Inline environment reaches the child.
    ("hello", ENV_PROOF),
    // Shell command text runs through the user's shell as one pipeline.
    ("shelled", PIPELINE_PROOF),
];

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

    // Output flows to each Process console without entering the control
    // plane; every proof must appear in its own Process's console.
    for (name, needle) in CONSOLE_PROOFS {
        let index = snapshot
            .processes
            .iter()
            .position(|process| process.name == *name)
            .unwrap_or_else(|| panic!("Process {name} is part of the fixture contract"));
        let run_id = snapshot.processes[index]
            .current_run
            .unwrap_or_else(|| panic!("Process {name} has an active Run"));
        let view = consoles
            .view(index as u32, run_id)
            .ok_or_else(|| anyhow!("no live console view for {name}"))?;
        wait_for_console_text(view, needle)?;
    }
    println!("fixture-output-ok");
    Ok(())
}

fn wait_for_console_text(view: crate::supervisor::ConsoleView, needle: &str) -> Result<()> {
    let deadline = Instant::now() + OUTPUT_WAIT;
    loop {
        if view
            .snapshot()
            .is_some_and(|snapshot| buffer_text(&snapshot).contains(needle))
        {
            break;
        }
        if Instant::now() >= deadline {
            bail!("the fixture proof '{needle}' never reached the console");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
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
