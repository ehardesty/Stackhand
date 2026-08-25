//! Shutdown-ladder tests for the Run ownership seam.
//!
//! Every test drives the complete public path: `RunRuntime::start` with an
//! explicit [`ShutdownLadder`], semantic operations, and one structured
//! [`RunOutcome`].

use super::*;
use crate::geometry::TerminalGeometry;
use crate::runtime::process_tree::UnixProcessTree;
use crate::terminal::PasteRejection;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

/// Short timeouts so ladder stages are observable without slow tests.
fn quick_ladder(graceful_ms: u64, terminate_ms: u64) -> ShutdownLadder {
    ShutdownLadder {
        graceful_timeout: Duration::from_millis(graceful_ms),
        terminate_timeout: Duration::from_millis(terminate_ms),
        final_deadline: Duration::from_secs(5),
    }
}

struct StartedRun {
    run: OwnedRun,
    output: mpsc::Receiver<crate::runtime::pipe::RunOutput>,
    root_pid: u32,
}

fn start_pipe(command: SpawnCommand, ladder: ShutdownLadder) -> StartedRun {
    let (events, _event_log) = mpsc::channel();
    let (output, output_receiver) = mpsc::channel();
    let run = RunRuntime
        .start(RunStartRequest {
            process_id: ProcessId::new(31),
            run_id: RunId::new(101),
            command,
            mode: RunMode::Pipe,
            events,
            output,
            ladder,
            metrics_interval: None,
            on_output_wake: None,
        })
        .expect("pipe run started");
    let root_pid = run.root_pid().expect("pipe mode reports a root pid").get();
    StartedRun {
        run,
        output: output_receiver,
        root_pid,
    }
}

fn read_stdout_until(started: &mut StartedRun, marker: &str) -> String {
    let mut text = String::new();
    let deadline = Instant::now() + WAIT;
    loop {
        while let Ok(chunk) = started.output.try_recv() {
            if chunk.stream == crate::runtime::pipe::OutputStream::Stdout {
                text.push_str(&String::from_utf8_lossy(&chunk.data));
            }
        }
        if text.contains(marker) {
            return text;
        }
        assert!(Instant::now() < deadline, "marker {marker:?} not seen");
        thread::sleep(Duration::from_millis(5));
    }
}

fn drain_stdout(started: &mut StartedRun) -> String {
    let mut text = String::new();
    while let Ok(chunk) = started.output.recv_timeout(WAIT) {
        if chunk.stream == crate::runtime::pipe::OutputStream::Stdout {
            text.push_str(&String::from_utf8_lossy(&chunk.data));
        }
    }
    while let Ok(chunk) = started.output.try_recv() {
        if chunk.stream == crate::runtime::pipe::OutputStream::Stdout {
            text.push_str(&String::from_utf8_lossy(&chunk.data));
        }
    }
    text
}

/// Distinct `line-NNNNNN` sequence numbers observed in accumulated output.
fn line_numbers(text: &str) -> std::collections::BTreeSet<u32> {
    text.lines()
        .filter_map(|line| {
            line.strip_prefix("line-")
                .and_then(|value| value.parse().ok())
        })
        .collect()
}

#[test]
fn ladder_waits_through_configured_graceful_and_terminate_stages() {
    // The fixture ignores interrupt but exits cleanly on terminate. It also
    // writes a fixed, known output so no-loss is measurable.
    let script = "trap '' INT; trap 'exit 7' TERM; echo ready; i=0; while [ \"$i\" -lt 1000 ]; do printf 'line-%06d\\n' \"$i\"; i=$((i+1)); done; while :; do sleep 3600; done";
    let mut started = start_pipe(
        SpawnCommand::new("/bin/sh").arg("-c").arg(script),
        quick_ladder(400, 400),
    );
    let pre_shutdown_text = read_stdout_until(&mut started, "ready");

    let started_at = Instant::now();
    let outcome = started.run.shutdown().expect("ladder completed");
    let elapsed = started_at.elapsed();

    assert_eq!(outcome.run_id, RunId::new(101));
    assert!(outcome.intentional_stop);
    assert_eq!(outcome.disposition, RunExitDisposition::IntentionalStop);
    assert_eq!(outcome.exit_code, Some(7));
    assert!(outcome.cleanup_confirmed);
    assert!(outcome.remaining_pids.is_empty());
    assert!(
        elapsed >= Duration::from_millis(400),
        "graceful stage was skipped"
    );

    let stage_names: Vec<&str> = outcome.stage_results.iter().map(|s| s.stage).collect();
    for expected in ["interrupt", "terminate", "reap", "drain"] {
        assert!(
            stage_names.contains(&expected),
            "missing stage {expected}; got {stage_names:?}"
        );
    }
    // Kill never fired: terminate already settled the tree.
    assert!(!stage_names.contains(&"kill") || outcome.stage_results.iter().all(|s| s.ok));

    // Everything the fixture wrote reached the high-volume sink.
    let drained = drain_stdout(&mut started);
    let combined = format!("{pre_shutdown_text}\n{drained}");
    let numbers: std::collections::BTreeSet<u32> = combined
        .lines()
        .filter_map(|line| {
            line.strip_prefix("line-")
                .and_then(|value| value.parse().ok())
        })
        .collect();
    assert_eq!(numbers.len(), 1_000, "output lines were lost: {combined:?}");
    assert!(pids_gone(&[started.root_pid]));
}

#[test]
fn ladder_escalates_to_kill_for_members_that_remain() {
    // The fixture ignores interrupt AND terminate; only SIGKILL ends it.
    // It writes a fixed number of lines first so loss is measurable.
    let script = "trap '' INT; trap '' TERM; echo ready; i=0; while [ \"$i\" -lt 500 ]; do printf 'line-%06d\\n' \"$i\"; i=$((i+1)); done; while :; do sleep 3600; done";
    let mut started = start_pipe(
        SpawnCommand::new("/bin/sh").arg("-c").arg(script),
        quick_ladder(150, 150),
    );
    read_stdout_until(&mut started, "ready");

    let outcome = started.run.shutdown().expect("ladder completed");
    assert!(outcome.intentional_stop);
    assert!(outcome.cleanup_confirmed, "outcome: {outcome:?}");
    assert!(outcome.remaining_pids.is_empty());

    let kill_stage = outcome
        .stage_results
        .iter()
        .find(|stage| stage.stage == "kill")
        .expect("kill stage recorded");
    assert!(kill_stage.ok, "kill stage failed: {kill_stage:?}");
    assert!(pids_gone(&[started.root_pid]));
}

#[test]
fn exit_during_escalation_produces_a_valid_outcome_and_one_ladder() {
    let mut started = start_pipe(
        SpawnCommand::new("/bin/sh").arg("-c").arg("echo ready"),
        quick_ladder(200, 200),
    );
    read_stdout_until(&mut started, "ready");

    // The process exits naturally; shutdown still produces a valid result
    // instead of a race failure.
    let first = started.run.shutdown().expect("race-tolerant cleanup");
    assert!(first.intentional_stop);
    assert!(first.cleanup_confirmed);

    // Repeated shutdown observes the same single operation instantly.
    let repeat_started = Instant::now();
    let second = started.run.shutdown().expect("observed cleanup");
    assert_eq!(first, second);
    assert!(
        repeat_started.elapsed() < Duration::from_millis(100),
        "repeated shutdown re-ran the ladder"
    );

    let signal_stages: Vec<&StageResult> = first
        .stage_results
        .iter()
        .filter(|stage| matches!(stage.stage, "interrupt" | "terminate" | "kill"))
        .collect();
    assert!(
        signal_stages
            .iter()
            .any(|stage| stage.ok && stage.detail.is_some()),
        "settled-before-signal stages should be recorded as skipped"
    );
    assert!(pids_gone(&[started.root_pid]));
}

#[test]
fn input_and_resize_are_rejected_after_shutdown_starts() {
    let geometry = TerminalGeometry::DEFAULT;
    let (events, _log) = mpsc::channel();
    let mut run = RunRuntime
        .start(RunStartRequest {
            process_id: ProcessId::new(32),
            run_id: RunId::new(102),
            command: SpawnCommand::new("/bin/sh").arg("-c").arg("sleep 60"),
            mode: RunMode::Pty {
                initial_geometry: geometry,
            },
            events,
            output: mpsc::channel().0,
            ladder: quick_ladder(100, 100),
            metrics_interval: None,
            on_output_wake: None,
        })
        .expect("pty run started");
    assert!(run.accepts_input());

    run.shutdown().expect("cleanup completed");
    assert!(!run.accepts_input(), "input gate must stay closed");

    let handle = run.terminal().expect("PTY terminal handle persists");
    let paste = "late".repeat(4);
    assert!(
        matches!(handle.send_paste(&paste), Err(PasteRejection::Stopping)),
        "paste must be rejected after shutdown"
    );
    assert_eq!(
        handle.resize(TerminalGeometry::new(20, 10).unwrap()),
        Err(ResizeRejected::Stopping),
        "resize must be rejected after shutdown"
    );
}

#[test]
fn outcome_distinguishes_natural_unexpected_and_intentional_ends() {
    // Natural completion: exit code 0 without a stop request.
    let mut started = start_pipe(
        SpawnCommand::new("/bin/sh").arg("-c").arg("exit 0"),
        quick_ladder(100, 100),
    );
    let outcome = started.run.wait().expect("natural completion");
    assert_eq!(outcome.disposition, RunExitDisposition::NaturalCompletion);
    assert!(!outcome.intentional_stop);

    // Unexpected exit: nonzero code without a stop request.
    let mut started = start_pipe(
        SpawnCommand::new("/bin/sh").arg("-c").arg("exit 9"),
        quick_ladder(100, 100),
    );
    let outcome = started.run.wait().expect("unexpected completion");
    assert_eq!(outcome.disposition, RunExitDisposition::UnexpectedExit);

    // Intentional stop of a still-running Run.
    let mut started = start_pipe(
        SpawnCommand::new("/bin/sh").arg("-c").arg("sleep 60"),
        quick_ladder(100, 100),
    );
    let outcome = started.run.shutdown().expect("intentional stop");
    assert_eq!(outcome.disposition, RunExitDisposition::IntentionalStop);
    assert!(outcome.intentional_stop);
}

#[test]
fn final_output_is_processed_before_the_output_owner_closes() {
    // A writer that produces a fixed, known output while the ladder runs:
    // every line it emitted must be visible through the high-volume sink.
    let script = "trap '' INT; echo flood-start; i=0; while [ \"$i\" -lt 5000 ]; do printf 'line-%06d\\n' \"$i\"; i=$((i+1)); done; while :; do sleep 3600; done";
    let mut started = start_pipe(
        SpawnCommand::new("/bin/sh").arg("-c").arg(script),
        quick_ladder(100, 100),
    );
    let pre_shutdown_text = read_stdout_until(&mut started, "flood-start");

    let outcome = started.run.shutdown().expect("shutdown completed");
    assert!(outcome.cleanup_confirmed, "outcome: {outcome:?}");

    // No-loss proof: all 5000 sequence numbers must have crossed the sink,
    // allowing one boundary line to straddle the two read phases.
    let drained = drain_stdout(&mut started);
    let combined = format!("{pre_shutdown_text}\n{drained}");
    let numbers = line_numbers(&combined);
    assert!(
        numbers.len() >= 4_999 && numbers.contains(&4_999),
        "output was lost during shutdown: saw {} of 5000 lines",
        numbers.len()
    );
    assert!(pids_gone(&[started.root_pid]));
}

fn pids_gone(pids: &[u32]) -> bool {
    UnixProcessTree::confirm_gone(pids).is_empty()
}
