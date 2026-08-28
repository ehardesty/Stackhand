//! Pressure evidence for the per-Process output module: several noisy pipe
//! Processes stay at their retention bound while lifecycle commands keep
//! flowing. Run through the production configuration, Supervisor, and Run
//! adapter, the same path the TUI uses.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::model::{
    Autostart, CommandForm, EffectiveProject, Enabled, InputPolicy, ProcessKind, ProcessSpec,
    RestartConfig, TerminalMode,
};
use crate::supervisor::{Command, Lifecycle};

/// How long each phase may take before the fixture reports a failure.
const PHASE_WAIT: Duration = Duration::from_secs(30);
/// A lifecycle command issued into a flooded output plane must still land
/// promptly; one noisy Process must not delay work for another Process.
const COMMAND_DEADLINE: Duration = Duration::from_secs(10);

/// The observed pressure results.
#[derive(Debug)]
pub struct OutputPressureReport {
    /// Noisy Processes that flooded their retained output.
    pub noisy_processes: usize,
    /// Processes whose retention bound actually truncated output.
    pub truncated_processes: usize,
    /// The largest retained byte count observed anywhere (bounded by
    /// construction; the proof is that it stayed at the bound).
    pub max_retained_bytes: usize,
    /// The largest retained chunk count observed anywhere.
    pub max_retained_chunks: usize,
    /// How long a stop command took while the noise continued, in ms.
    pub command_latency_ms: u128,
}

/// One noisy pipe Process: `yes` floods stdout until the Run stops.
fn noisy_process(name: &str) -> ProcessSpec {
    ProcessSpec {
        name: name.to_string(),
        kind: ProcessKind::Service,
        enabled: Enabled::Yes,
        autostart: Autostart::Yes,
        success_exit_codes: vec![0],
        restart: RestartConfig::default(),
        command: CommandForm::Direct {
            program: "yes".into(),
            args: vec!["stackhand-noise".into()],
        },
        working_dir: std::env::temp_dir(),
        env: Vec::new(),
        terminal_mode: TerminalMode::Pipe,
        input_policy: InputPolicy::Disabled,
        dependencies: Vec::new(),
        readiness: None,
    }
}

/// One quiet pipe Service that a command can target while the noise runs.
fn quiet_process(name: &str, autostart: bool) -> ProcessSpec {
    ProcessSpec {
        name: name.to_string(),
        kind: ProcessKind::Service,
        enabled: Enabled::Yes,
        autostart: if autostart {
            Autostart::Yes
        } else {
            Autostart::No
        },
        success_exit_codes: vec![0],
        restart: RestartConfig::default(),
        command: CommandForm::Direct {
            program: "/bin/sleep".into(),
            args: vec!["60".into()],
        },
        working_dir: std::env::temp_dir(),
        env: Vec::new(),
        terminal_mode: TerminalMode::Pipe,
        input_policy: InputPolicy::Disabled,
        dependencies: Vec::new(),
        readiness: None,
    }
}

/// Run the multi-Process output pressure fixture.
pub fn run_output_pressure_fixture() -> Result<OutputPressureReport> {
    let processes = vec![
        noisy_process("noisy-0"),
        noisy_process("noisy-1"),
        noisy_process("noisy-2"),
        quiet_process("quiet", true),
        quiet_process("manual", false),
    ];
    let project = EffectiveProject::new(processes)
        .map_err(|error| anyhow::anyhow!("the pressure Project is invalid: {error:?}"))?;

    let (supervisor, _consoles, outputs) = crate::supervisor::start(project)?;
    supervisor.command(Command::StartAutostart);

    // Every autostarted Process must reach its active state before the
    // pressure phase; startup uses bounded polling, not arbitrary sleeps.
    let snapshot = wait_for(&supervisor, PHASE_WAIT, |snapshot| {
        snapshot
            .processes
            .iter()
            .all(|process| !process.autostart || matches!(process.lifecycle, Lifecycle::Running))
    })?;
    assert!(
        snapshot
            .processes
            .iter()
            .all(|process| !process.autostart || process.current_run.is_some()),
        "every autostart Process must own a Run before the pressure phase"
    );

    // Flood until every noisy Process has hit its retention bound; all
    // three must keep draining while the other two flood too.
    wait_for(&supervisor, PHASE_WAIT, |_| {
        [0u32, 1, 2].iter().all(|index| {
            outputs
                .for_process(*index)
                .is_some_and(|module| module.snapshot().truncated)
        })
    })
    .map_err(|_| {
        let observed: Vec<bool> = [0u32, 1, 2]
            .iter()
            .map(|index| {
                outputs
                    .for_process(*index)
                    .is_some_and(|module| module.snapshot().truncated)
            })
            .collect();
        anyhow::anyhow!("not every noisy Process truncated within the bound: {observed:?}")
    })?;

    // A stop for one quiet Process must still complete promptly while the
    // three noisies keep flooding their own modules.
    let issued = Instant::now();
    supervisor.command(Command::Stop("quiet".to_string()));
    let snapshot = wait_for(&supervisor, COMMAND_DEADLINE, |snapshot| {
        snapshot
            .named("quiet")
            .is_some_and(|process| process.current_run.is_none())
    })
    .map_err(|_| anyhow::anyhow!("a stop issued into flooded output did not finish in time"))?;
    let latency = issued.elapsed();
    assert!(
        snapshot
            .named("quiet")
            .is_some_and(|process| process.failure.is_none()),
        "a clean stop must not read as a failure"
    );

    let mut max_retained_bytes = 0usize;
    let mut max_retained_chunks = 0usize;
    let mut truncated_processes = 0usize;
    for index in 0..5u32 {
        let Some(module) = outputs.for_process(index) else {
            continue;
        };
        let snapshot = module.snapshot();
        if snapshot.truncated {
            truncated_processes += 1;
        }
        let bytes = snapshot
            .chunks
            .iter()
            .map(|chunk| match chunk {
                crate::output::RetainedChunk::Marker { label, .. } => label.len(),
                crate::output::RetainedChunk::Data { text, .. } => text.len(),
            })
            .sum();
        max_retained_bytes = max_retained_bytes.max(bytes);
        max_retained_chunks = max_retained_chunks.max(snapshot.chunks.len());
    }

    // Leave no Process behind.
    supervisor.command(Command::StopAll);
    wait_for(&supervisor, PHASE_WAIT, |snapshot| {
        snapshot
            .processes
            .iter()
            .all(|process| process.current_run.is_none())
    })?;
    supervisor.stop_task();

    Ok(OutputPressureReport {
        noisy_processes: 3,
        truncated_processes,
        max_retained_bytes,
        max_retained_chunks,
        command_latency_ms: latency.as_millis(),
    })
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
            poll_interval: Duration::from_millis(50),
            stopped_message: "the Supervisor stopped",
            timeout_message: "the phase did not finish within its bound",
        },
        done,
    )
}
