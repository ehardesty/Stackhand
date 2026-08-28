//! Headless smoke and repeated-cycle proof for a small real Project.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail, ensure};

use crate::supervisor::{Command, Lifecycle, ReadinessState};

const SMOKE_WAIT: Duration = Duration::from_secs(30);
const RESOURCE_CONVERGENCE_WAIT: Duration = Duration::from_secs(10);
const RESOURCE_POLL: Duration = Duration::from_millis(25);
const RESOURCE_SLACK: usize = 2;
const REAL_PROJECT_CYCLES: usize = 3;

pub fn run(config_path: &Path) -> Result<()> {
    let project = crate::config::load(config_path)
        .map_err(|error| anyhow!("configuration error: {error}"))?;
    let baseline_fds = open_fd_count()?;
    let baseline_threads = thread_count()?;

    for cycle in 1..=REAL_PROJECT_CYCLES {
        run_cycle(project.clone(), cycle)?;
    }
    let final_fds = wait_for_resource_convergence("file descriptors", baseline_fds, open_fd_count)?;
    let final_threads = wait_for_resource_convergence("threads", baseline_threads, thread_count)?;
    checkpoint(&format!(
        "real-project-resources-ok: fds {baseline_fds} -> {final_fds}; threads {baseline_threads} -> {final_threads}; tolerance {RESOURCE_SLACK}"
    ));
    checkpoint(&format!("real-project-cycles-ok: {REAL_PROJECT_CYCLES}"));
    checkpoint("real-project-smoke-ok");
    Ok(())
}

fn run_cycle(project: crate::model::EffectiveProject, cycle: usize) -> Result<()> {
    let (supervisor, consoles, outputs) = crate::supervisor::start(project)?;
    let output_lifetime = std::sync::Arc::downgrade(&outputs);
    supervisor.command(Command::StartAutostart);

    let snapshot = wait_for(&supervisor, |snapshot| {
        snapshot.named("inspect").is_some_and(|process| {
            process.lifecycle == Lifecycle::Done && process.current_run.is_none()
        }) && snapshot
            .named("hold")
            .is_some_and(|process| process.lifecycle == Lifecycle::Running)
            && snapshot.named("ready-service").is_some_and(|process| {
                process.lifecycle == Lifecycle::Running
                    && process
                        .readiness
                        .as_ref()
                        .is_some_and(|readiness| readiness.state == ReadinessState::Passing)
            })
            && snapshot
                .named("ready-dependent")
                .is_some_and(|process| process.lifecycle == Lifecycle::Running)
    })?;
    let inspect = snapshot
        .named("inspect")
        .expect("the smoke Project defines inspect");
    let retained = outputs
        .for_process_id(inspect.process_id)
        .expect("inspect has retained output")
        .snapshot();
    let output = retained
        .chunks
        .iter()
        .filter_map(|chunk| match chunk {
            crate::output::RetainedChunk::Data { text, .. } => Some(text.as_str()),
            crate::output::RetainedChunk::Marker { .. } => None,
        })
        .collect::<String>();
    ensure!(
        output.contains("workspace_root"),
        "the real Project inspection output was not retained"
    );
    let retained_bytes: usize = retained
        .chunks
        .iter()
        .map(|chunk| match chunk {
            crate::output::RetainedChunk::Data { text, .. } => text.len(),
            crate::output::RetainedChunk::Marker { label, .. } => label.len(),
        })
        .sum();
    ensure!(
        retained_bytes <= crate::supervisor::RETAINED_BYTES,
        "the real Project output exceeded its memory bound"
    );

    let ready_service = snapshot
        .named("ready-service")
        .expect("the smoke Project defines ready-service");
    let readiness_run = ready_service
        .current_run
        .expect("ready-service has a current Run");
    let dependent_run = snapshot
        .named("ready-dependent")
        .and_then(|process| process.current_run)
        .expect("ready-dependent has a current Run");
    checkpoint("real-project-ready");

    let failing = wait_for(&supervisor, |snapshot| {
        snapshot.named("ready-service").is_some_and(|process| {
            process.lifecycle == Lifecycle::Running
                && process.current_run == Some(readiness_run)
                && process
                    .readiness
                    .as_ref()
                    .is_some_and(|readiness| readiness.state == ReadinessState::Failing)
        })
    })?;
    let failed_readiness = failing
        .named("ready-service")
        .and_then(|process| process.readiness.as_ref())
        .expect("failed readiness remains visible");
    ensure!(
        failed_readiness
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("503")),
        "the real readiness failure did not retain the HTTP diagnostic: {failed_readiness:?}"
    );
    ensure!(
        failing
            .named("ready-dependent")
            .is_some_and(|process| process.current_run == Some(dependent_run)),
        "readiness loss changed the already-running dependent"
    );
    checkpoint("real-project-failing");

    let recovered = wait_for(&supervisor, |snapshot| {
        snapshot.named("ready-service").is_some_and(|process| {
            process.lifecycle == Lifecycle::Running
                && process.current_run == Some(readiness_run)
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
    checkpoint("real-project-recovered");

    let hold = snapshot
        .named("hold")
        .expect("the smoke Project defines hold");
    let hold_pid = retained_pid(&outputs, hold.process_id)?;

    supervisor.command(Command::Shutdown {
        deadline: Instant::now() + SMOKE_WAIT,
    });
    let shutdown = wait_for(&supervisor, |snapshot| {
        snapshot
            .shutdown
            .as_ref()
            .is_some_and(|result| result.complete)
    })?;
    let result = shutdown.shutdown.expect("shutdown completed");
    ensure!(
        !result.timed_out,
        "the real Project smoke shutdown timed out"
    );
    ensure!(
        result.failures.is_empty(),
        "the real Project smoke shutdown failed: {:?}",
        result.failures
    );
    ensure!(
        shutdown.processes.iter().all(|process| {
            process.current_run.is_none()
                && matches!(process.lifecycle, Lifecycle::Stopped | Lifecycle::Done)
        }),
        "shutdown left an active Process: {:?}",
        shutdown.processes
    );

    supervisor.stop_task();
    drop(consoles);
    drop(outputs);
    ensure!(
        output_lifetime.upgrade().is_none(),
        "the Project output memory owner remained after shutdown"
    );
    wait_for_pid_exit(hold_pid)?;
    checkpoint(&format!("real-project-cycle-{cycle}-cleanup-ok"));
    Ok(())
}

fn wait_for_resource_convergence(
    resource: &str,
    before: usize,
    mut measure: impl FnMut() -> Result<usize>,
) -> Result<usize> {
    let deadline = Instant::now() + RESOURCE_CONVERGENCE_WAIT;
    loop {
        let after = measure()?;
        if after <= before + RESOURCE_SLACK {
            return Ok(after);
        }
        if Instant::now() >= deadline {
            bail!("{resource} did not converge: before {before}, after {after}");
        }
        std::thread::sleep(RESOURCE_POLL);
    }
}

fn retained_pid(
    outputs: &crate::supervisor::OutputViews,
    process_id: crate::supervisor::ProcessId,
) -> Result<u32> {
    let output = outputs
        .for_process_id(process_id)
        .ok_or_else(|| anyhow!("the hold Process output module is missing"))?;
    let deadline = Instant::now() + SMOKE_WAIT;
    loop {
        if let Some(pid) = output
            .snapshot()
            .chunks
            .iter()
            .find_map(|chunk| match chunk {
                crate::output::RetainedChunk::Data { text, .. } => text
                    .split("hold-child-")
                    .nth(1)
                    .and_then(|value| value.lines().next())
                    .and_then(|value| value.trim().parse::<u32>().ok()),
                crate::output::RetainedChunk::Marker { .. } => None,
            })
        {
            return Ok(pid);
        }
        if Instant::now() >= deadline {
            bail!("the hold Process output did not contain its child PID");
        }
        std::thread::sleep(RESOURCE_POLL);
    }
}

#[cfg(unix)]
fn wait_for_pid_exit(pid: u32) -> Result<()> {
    let deadline = Instant::now() + SMOKE_WAIT;
    loop {
        // SAFETY: signal 0 only probes this fixture-owned Process Tree member.
        let status = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if status == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            bail!("could not probe ordinary child PID {pid}: {error}");
        }
        if Instant::now() >= deadline {
            bail!("ordinary child PID {pid} remained after Project shutdown");
        }
        std::thread::sleep(RESOURCE_POLL);
    }
}

#[cfg(not(unix))]
fn wait_for_pid_exit(_pid: u32) -> Result<()> {
    bail!("ordinary Process Tree cleanup proof requires Unix")
}

fn checkpoint(label: &str) {
    println!("{label}");
    let _ = std::io::stdout().flush();
}

fn open_fd_count() -> Result<usize> {
    std::fs::read_dir("/dev/fd")
        .map(Iterator::count)
        .map_err(|error| anyhow!("could not count file descriptors: {error}"))
}

#[cfg(target_os = "macos")]
fn thread_count() -> Result<usize> {
    let output = std::process::Command::new("ps")
        .args(["-M", "-p", &std::process::id().to_string(), "-o", "pid="])
        .output()
        .map_err(|error| anyhow!("could not count threads with ps: {error}"))?;
    ensure!(
        output.status.success(),
        "could not count threads with ps: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).lines().count())
}

#[cfg(target_os = "linux")]
fn thread_count() -> Result<usize> {
    std::fs::read_dir("/proc/self/task")
        .map(Iterator::count)
        .map_err(|error| anyhow!("could not count threads: {error}"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn thread_count() -> Result<usize> {
    bail!("thread count is unavailable on this platform")
}

fn wait_for(
    supervisor: &crate::supervisor::SupervisorHandle,
    done: impl Fn(&crate::supervisor::ProjectSnapshot) -> bool,
) -> Result<crate::supervisor::ProjectSnapshot> {
    crate::sync_fixture::wait_for_snapshot(
        supervisor,
        crate::sync_fixture::SnapshotWait {
            timeout: SMOKE_WAIT,
            poll_interval: Duration::from_millis(25),
            stopped_message: "the Supervisor stopped before the smoke proof completed",
            timeout_message: "the real Project smoke proof exceeded its bound",
        },
        done,
    )
}
