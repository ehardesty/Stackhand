//! Headless smoke and repeated-cycle proof for a small real Project.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

use crate::supervisor::{Command, Lifecycle};

const SMOKE_WAIT: Duration = Duration::from_secs(30);
const REAL_PROJECT_CYCLES: usize = 3;

pub fn run(config_path: &Path) -> Result<()> {
    let project = crate::config::load(config_path)
        .map_err(|error| anyhow!("configuration error: {error}"))?;
    let baseline_fds = open_fd_count();
    let baseline_threads = thread_count();

    for _ in 0..REAL_PROJECT_CYCLES {
        run_cycle(project.clone())?;
    }
    std::thread::sleep(Duration::from_millis(100));
    assert_resource_convergence("file descriptors", baseline_fds, open_fd_count())?;
    assert_resource_convergence("threads", baseline_threads, thread_count())?;
    println!("real-project-cycles-ok");
    println!("real-project-smoke-ok");
    Ok(())
}

fn run_cycle(project: crate::model::EffectiveProject) -> Result<()> {
    let (supervisor, _consoles, outputs) = crate::supervisor::start(project)?;
    let output_lifetime = std::sync::Arc::downgrade(&outputs);
    supervisor.command(Command::StartAutostart);

    let snapshot = wait_for(&supervisor, |snapshot| {
        snapshot.named("inspect").is_some_and(|process| {
            process.lifecycle == Lifecycle::Done && process.current_run.is_none()
        }) && snapshot
            .named("hold")
            .is_some_and(|process| process.lifecycle == Lifecycle::Running)
    })?;
    let inspect_index = snapshot
        .processes
        .iter()
        .position(|process| process.name == "inspect")
        .expect("the smoke Project defines inspect");
    let retained = outputs
        .for_process(inspect_index as u32)
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
    if !output.contains("workspace_root") {
        bail!("the real Project inspection output was not retained");
    }
    let retained_bytes: usize = retained
        .chunks
        .iter()
        .map(|chunk| match chunk {
            crate::output::RetainedChunk::Data { text, .. } => text.len(),
            crate::output::RetainedChunk::Marker { label, .. } => label.len(),
        })
        .sum();
    if retained_bytes > crate::supervisor::RETAINED_BYTES {
        bail!("the real Project output exceeded its memory bound");
    }

    supervisor.command(Command::Shutdown {
        deadline: Instant::now() + SMOKE_WAIT,
    });
    let result = wait_for(&supervisor, |snapshot| {
        snapshot
            .shutdown
            .as_ref()
            .is_some_and(|result| result.complete)
    })?
    .shutdown
    .expect("shutdown completed");
    supervisor.stop_task();
    drop(outputs);
    if output_lifetime.upgrade().is_some() {
        bail!("the Project output memory owner remained after shutdown");
    }
    if !result.failures.is_empty() {
        bail!(
            "the real Project smoke shutdown failed: {:?}",
            result.failures
        );
    }
    Ok(())
}

fn assert_resource_convergence(
    resource: &str,
    before: Option<usize>,
    after: Option<usize>,
) -> Result<()> {
    if let (Some(before), Some(after)) = (before, after)
        && after > before + 2
    {
        bail!("{resource} did not converge: before {before}, after {after}");
    }
    Ok(())
}

fn open_fd_count() -> Option<usize> {
    std::fs::read_dir("/dev/fd").ok().map(Iterator::count)
}

#[cfg(target_os = "macos")]
fn thread_count() -> Option<usize> {
    let output = std::process::Command::new("ps")
        .args(["-M", "-p", &std::process::id().to_string(), "-o", "pid="])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).lines().count())
}

#[cfg(target_os = "linux")]
fn thread_count() -> Option<usize> {
    std::fs::read_dir("/proc/self/task")
        .ok()
        .map(Iterator::count)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn thread_count() -> Option<usize> {
    None
}

fn wait_for(
    supervisor: &crate::supervisor::SupervisorHandle,
    done: impl Fn(&crate::supervisor::ProjectSnapshot) -> bool,
) -> Result<crate::supervisor::ProjectSnapshot> {
    let deadline = Instant::now() + SMOKE_WAIT;
    loop {
        match supervisor.snapshot() {
            Some(snapshot) if done(&snapshot) => return Ok(snapshot),
            Some(_) => {}
            None => bail!("the Supervisor stopped before the smoke proof completed"),
        }
        if Instant::now() >= deadline {
            bail!("the real Project smoke proof exceeded its bound");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
