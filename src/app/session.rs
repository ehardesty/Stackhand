//! One owner for the interactive session's Project shutdown obligation.

use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

use crate::model::EffectiveProject;
use crate::output::OutputViews;
use crate::supervisor::{Command, Consoles, ProjectShutdownSnapshot, SupervisorHandle};

use super::signals::SessionSignal;

const PROJECT_SHUTDOWN_WAIT: Duration = Duration::from_secs(20);
const SHUTDOWN_OBSERVATION_GRACE: Duration = Duration::from_secs(5);
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(25);

pub(super) fn require_interactive_terminal() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("an interactive terminal is required on both stdin and stdout");
    }
    Ok(())
}

pub(super) enum LoopEnd {
    Completed(ProjectShutdownSnapshot),
    Hangup,
}

pub(super) struct ProjectSession {
    supervisor: Option<SupervisorHandle>,
    deadline: Option<Instant>,
}

impl ProjectSession {
    pub(super) fn start(project: EffectiveProject) -> Result<(Self, Consoles, Arc<OutputViews>)> {
        let (supervisor, consoles, outputs) = crate::supervisor::start(project)?;
        Ok((
            Self {
                supervisor: Some(supervisor),
                deadline: None,
            },
            consoles,
            outputs,
        ))
    }

    pub(super) fn supervisor(&self) -> &SupervisorHandle {
        self.supervisor
            .as_ref()
            .expect("unfinished Project session")
    }

    pub(super) fn is_shutting_down(&self) -> bool {
        self.deadline.is_some()
    }

    pub(super) fn begin(&mut self) -> Instant {
        *self.deadline.get_or_insert_with(|| {
            let deadline = Instant::now() + PROJECT_SHUTDOWN_WAIT;
            self.supervisor
                .as_ref()
                .expect("unfinished Project session")
                .command(Command::Shutdown { deadline });
            deadline
        })
    }

    pub(super) fn finish(mut self, observed: Option<ProjectShutdownSnapshot>) -> Result<()> {
        let result = match observed.filter(|snapshot| snapshot.complete) {
            Some(snapshot) => shutdown_result(&snapshot),
            None => self.drain(),
        };
        self.stop_task();
        result
    }

    fn drain(&mut self) -> Result<()> {
        let deadline = self.begin() + SHUTDOWN_OBSERVATION_GRACE;
        loop {
            // snapshot() can wait one second. Reserve that wait inside the grace.
            if Instant::now() + Duration::from_secs(1) >= deadline {
                bail!("Project cleanup not confirmed before the shutdown deadline");
            }
            let snapshot = self.supervisor().snapshot().ok_or_else(|| {
                anyhow!("Project cleanup not confirmed: the Supervisor stopped responding")
            })?;
            if let Some(result) = snapshot.shutdown.filter(|result| result.complete) {
                return shutdown_result(&result);
            }
            std::thread::sleep(
                SNAPSHOT_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }

    fn stop_task(&mut self) {
        if let Some(supervisor) = self.supervisor.take() {
            supervisor.stop_task();
        }
    }
}

impl Drop for ProjectSession {
    fn drop(&mut self) {
        // Panic fallback uses existing Run drop cleanup, not another shutdown wait.
        self.stop_task();
    }
}

pub(super) fn shutdown_result(result: &ProjectShutdownSnapshot) -> Result<()> {
    if !result.complete {
        bail!("Project cleanup not confirmed");
    }
    if result.failures.is_empty() && !result.timed_out {
        return Ok(());
    }
    let details = result
        .failures
        .iter()
        .map(|failure| {
            if failure.remaining_pids.is_empty() {
                format!("{}: {}", failure.process, failure.detail)
            } else {
                format!(
                    "{}: {} (remaining PIDs: {:?})",
                    failure.process, failure.detail, failure.remaining_pids
                )
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    if details.is_empty() {
        bail!("Project shutdown did not finish cleanly: shutdown deadline expired");
    }
    bail!("Project shutdown did not finish cleanly: {details}")
}

pub(super) fn report(
    end: Result<LoopEnd>,
    signal: Option<SessionSignal>,
    cleanup: Result<()>,
) -> Result<()> {
    let _ = writeln!(io::stderr(), "Stackhand is shutting down the Project…");
    if cleanup.is_ok() {
        let _ = writeln!(io::stderr(), "Project shutdown complete.");
    }
    match (end, signal) {
        (Err(error), None) => match cleanup {
            Ok(()) => Err(error),
            Err(cleanup) => Err(error.context(format!("{cleanup:#}"))),
        },
        (Err(error), Some(signal)) => {
            let _ = writeln!(io::stderr(), "Terminal error after {signal:?}: {error:#}");
            cleanup
        }
        _ => cleanup,
    }
}

#[cfg(test)]
mod tests;
