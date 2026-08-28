//! The integrated Project fixture: a headless proof of the full vertical
//! slice through the production configuration, Supervisor, Run adapter, and
//! console view. It prints observable checkpoints the executable fixture
//! test asserts.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

use crate::supervisor::{Command, DesiredState, Lifecycle};

const STARTUP_WAIT: Duration = Duration::from_secs(15);
const OUTPUT_WAIT: Duration = Duration::from_secs(10);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(20);

/// The lifecycle each Process kind must reach during startup: Services run,
/// One-shots complete.
fn expected_startup_lifecycle(kind: crate::model::ProcessKind) -> Lifecycle {
    match kind {
        crate::model::ProcessKind::OneShot => Lifecycle::Done,
        crate::model::ProcessKind::Service => Lifecycle::Running,
    }
}

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
    // Shell command text runs through the Project's shell launcher as one
    // pipeline.
    ("shelled", PIPELINE_PROOF),
];

/// Retained pipe-mode proofs: the Process's bytes never enter the control
/// plane, so each needle must land in the bounded per-Process output module
/// with its stream identity intact.
const PIPE_PROOFS: &[(&str, &str, crate::runtime::OutputStream)] = &[
    (
        "piped",
        "fixture-pipe-out",
        crate::runtime::OutputStream::Stdout,
    ),
    (
        "piped",
        "fixture-pipe-err",
        crate::runtime::OutputStream::Stderr,
    ),
];

pub fn run(config_path: &Path) -> Result<()> {
    let project = crate::config::load(config_path)
        .map_err(|error| anyhow!("configuration error: {error}"))?;
    let (supervisor, consoles, outputs) = crate::supervisor::start(project)?;
    supervisor.command(Command::StartAutostart);

    let descendant_pid = prove_slice(&supervisor, &consoles, &outputs)?;
    shutdown(supervisor)?;
    wait_for_pid_exit(descendant_pid)?;
    println!("fixture-shutdown-ok");
    Ok(())
}

fn prove_slice(
    supervisor: &crate::supervisor::SupervisorHandle,
    consoles: &crate::supervisor::Consoles,
    outputs: &crate::output::OutputViews,
) -> Result<u32> {
    // Configuration order puts the started-condition dependent before its
    // Dependency, so its first serialized snapshot proves the exact edge.
    let started_blocked = wait_for(supervisor, STARTUP_WAIT, |snapshot| {
        snapshot.named("gated-started").is_some_and(|process| {
            process.lifecycle == Lifecycle::Waiting
                && process.blocked_reason.as_deref() == Some("hello: started")
        })
    })?;
    assert_eq!(
        started_blocked
            .named("gated-started")
            .and_then(|process| process.blocked_reason.as_deref()),
        Some("hello: started"),
        "the started-condition dependent names its graph edge"
    );

    // Slow readiness and One-shot completion keep the other dependents
    // Waiting long enough to prove their graph diagnostics.
    wait_for(supervisor, STARTUP_WAIT, |snapshot| {
        snapshot.named("gated").is_some_and(|process| {
            process.lifecycle == Lifecycle::Waiting
                && process.blocked_reason.as_deref() == Some("setup: completed_successfully")
        }) && snapshot.named("gated-ready").is_some_and(|process| {
            process.lifecycle == Lifecycle::Waiting
                && process.blocked_reason.as_deref() == Some("http-ready: ready")
        })
    })?;
    println!("fixture-blocked-ok");

    // Every enabled autostart Process reaches its terminal-for-now state:
    // Services run, One-shots complete. Starting becomes Running on the
    // Spawned event; a One-shot becomes Done once its natural exit is
    // observed, and this waits past its completion report so the Run identity
    // has cleared.
    let snapshot = wait_for(supervisor, STARTUP_WAIT, |snapshot| {
        snapshot.processes.iter().all(|process| {
            !process.enabled
                || !process.autostart
                || (process.lifecycle == expected_startup_lifecycle(process.kind)
                    && (process.lifecycle != Lifecycle::Done || process.current_run.is_none()))
        })
    })?;
    for process in &snapshot.processes {
        if process.enabled && process.autostart {
            assert_eq!(
                process.lifecycle,
                expected_startup_lifecycle(process.kind),
                "Process {} did not reach its expected lifecycle",
                process.name
            );
        }
    }
    println!("fixture-started-ok");

    // Enabled non-autostart Processes stay stopped and available for a
    // manual start; disabled Processes stay visible without any Run.
    let manual = snapshot
        .named("manual")
        .expect("the fixture defines 'manual'");
    assert!(manual.enabled && !manual.autostart);
    assert_eq!(manual.desired, DesiredState::Stopped);
    assert!(matches!(
        manual.lifecycle,
        Lifecycle::Idle | Lifecycle::Stopped
    ));
    assert_eq!(manual.current_run, None);
    let off = snapshot.named("off").expect("the fixture defines 'off'");
    assert!(!off.enabled);
    assert_eq!(off.current_run, None);

    // The One-shot completed through natural exit observation: it ends Done
    // with no failure and no desire to run again.
    let setup = snapshot
        .named("setup")
        .expect("the fixture defines 'setup'");
    assert_eq!(setup.lifecycle, Lifecycle::Done);
    assert_eq!(setup.desired, DesiredState::Stopped);
    assert_eq!(setup.failure, None);
    assert_eq!(setup.current_run, None);

    // Its dependent started only after `completed_successfully` held and is
    // an active Service Run now.
    let gated = snapshot
        .named("gated")
        .expect("the fixture defines 'gated'");
    assert_eq!(gated.lifecycle, Lifecycle::Running);
    assert!(gated.current_run.is_some());
    assert_eq!(gated.failure, None);
    let hello = snapshot.named("hello").expect("the fixture defines hello");
    let gated_started = snapshot
        .named("gated-started")
        .expect("the fixture defines gated-started");
    assert!(
        gated_started.run_started_at_ms >= hello.run_started_at_ms,
        "the started-condition dependent starts after its Dependency"
    );
    let setup_finished_at = setup
        .recent_runs
        .first()
        .expect("the setup Run is summarized")
        .ended_at_ms;
    assert!(
        gated
            .run_started_at_ms
            .is_some_and(|started| started >= setup_finished_at),
        "the completed-successfully dependent starts after the One-shot"
    );
    println!("fixture-one-shot-ok");

    // The TCP-probed Service reached Running only through a real probe pass
    // against the listener this process hosts; its Passing state stays
    // visible while later checks can detect loss and recovery.
    let tcp_ready = snapshot
        .named("tcp-ready")
        .expect("the fixture defines 'tcp-ready'");
    assert_eq!(tcp_ready.lifecycle, Lifecycle::Running);
    assert!(tcp_ready.current_run.is_some());
    assert_eq!(
        tcp_ready.readiness.as_ref().map(|status| status.state),
        Some(crate::supervisor::ReadinessState::Passing)
    );
    assert_eq!(tcp_ready.failure, None);
    // The same holds for the HTTP-probed Service against the real local
    // health endpoint this process hosts.
    let http_ready = snapshot
        .named("http-ready")
        .expect("the fixture defines 'http-ready'");
    assert_eq!(http_ready.lifecycle, Lifecycle::Running);
    assert!(http_ready.current_run.is_some());
    assert_eq!(
        http_ready.readiness.as_ref().map(|status| status.state),
        Some(crate::supervisor::ReadinessState::Passing)
    );
    assert_eq!(http_ready.failure, None);
    let gated_ready = snapshot
        .named("gated-ready")
        .expect("the fixture defines gated-ready");
    assert!(
        gated_ready.run_started_at_ms >= http_ready.run_started_at_ms,
        "the ready-condition dependent starts after the probed Service"
    );
    println!("fixture-tcp-ready-ok");

    // Output flows to each Process console without entering the control
    // plane; every proof must appear in its own Process's console.
    for (name, needle) in CONSOLE_PROOFS {
        let process = snapshot
            .named(name)
            .unwrap_or_else(|| panic!("Process {name} is part of the fixture contract"));
        let run_id = process
            .current_run
            .unwrap_or_else(|| panic!("Process {name} has an active Run"));
        let view = consoles
            .view_process(process.process_id, run_id)
            .ok_or_else(|| anyhow!("no live console view for {name}"))?;
        wait_for_console_text(view, needle)?;
    }
    println!("fixture-output-ok");

    // Pipe output stays out of the terminal sessions: it lands in the
    // bounded per-Process module with stream identity, under the Run
    // marker that divides attempts.
    for (name, needle, stream) in PIPE_PROOFS {
        let process = snapshot
            .named(name)
            .unwrap_or_else(|| panic!("Process {name} is part of the fixture contract"));
        let module = outputs
            .for_process_id(process.process_id)
            .ok_or_else(|| anyhow!("no retained output module for {name}"))?;
        let run_id = process
            .current_run
            .unwrap_or_else(|| panic!("Process {name} has an active Run"));
        wait_for_retained_text(&module, *stream, needle, Some(run_id))?;
    }
    println!("fixture-pipe-output-ok");
    let piped = snapshot.named("piped").expect("the fixture defines piped");
    let piped_output = outputs
        .for_process_id(piped.process_id)
        .expect("piped has retained output")
        .snapshot()
        .chunks
        .iter()
        .filter_map(|chunk| match chunk {
            crate::output::RetainedChunk::Data { text, .. } => Some(text.as_str()),
            crate::output::RetainedChunk::Marker { .. } => None,
        })
        .collect::<String>();
    let descendant_pid = piped_output
        .split("fixture-descendant-pid-")
        .nth(1)
        .and_then(|suffix| suffix.lines().next())
        .and_then(|pid| pid.trim().parse::<u32>().ok())
        .ok_or_else(|| anyhow!("the fixture did not report its descendant PID"))?;
    Ok(descendant_pid)
}

fn wait_for_pid_exit(pid: u32) -> Result<()> {
    let deadline = Instant::now() + SHUTDOWN_WAIT;
    loop {
        let status = unsafe { libc::kill(pid as i32, 0) };
        if status == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("ordinary descendant PID {pid} remained after Project shutdown");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_retained_text(
    module: &crate::output::ProcessOutput,
    stream: crate::runtime::OutputStream,
    needle: &str,
    run_id: Option<u64>,
) -> Result<()> {
    use crate::output::RetainedChunk;
    let deadline = Instant::now() + OUTPUT_WAIT;
    loop {
        let snapshot = module.snapshot();
        let marker_present = run_id.is_some_and(|run| {
            snapshot.chunks.iter().any(|chunk| {
                matches!(chunk, RetainedChunk::Marker { run_id: marked, .. } if *marked == run)
            })
        });
        let proof_present = snapshot.chunks.iter().any(|chunk| {
            matches!(
                chunk,
                RetainedChunk::Data { run_id: _, stream: chunk_stream, text, .. }
                    if *chunk_stream == stream && text.contains(needle)
            )
        });
        if marker_present && proof_present {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "the retained proof '{needle}' never reached the module (marker: {marker_present})"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
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
    crate::sync_fixture::wait_for_snapshot(
        supervisor,
        crate::sync_fixture::SnapshotWait {
            timeout: limit,
            poll_interval: Duration::from_millis(25),
            stopped_message: "the Supervisor stopped before startup completed",
            timeout_message: "startup did not finish within its bound",
        },
        done,
    )
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
    supervisor.command(Command::Shutdown {
        deadline: Instant::now() + SHUTDOWN_WAIT,
    });
    let snapshot = wait_for(&supervisor, SHUTDOWN_WAIT, |snapshot| {
        snapshot
            .shutdown
            .as_ref()
            .is_some_and(|result| result.complete)
    })?;
    assert!(
        snapshot
            .shutdown
            .as_ref()
            .is_some_and(|result| result.failures.is_empty()),
        "Project shutdown reports every cleanup failure"
    );
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
