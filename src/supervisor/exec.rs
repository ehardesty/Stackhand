//! The production exec-readiness adapter. It reuses the Run owner for one
//! non-interactive command, so stdin, output drains, Process Tree cleanup,
//! and bounded shutdown keep one owner and one lifecycle protocol.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::model::ReadinessProbe;
use crate::runtime::{
    RunOutputReceiver, RunRuntime, RunStartRequest, RunTransport, ShutdownLadder, SpawnCommand,
    root_exit_pending,
};
use crate::supervisor::seam::ProbeIntent;

/// Maximum total stdout and stderr bytes retained in one failed attempt's
/// diagnostic. The process readers continue draining after this limit.
pub(crate) const EXEC_DIAGNOSTIC_LIMIT_BYTES: usize = 8 * 1_024;

const EXEC_OUTPUT_POLL: Duration = Duration::from_millis(20);
const EXEC_EXIT_POLL: Duration = Duration::from_millis(10);
const EXEC_CLEANUP_BUDGET: Duration = Duration::from_secs(2);
const EXEC_CLEANUP_LADDER: ShutdownLadder = ShutdownLadder {
    graceful_timeout: Duration::from_millis(100),
    terminate_timeout: Duration::from_millis(100),
    final_deadline: Duration::from_secs(2),
};

#[derive(Default)]
struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Run one exec check through the existing Run owner. The caller's
/// cancellation flag is checked before reporting, while this function also
/// performs physical Process Tree cleanup for timeout and cancellation.
pub(crate) fn attempt(intent: &ProbeIntent, canceled: &AtomicBool) -> Result<(), String> {
    if canceled.load(Ordering::Acquire) {
        return Err("exec readiness attempt was canceled".to_string());
    }

    let (command, success_exit_codes) = exec_command(intent)?;
    let (event_tx, _event_rx) = mpsc::channel();
    let (output_tx, output_rx) = crate::runtime::output_channel();
    let request = RunStartRequest {
        process_id: intent.process_id,
        run_id: intent.run_id,
        command,
        transport: RunTransport::Pipe { output: output_tx },
        events: event_tx,
        ladder: EXEC_CLEANUP_LADDER,
        metrics_interval: None,
        output_observer: None,
    };
    let mut run = RunRuntime
        .start(request)
        .map_err(|error| format!("exec spawn failed: {error}"))?;
    let root_pid = run
        .root_pid()
        .ok_or_else(|| "exec spawn did not provide a Process Tree identity".to_string());
    let (stop_output, output_worker) = start_output_collector(output_rx);

    let completion = match root_pid {
        Ok(root_pid) => wait_for_completion(root_pid, intent.timeout, canceled),
        Err(detail) => Completion::Unavailable(detail),
    };
    let outcome = match completion {
        Completion::Natural => run
            .wait_with_timeout(EXEC_CLEANUP_BUDGET)
            .map_err(|error| format!("exec cleanup failed: {error}")),
        Completion::TimedOut | Completion::Canceled | Completion::Unavailable(_) => run
            .shutdown_with_timeout(EXEC_CLEANUP_BUDGET)
            .map_err(|error| format!("exec cleanup failed: {error}")),
    };
    drop(run);

    stop_output.store(true, Ordering::Release);
    let captured = output_worker
        .join()
        .map_err(|_| "exec output collector failed".to_string())?;

    match completion {
        Completion::Canceled => Err("exec readiness attempt was canceled".to_string()),
        Completion::TimedOut => {
            let mut detail = format!("timed out after {} ms", intent.timeout.as_millis());
            append_cleanup_failure(&mut detail, &outcome);
            append_output(&mut detail, &captured);
            Err(detail)
        }
        Completion::Unavailable(detail) => {
            let mut message = detail;
            append_cleanup_failure(&mut message, &outcome);
            append_output(&mut message, &captured);
            Err(message)
        }
        Completion::Natural => match outcome {
            Err(error) => {
                let mut detail = error;
                append_output(&mut detail, &captured);
                Err(detail)
            }
            Ok(outcome) if !outcome.cleanup_confirmed => {
                let mut detail = format!(
                    "exec cleanup did not confirm; remaining PIDs: {:?}",
                    outcome.remaining_pids
                );
                append_output(&mut detail, &captured);
                Err(detail)
            }
            Ok(outcome) if success_exit_codes.contains(&outcome.exit_code.unwrap_or(-1)) => Ok(()),
            Ok(outcome) => {
                let mut detail = match outcome.exit_code {
                    Some(code) => format!("command exited with code {code}"),
                    None => "command ended without an exit code".to_string(),
                };
                append_output(&mut detail, &captured);
                Err(detail)
            }
        },
    }
}

fn exec_command(intent: &ProbeIntent) -> Result<(SpawnCommand, Vec<i32>), String> {
    let ReadinessProbe::Exec {
        command,
        working_dir,
        env,
        success_exit_codes,
    } = &intent.probe
    else {
        return Err("non-exec probe was sent to the exec adapter".to_string());
    };
    let context = intent
        .exec_context
        .as_ref()
        .ok_or_else(|| "exec probe is missing its Process context".to_string())?;
    let (program, args) = command.resolve(&context.shell);
    let mut spawn = SpawnCommand::new(program);
    for arg in args {
        spawn = spawn.arg(arg);
    }
    spawn = spawn.with_current_dir(working_dir.as_ref().unwrap_or(&context.working_dir).clone());
    for key in &context.env_remove {
        spawn = spawn.without_env(key.clone());
    }
    for (key, value) in context.env.iter().chain(env) {
        spawn = spawn.with_env(key.clone(), value.clone());
    }
    Ok((spawn, success_exit_codes.clone()))
}

enum Completion {
    Natural,
    TimedOut,
    Canceled,
    Unavailable(String),
}

fn wait_for_completion(
    root_pid: crate::runtime::OsPid,
    timeout: Duration,
    canceled: &AtomicBool,
) -> Completion {
    let deadline = Instant::now() + timeout;
    loop {
        if canceled.load(Ordering::Acquire) {
            return Completion::Canceled;
        }
        if root_exit_pending(root_pid) {
            return Completion::Natural;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Completion::TimedOut;
        }
        std::thread::sleep(EXEC_EXIT_POLL.min(remaining));
    }
}

fn start_output_collector(
    receiver: RunOutputReceiver,
) -> (std::sync::Arc<AtomicBool>, JoinHandle<CapturedOutput>) {
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let stop_for_thread = std::sync::Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("exec-output".to_string())
        .spawn(move || {
            let mut captured = CapturedOutput::default();
            loop {
                if stop_for_thread.load(Ordering::Acquire) {
                    drain_pending(&receiver, &mut captured);
                    return captured;
                }
                match receiver.recv_timeout(EXEC_OUTPUT_POLL) {
                    Ok(chunk) => retain(&mut captured, &chunk.data),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return captured,
                }
            }
        })
        .expect("exec output collector thread spawns");
    (stop, handle)
}

fn drain_pending(receiver: &RunOutputReceiver, captured: &mut CapturedOutput) {
    loop {
        match receiver.try_recv() {
            Ok(chunk) => retain(captured, &chunk.data),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
        }
    }
}

fn retain(captured: &mut CapturedOutput, bytes: &[u8]) {
    let available = EXEC_DIAGNOSTIC_LIMIT_BYTES.saturating_sub(captured.bytes.len());
    let amount = available.min(bytes.len());
    captured.bytes.extend_from_slice(&bytes[..amount]);
    captured.truncated |= amount < bytes.len();
}

fn append_cleanup_failure(
    detail: &mut String,
    outcome: &Result<crate::runtime::RunOutcome, String>,
) {
    match outcome {
        Ok(outcome) if !outcome.cleanup_confirmed => detail.push_str(&format!(
            "; cleanup did not confirm; remaining PIDs: {:?}",
            outcome.remaining_pids
        )),
        Err(error) => detail.push_str(&format!("; {error}")),
        _ => {}
    }
}

fn append_output(detail: &mut String, output: &CapturedOutput) {
    if !output.bytes.is_empty() {
        detail.push_str("; output: ");
        detail.push_str(&String::from_utf8_lossy(&output.bytes));
    }
    if output.truncated {
        detail.push_str(&format!(
            "; output truncated at {EXEC_DIAGNOSTIC_LIMIT_BYTES} bytes"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CommandForm;
    use crate::runtime::{ProcessId, RunId};
    use crate::supervisor::seam::{AttemptId, ExecContext, ProbeScope, WorkId};

    fn intent(command: CommandForm, timeout: Duration) -> ProbeIntent {
        ProbeIntent {
            process_id: ProcessId::new(1),
            run_id: RunId::new(1),
            work_id: WorkId::new(1),
            attempt_id: AttemptId::new(1),
            probe: ReadinessProbe::Exec {
                command,
                working_dir: None,
                env: Vec::new(),
                success_exit_codes: vec![0],
            },
            timeout,
            exec_context: Some(ExecContext {
                working_dir: std::env::temp_dir(),
                env: Vec::new(),
                env_remove: Vec::new(),
                shell: crate::model::ShellConfig::default(),
            }),
            scope: ProbeScope::Readiness,
        }
    }

    #[test]
    fn direct_exec_runs_without_shell_parsing() {
        let result = attempt(
            &intent(
                CommandForm::Direct {
                    program: "/usr/bin/printf".into(),
                    args: vec!["direct-ok".into()],
                },
                Duration::from_secs(2),
            ),
            &AtomicBool::new(false),
        );
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn failed_exec_retains_stdout_and_stderr() {
        let result = attempt(
            &intent(
                CommandForm::Shell {
                    text: "printf out; printf err >&2; exit 7".to_string(),
                },
                Duration::from_secs(2),
            ),
            &AtomicBool::new(false),
        )
        .expect_err("nonzero exit fails");
        assert!(result.contains("code 7"), "{result}");
        assert!(result.contains("out"), "{result}");
        assert!(result.contains("err"), "{result}");
    }

    #[test]
    fn noisy_exec_reports_the_central_output_limit() {
        let result = attempt(
            &intent(
                CommandForm::Shell {
                    text: "i=0; while [ $i -lt 20000 ]; do printf x; i=$((i+1)); done; exit 1"
                        .to_string(),
                },
                Duration::from_secs(2),
            ),
            &AtomicBool::new(false),
        )
        .expect_err("nonzero exit fails");
        assert!(result.contains("output truncated"), "{result}");
        assert!(
            result.contains(&EXEC_DIAGNOSTIC_LIMIT_BYTES.to_string()),
            "{result}"
        );
    }

    #[test]
    fn timeout_cleans_up_a_sleeping_exec() {
        let started = Instant::now();
        let result = attempt(
            &intent(
                CommandForm::Shell {
                    text: "sleep 30".to_string(),
                },
                Duration::from_millis(50),
            ),
            &AtomicBool::new(false),
        )
        .expect_err("sleep must time out");
        assert!(result.contains("timed out"), "{result}");
        assert!(started.elapsed() < Duration::from_secs(3), "{result}");
    }
}
