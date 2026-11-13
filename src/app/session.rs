use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

use crate::model::EffectiveProject;
use crate::output::OutputViews;
use crate::supervisor::{Command, Consoles, ProjectShutdownSnapshot, SupervisorHandle};

use crate::host::SessionSignal;

const PROJECT_SHUTDOWN_WAIT: Duration = Duration::from_secs(20);
const SHUTDOWN_OBSERVATION_GRACE: Duration = Duration::from_secs(5);
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(25);

pub(super) fn require_interactive_terminal() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("an interactive terminal is required on both stdin and stdout");
    }
    Ok(())
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

    pub(super) fn shutdown_wait_expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline + SHUTDOWN_OBSERVATION_GRACE)
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
            let snapshot = self.supervisor().snapshot().ok_or_else(|| {
                anyhow!("Project cleanup not confirmed: the Supervisor stopped responding")
            })?;
            if let Some(result) = snapshot.shutdown.filter(|result| result.complete) {
                return shutdown_result(&result);
            }
            if Instant::now() + crate::supervisor::SNAPSHOT_WAIT >= deadline {
                bail!("Project cleanup not confirmed before the shutdown deadline");
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

pub(super) struct SessionReport {
    pub(super) text: String,
    pub(super) result: Result<()>,
    reported_errors: Vec<String>,
}

impl SessionReport {
    pub(super) fn fail(&mut self, error: anyhow::Error) {
        let message = format!("{error:#}");
        if !self.reported_errors.contains(&message) {
            self.text.push_str(&format!("Error: {message}\n"));
            self.reported_errors.push(message);
        }
        if self.result.is_ok() {
            self.result = Err(error);
        }
    }
}

pub(super) fn report(
    end: Result<()>,
    signal: Option<SessionSignal>,
    cleanup: Result<()>,
) -> SessionReport {
    let mut text = String::from("Stackhand is shutting down the Project…\n");
    let mut reported_errors = Vec::new();
    if cleanup.is_ok() {
        text.push_str("Project shutdown complete.\n");
    }
    let result = match (end, signal) {
        (Err(error), None) => match cleanup {
            Ok(()) => Err(error),
            Err(cleanup) => Err(error.context(format!("{cleanup:#}"))),
        },
        (Err(error), Some(signal)) => {
            let message = format!("{error:#}");
            text.push_str(&format!("Error after {signal:?} request: {message}\n"));
            reported_errors.push(message);
            cleanup
        }
        _ => cleanup,
    };
    if let Err(error) = &result {
        let message = format!("{error:#}");
        if !reported_errors.contains(&message) {
            text.push_str(&format!("Error: {message}\n"));
            reported_errors.push(message);
        }
    }
    SessionReport {
        text,
        result,
        reported_errors,
    }
}

#[cfg(test)]
mod tests;
