//! The integrated Project fixture: a headless proof of the full Milestone 2
//! user path through production configuration, Supervisor, Run adapters,
//! readiness and liveness checks, output, and controlled shutdown.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail, ensure};

use crate::supervisor::{
    Command, DesiredState, FailureKind, Lifecycle, OutputViews, ProcessId, ProcessSnapshot,
    ProjectSnapshot, ReadinessCheckKind, ReadinessState, SupervisorHandle,
};

const STARTUP_WAIT: Duration = Duration::from_secs(20);
const TRANSITION_WAIT: Duration = Duration::from_secs(15);
const OUTPUT_WAIT: Duration = Duration::from_secs(10);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(25);

/// Console text that proves inline environment reached the child.
const ENV_PROOF: &str = "fixture-token-stackhand-env-ok";

/// Console text that proves a shell pipeline ran end to end.
const PIPELINE_PROOF: &str = "FIXTURE-PIPELINE-LOWER";

/// One expected console proof per configured PTY Process.
const CONSOLE_PROOFS: &[(&str, &str)] = &[
    ("hello", "fixture-marker"),
    ("hello", ENV_PROOF),
    ("shelled", PIPELINE_PROOF),
];

/// Retained pipe-mode proofs. The marker keeps the Run identity visible.
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

/// Run the integrated fixture from one production YAML Project.
pub fn run(config_path: &Path) -> Result<()> {
    let resolution =
        crate::config::resolve(crate::config::ResolutionRequest::explicit(config_path))
            .map_err(|error| anyhow!("configuration error: {error}"))?;
    let (supervisor, consoles, outputs) = crate::supervisor::start(resolution.into_project())?;
    let output_lifetime = std::sync::Arc::downgrade(&outputs);
    supervisor.command(Command::StartAutostart);

    let descendant_pid = prove_slice(&supervisor, &consoles, &outputs)?;
    shutdown(supervisor)?;
    wait_for_pid_exit(descendant_pid)?;

    drop(consoles);
    drop(outputs);
    ensure!(
        output_lifetime.upgrade().is_none(),
        "the Project output owner remained after shutdown"
    );
    checkpoint("fixture-shutdown-ok");
    Ok(())
}

fn prove_slice(
    supervisor: &SupervisorHandle,
    consoles: &crate::supervisor::Consoles,
    outputs: &OutputViews,
) -> Result<u32> {
    // Configuration order puts this dependent before its One-shot. Its
    // snapshot proves a failed completion dependency is visible and remains
    // desired-running until the One-shot is rerun.
    wait_for(supervisor, STARTUP_WAIT, |snapshot| {
        snapshot.named("rerun-dependent").is_some_and(|process| {
            process.lifecycle == Lifecycle::Waiting
                && process.blocked_reason.as_deref() == Some("rerun-setup: completed_successfully")
        })
    })?;
    checkpoint("fixture-blocked-ok");

    // Wait for the stable startup evidence. The processes that intentionally
    // fail are checked below through their structured snapshots instead of
    // being treated as startup errors for the fixture itself.
    let snapshot = wait_for(supervisor, STARTUP_WAIT, |snapshot| {
        running(snapshot, "started-source")
            && running(snapshot, "started-dependent")
            && ready(snapshot, "tcp-ready", ReadinessCheckKind::Tcp)
            && ready(snapshot, "http-ready", ReadinessCheckKind::Http)
            && ready(snapshot, "exec-ready", ReadinessCheckKind::Exec)
            && ready(snapshot, "log-ready", ReadinessCheckKind::Log)
            && ready(snapshot, "all-ready", ReadinessCheckKind::All)
            && ready(snapshot, "recovering", ReadinessCheckKind::Http)
            && running(snapshot, "ready-dependent")
            && liveness_passing(snapshot, "liveness-recover")
            && liveness_passing(snapshot, "liveness-restart")
            && running(snapshot, "hello")
            && running(snapshot, "shelled")
            && running(snapshot, "piped")
            && running(snapshot, "noisy")
            && completed(snapshot, "accepted", Some(42))
            && running(snapshot, "completed-dependent")
            && failed_exit(snapshot, "exited-source", Some(7))
            && running(snapshot, "exited-dependent")
            && failed_exit(snapshot, "rerun-setup", Some(7))
            && waiting(snapshot, "rerun-dependent")
            && startup_timed_out(snapshot, "startup-timeout")
            && snapshot.named("shutdown-restart").is_some_and(|process| {
                process.lifecycle == Lifecycle::RestartBackoff
                    && process.current_run.is_none()
                    && process.automatic_restart_budget.automatic_retries_used == 0
                    && process
                        .restart_backoff
                        .as_ref()
                        .is_some_and(|backoff| backoff.reason == "failed Run")
            })
    })?;

    assert_startup_snapshot(&snapshot)?;
    checkpoint("fixture-started-ok");

    prove_console_output(&snapshot, consoles)?;
    checkpoint("fixture-output-ok");
    let descendant_pid = prove_pipe_output(&snapshot, outputs)?;
    prove_noisy_output(outputs, process(&snapshot, "noisy").process_id)?;
    checkpoint("fixture-pipe-output-ok");

    let timeout_pid = retained_pid(
        outputs,
        process(&snapshot, "startup-timeout").process_id,
        "startup-timeout",
        "fixture-timeout-descendant-pid-",
    )?;
    wait_for_pid_exit(timeout_pid)?;
    checkpoint("fixture-startup-timeout-ok");

    // A ready Service can lose readiness and recover in place. The parent
    // executable fixture changes the real HTTP endpoint after each marker.
    let recovering_run = current_run(&snapshot, "recovering")?;
    let dependent_run = current_run(&snapshot, "ready-dependent")?;
    checkpoint("fixture-readiness-ready");
    let failing = wait_for(supervisor, TRANSITION_WAIT, |snapshot| {
        snapshot.named("recovering").is_some_and(|process| {
            process.lifecycle == Lifecycle::Running
                && process.current_run == Some(recovering_run)
                && process
                    .readiness
                    .as_ref()
                    .is_some_and(|readiness| readiness.state == ReadinessState::Failing)
        })
    })?;
    let failing_readiness = failing
        .named("recovering")
        .and_then(|process| process.readiness.as_ref())
        .expect("the failing readiness status remains visible");
    ensure!(
        failing_readiness
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("503")),
        "readiness loss did not retain the endpoint diagnostic: {failing_readiness:?}"
    );
    ensure!(
        failing
            .named("ready-dependent")
            .is_some_and(|process| process.current_run == Some(dependent_run)),
        "readiness loss stopped an already-running dependent"
    );
    checkpoint("fixture-readiness-failing");
    let recovered = wait_for(supervisor, TRANSITION_WAIT, |snapshot| {
        snapshot.named("recovering").is_some_and(|process| {
            process.lifecycle == Lifecycle::Running
                && process.current_run == Some(recovering_run)
                && process
                    .readiness
                    .as_ref()
                    .is_some_and(|readiness| readiness.state == ReadinessState::Passing)
        })
    })?;
    ensure!(
        recovered
            .named("ready-dependent")
            .is_some_and(|process| process.current_run == Some(dependent_run)),
        "readiness recovery changed the already-running dependent"
    );
    checkpoint("fixture-readiness-recovered");

    // Liveness uses the same real adapter but starts after effective
    // readiness. With unhealthy restart disabled, health can fail and
    // recover without changing the Run identity.
    let liveness_run = current_run(&recovered, "liveness-recover")?;
    checkpoint("fixture-liveness-ready");
    let unhealthy = wait_for(supervisor, TRANSITION_WAIT, |snapshot| {
        snapshot.named("liveness-recover").is_some_and(|process| {
            process.lifecycle == Lifecycle::Running
                && process.current_run == Some(liveness_run)
                && process.liveness.as_ref().is_some_and(|liveness| {
                    liveness.state == crate::supervisor::LivenessState::Failing
                })
        })
    })?;
    let unhealthy_process = unhealthy
        .named("liveness-recover")
        .expect("the unhealthy Service exists");
    ensure!(
        unhealthy_process
            .failure
            .as_ref()
            .is_some_and(|failure| failure.kind == FailureKind::Liveness),
        "liveness failure is not structured: {unhealthy_process:?}"
    );
    checkpoint("fixture-liveness-failing");
    let healthy = wait_for(supervisor, TRANSITION_WAIT, |snapshot| {
        snapshot.named("liveness-recover").is_some_and(|process| {
            process.lifecycle == Lifecycle::Running
                && process.current_run == Some(liveness_run)
                && process.liveness.as_ref().is_some_and(|liveness| {
                    liveness.state == crate::supervisor::LivenessState::Passing
                })
        })
    })?;
    ensure!(
        healthy
            .named("liveness-recover")
            .is_some_and(|process| process.failure.is_none()),
        "liveness recovery left a stale failure visible"
    );
    checkpoint("fixture-liveness-recovered");

    // An unhealthy Service with explicit recovery enabled is stopped and
    // replaced. The parent changes its real endpoint after the bounded
    // backoff is visible, so the replacement can become healthy.
    let first_unhealthy_run = current_run(&healthy, "liveness-restart")?;
    checkpoint("fixture-unhealthy-restart-ready");
    let backoff = wait_for(supervisor, TRANSITION_WAIT, |snapshot| {
        snapshot.named("liveness-restart").is_some_and(|process| {
            process.lifecycle == Lifecycle::RestartBackoff
                && process.current_run.is_none()
                && process
                    .restart_backoff
                    .as_ref()
                    .is_some_and(|backoff| backoff.reason == "unhealthy")
        })
    })?;
    let backoff_process = backoff
        .named("liveness-restart")
        .expect("the unhealthy restart Service exists");
    ensure!(
        backoff_process
            .automatic_restart_budget
            .automatic_retries_used
            == 0,
        "the unhealthy retry was counted before its replacement started"
    );
    checkpoint("fixture-unhealthy-restart-backoff");
    let restarted = wait_for(supervisor, TRANSITION_WAIT, |snapshot| {
        snapshot.named("liveness-restart").is_some_and(|process| {
            process.lifecycle == Lifecycle::Running
                && process
                    .current_run
                    .is_some_and(|run| run != first_unhealthy_run)
                && process.liveness.as_ref().is_some_and(|liveness| {
                    liveness.state == crate::supervisor::LivenessState::Passing
                })
        })
    })?;
    let restarted_process = restarted
        .named("liveness-restart")
        .expect("the replacement Service exists");
    assert_eq!(
        restarted_process
            .automatic_restart_budget
            .automatic_retries_used,
        1
    );
    checkpoint("fixture-unhealthy-restart-recovered");

    // The budget Process fails repeatedly through the real runtime. Its
    // output and bounded recent history retain one marker per Run.
    let exhausted = wait_for(supervisor, STARTUP_WAIT, |snapshot| {
        snapshot.named("budget").is_some_and(|process| {
            process.lifecycle == Lifecycle::Stopped
                && process.current_run.is_none()
                && process
                    .failure
                    .as_ref()
                    .is_some_and(|failure| failure.kind == FailureKind::RestartLimit)
                && process.automatic_restart_budget.exhausted
        })
    })?;
    let budget = exhausted
        .named("budget")
        .expect("the budget Process exists");
    assert_eq!(budget.automatic_restart_budget.automatic_retries_used, 2);
    assert_eq!(budget.automatic_restart_budget.max_restarts, 2);
    assert_eq!(budget.recent_runs.len(), 3);
    assert!(
        budget
            .failure
            .as_ref()
            .is_some_and(|failure| failure.detail.contains("Restart limit"))
    );
    prove_restart_output(outputs, budget.process_id)?;
    checkpoint("fixture-restart-budget-ok");

    // The failed One-shot remains desired-running through its blocked
    // dependent. Rerunning it changes the completion source, and the
    // dependent starts without another command for the dependent.
    supervisor.command(Command::Rerun("rerun-setup".to_string()));
    let rerun = wait_for(supervisor, TRANSITION_WAIT, |snapshot| {
        snapshot.named("rerun-setup").is_some_and(|process| {
            process.lifecycle == Lifecycle::Done && process.current_run.is_none()
        }) && snapshot.named("rerun-dependent").is_some_and(|process| {
            process.lifecycle == Lifecycle::Running && process.current_run.is_some()
        })
    })?;
    let rerun_setup = rerun
        .named("rerun-setup")
        .expect("the rerun One-shot exists");
    assert_eq!(rerun_setup.recent_runs.len(), 2);
    assert_eq!(
        rerun_setup.recent_runs[0].trigger,
        crate::supervisor::RunTrigger::Rerun
    );
    assert_eq!(
        rerun
            .named("rerun-dependent")
            .and_then(|process| process.blocked_reason.as_deref()),
        None
    );
    checkpoint("fixture-rerun-recovered");

    Ok(descendant_pid)
}

fn assert_startup_snapshot(snapshot: &ProjectSnapshot) -> Result<()> {
    let started_source = process(snapshot, "started-source");
    let started_dependent = process(snapshot, "started-dependent");
    ensure!(
        started_dependent.run_started_at_ms >= started_source.run_started_at_ms,
        "started dependency order was not preserved"
    );

    let all = process(snapshot, "all-ready");
    let readiness = all.readiness.as_ref().expect("all readiness is visible");
    assert_eq!(readiness.children.len(), 2);
    assert_eq!(readiness.children[0].kind, ReadinessCheckKind::Tcp);
    assert_eq!(readiness.children[1].kind, ReadinessCheckKind::Http);
    assert!(
        readiness
            .children
            .iter()
            .all(|child| child.state == ReadinessState::Passing)
    );
    let accepted = process(snapshot, "accepted");
    assert_eq!(accepted.desired, DesiredState::Stopped);
    assert_eq!(accepted.failure, None);

    let rerun_dependent = process(snapshot, "rerun-dependent");
    assert_eq!(rerun_dependent.desired, DesiredState::Running);
    assert!(
        rerun_dependent
            .blocked_reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("rerun-setup: completed_successfully"))
    );

    let pending_restart = process(snapshot, "shutdown-restart");
    assert_eq!(pending_restart.recent_runs.len(), 1);
    assert_eq!(pending_restart.automatic_restart_budget.max_restarts, 2);

    let recovering = process(snapshot, "recovering");
    assert!(recovering.readiness.as_ref().unwrap().attempts > 0);
    let liveness = process(snapshot, "liveness-recover")
        .liveness
        .as_ref()
        .expect("liveness diagnostics are visible");
    assert!(liveness.attempts > 0);

    let manual = process(snapshot, "manual");
    assert_eq!(manual.desired, DesiredState::Stopped);
    assert_eq!(manual.current_run, None);
    let disabled = process(snapshot, "off");
    assert!(!disabled.enabled);
    assert_eq!(disabled.current_run, None);
    Ok(())
}

fn prove_console_output(
    snapshot: &ProjectSnapshot,
    consoles: &crate::supervisor::Consoles,
) -> Result<()> {
    for (name, needle) in CONSOLE_PROOFS {
        let process = process(snapshot, name);
        let run_id = current_run(snapshot, name)?;
        let view = consoles
            .view_process(process.process_id, run_id)
            .ok_or_else(|| anyhow!("no live console view for {name}"))?;
        wait_for_console_text(view, needle)?;
    }
    Ok(())
}

fn prove_pipe_output(snapshot: &ProjectSnapshot, outputs: &OutputViews) -> Result<u32> {
    for (name, needle, stream) in PIPE_PROOFS {
        let process = process(snapshot, name);
        let run_id = current_run(snapshot, name)?;
        let module = outputs
            .for_process_id(process.process_id)
            .ok_or_else(|| anyhow!("no retained output module for {name}"))?;
        wait_for_retained_text(&module, *stream, needle, run_id)?;
    }
    let process = process(snapshot, "piped");
    retained_pid(
        outputs,
        process.process_id,
        process.name.as_str(),
        "fixture-descendant-pid-",
    )
}

fn prove_noisy_output(outputs: &OutputViews, process_id: ProcessId) -> Result<()> {
    let module = outputs
        .for_process_id(process_id)
        .ok_or_else(|| anyhow!("the noisy Process output module is missing"))?;
    let snapshot = module.snapshot();
    let bytes = retained_bytes(&snapshot);
    ensure!(
        bytes <= crate::supervisor::RETAINED_BYTES,
        "noisy Process output exceeded its bound: {bytes}"
    );
    ensure!(
        snapshot.chunks.iter().any(|chunk| {
            matches!(chunk, crate::supervisor::RetainedChunk::Data { text, .. } if text.contains("fixture-noisy"))
        }),
        "noisy Process output did not reach retained history"
    );
    Ok(())
}

fn prove_restart_output(outputs: &OutputViews, process_id: ProcessId) -> Result<()> {
    let module = outputs
        .for_process_id(process_id)
        .ok_or_else(|| anyhow!("the restart Process output module is missing"))?;
    let snapshot = module.snapshot();
    let marker_count = snapshot
        .chunks
        .iter()
        .filter(|chunk| matches!(chunk, crate::supervisor::RetainedChunk::Marker { .. }))
        .count();
    ensure!(
        marker_count >= 3,
        "restart output lost Run boundaries: {snapshot:?}"
    );
    ensure!(
        retained_bytes(&snapshot) <= crate::supervisor::RETAINED_BYTES,
        "restart output exceeded its bound"
    );
    Ok(())
}

fn retained_pid(
    outputs: &OutputViews,
    process_id: ProcessId,
    name: &str,
    prefix: &str,
) -> Result<u32> {
    let module = outputs
        .for_process_id(process_id)
        .ok_or_else(|| anyhow!("the {name} output module is missing"))?;
    let deadline = Instant::now() + OUTPUT_WAIT;
    loop {
        let snapshot = module.snapshot();
        if let Some(pid) = snapshot.chunks.iter().find_map(|chunk| match chunk {
            crate::supervisor::RetainedChunk::Data { text, .. } => text
                .split(prefix)
                .nth(1)
                .and_then(|value| value.lines().next())
                .and_then(|value| value.trim().parse::<u32>().ok()),
            crate::supervisor::RetainedChunk::Marker { .. } => None,
        }) {
            return Ok(pid);
        }
        if Instant::now() >= deadline {
            bail!("the {name} output did not contain a PID marker: {snapshot:?}");
        }
        std::thread::sleep(POLL);
    }
}

fn retained_bytes(snapshot: &crate::supervisor::RetainedOutput) -> usize {
    snapshot
        .chunks
        .iter()
        .map(|chunk| match chunk {
            crate::supervisor::RetainedChunk::Data { text, .. } => text.len(),
            crate::supervisor::RetainedChunk::Marker { label, .. } => label.len(),
        })
        .sum()
}

fn wait_for_retained_text(
    module: &crate::supervisor::ProcessOutput,
    stream: crate::runtime::OutputStream,
    needle: &str,
    run_id: u64,
) -> Result<()> {
    let deadline = Instant::now() + OUTPUT_WAIT;
    loop {
        let snapshot = module.snapshot();
        let marker_present = snapshot.chunks.iter().any(|chunk| {
            matches!(chunk, crate::supervisor::RetainedChunk::Marker { run_id: marked, .. } if *marked == run_id)
        });
        let proof_present = snapshot.chunks.iter().any(|chunk| {
            matches!(
                chunk,
                crate::supervisor::RetainedChunk::Data {
                    run_id: marked,
                    stream: chunk_stream,
                    text,
                    ..
                } if *marked == run_id && *chunk_stream == stream && text.contains(needle)
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
        std::thread::sleep(POLL);
    }
}

fn wait_for_console_text(view: crate::supervisor::ConsoleView, needle: &str) -> Result<()> {
    let deadline = Instant::now() + OUTPUT_WAIT;
    loop {
        if view
            .snapshot()
            .is_some_and(|snapshot| buffer_text(&snapshot).contains(needle))
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("the fixture proof '{needle}' never reached the console");
        }
        std::thread::sleep(POLL);
    }
}

fn shutdown(supervisor: SupervisorHandle) -> Result<()> {
    supervisor.command(Command::Shutdown {
        deadline: Instant::now() + SHUTDOWN_WAIT,
    });
    let snapshot = wait_for(&supervisor, SHUTDOWN_WAIT, |snapshot| {
        snapshot
            .shutdown
            .as_ref()
            .is_some_and(|result| result.complete)
    })?;
    let result = snapshot.shutdown.as_ref().expect("shutdown completed");
    ensure!(!result.timed_out, "Project shutdown reached its deadline");
    ensure!(
        result.failures.is_empty(),
        "Project shutdown reports cleanup failures: {:?}",
        result.failures
    );
    for process in &snapshot.processes {
        ensure!(
            process
                .failure
                .as_ref()
                .is_none_or(|failure| failure.kind != FailureKind::Shutdown),
            "Process {} reported a cleanup failure: {:?}",
            process.name,
            process.failure
        );
        ensure!(
            process.restart_backoff.is_none(),
            "Project shutdown left an automatic restart pending for {}",
            process.name
        );
    }
    let suppressed = process(&snapshot, "shutdown-restart");
    ensure!(
        suppressed.recent_runs.len() == 1
            && suppressed.automatic_restart_budget.automatic_retries_used == 0,
        "Project shutdown restarted a pending retry: {suppressed:?}"
    );
    supervisor.stop_task();
    Ok(())
}

fn wait_for_pid_exit(pid: u32) -> Result<()> {
    let deadline = Instant::now() + SHUTDOWN_WAIT;
    loop {
        // SAFETY: signal 0 only probes whether this fixture-owned PID exists.
        let status = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if status == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("ordinary descendant PID {pid} remained after Project shutdown");
        }
        std::thread::sleep(POLL);
    }
}

fn wait_for(
    supervisor: &SupervisorHandle,
    limit: Duration,
    done: impl Fn(&ProjectSnapshot) -> bool,
) -> Result<ProjectSnapshot> {
    crate::sync_fixture::wait_for_snapshot(
        supervisor,
        crate::sync_fixture::SnapshotWait {
            timeout: limit,
            poll_interval: POLL,
            stopped_message: "the Supervisor stopped before the fixture proof completed",
            timeout_message: "the integrated Project fixture exceeded its bound",
        },
        done,
    )
}

fn process<'a>(snapshot: &'a ProjectSnapshot, name: &str) -> &'a ProcessSnapshot {
    snapshot
        .named(name)
        .unwrap_or_else(|| panic!("the fixture defines Process '{name}'"))
}

fn current_run(snapshot: &ProjectSnapshot, name: &str) -> Result<u64> {
    process(snapshot, name)
        .current_run
        .ok_or_else(|| anyhow!("Process '{name}' has no current Run"))
}

fn running(snapshot: &ProjectSnapshot, name: &str) -> bool {
    process(snapshot, name).lifecycle == Lifecycle::Running
        && process(snapshot, name).current_run.is_some()
}

fn waiting(snapshot: &ProjectSnapshot, name: &str) -> bool {
    process(snapshot, name).lifecycle == Lifecycle::Waiting
}

fn ready(snapshot: &ProjectSnapshot, name: &str, kind: ReadinessCheckKind) -> bool {
    let process = process(snapshot, name);
    running(snapshot, name)
        && process.readiness.as_ref().is_some_and(|readiness| {
            readiness.kind == kind && readiness.state == ReadinessState::Passing
        })
}

fn liveness_passing(snapshot: &ProjectSnapshot, name: &str) -> bool {
    let process = process(snapshot, name);
    running(snapshot, name)
        && process
            .liveness
            .as_ref()
            .is_some_and(|liveness| liveness.state == crate::supervisor::LivenessState::Passing)
}

fn completed(snapshot: &ProjectSnapshot, name: &str, code: Option<i32>) -> bool {
    let process = process(snapshot, name);
    process.lifecycle == Lifecycle::Done
        && process.current_run.is_none()
        && process.recent_runs.first().is_some_and(|run| {
            run.exit == crate::supervisor::RunExitDisposition::Success && run.exit_code == code
        })
}

fn failed_exit(snapshot: &ProjectSnapshot, name: &str, code: Option<i32>) -> bool {
    let process = process(snapshot, name);
    process.lifecycle == Lifecycle::Stopped
        && process.current_run.is_none()
        && process
            .failure
            .as_ref()
            .is_some_and(|failure| failure.kind == FailureKind::ProcessExit)
        && process
            .recent_runs
            .first()
            .is_some_and(|run| run.exit == crate::supervisor::RunExitDisposition::Failed { code })
}

fn startup_timed_out(snapshot: &ProjectSnapshot, name: &str) -> bool {
    let process = process(snapshot, name);
    process.lifecycle == Lifecycle::Stopped
        && process.current_run.is_none()
        && process.failure.as_ref().is_some_and(|failure| {
            failure.kind == FailureKind::Readiness && failure.detail.contains("startup timeout")
        })
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

fn checkpoint(label: &str) {
    println!("{label}");
    let _ = std::io::stdout().flush();
}
