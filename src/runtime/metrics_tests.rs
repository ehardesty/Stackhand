//! Aggregate Process Tree metrics tests through the public Run seam.

use super::*;
use crate::runtime::pipe::RunOutput;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(5);

struct StartedRun {
    run: OwnedRun,
    events: Receiver<RunEvent>,
}

fn start_sampled(command: SpawnCommand) -> StartedRun {
    let (events, event_receiver) = mpsc::channel();
    let (_output, _output_log): (std::sync::mpsc::Sender<RunOutput>, _) = mpsc::channel();
    let run = RunRuntime
        .start(RunStartRequest {
            process_id: ProcessId::new(41),
            run_id: RunId::new(202),
            command,
            mode: RunMode::Pipe,
            events,
            output: mpsc::channel().0,
            ladder: quick_ladder(100, 100),
            metrics_interval: Some(Duration::from_millis(20)),
            on_output_wake: None,
        })
        .expect("sampled run started");
    StartedRun {
        run,
        events: event_receiver,
    }
}

fn quick_ladder(graceful_ms: u64, terminate_ms: u64) -> ShutdownLadder {
    ShutdownLadder {
        graceful_timeout: Duration::from_millis(graceful_ms),
        terminate_timeout: Duration::from_millis(terminate_ms),
        final_deadline: Duration::from_secs(5),
    }
}

/// Collect metrics events until `count` arrive or the deadline expires.
fn collect_metrics(events: &Receiver<RunEvent>, count: usize) -> Vec<RunMetrics> {
    let mut samples = Vec::new();
    let deadline = Instant::now() + WAIT;
    while samples.len() < count && Instant::now() < deadline {
        if let Ok(event) = events.recv_timeout(Duration::from_millis(200))
            && let RunEventKind::Metrics(snapshot) = event.kind
        {
            samples.push(snapshot);
        }
    }
    samples
}

#[test]
fn sampler_emits_runid_scoped_snapshots_with_child_contribution() {
    // The fixture keeps two busy children alive so aggregate CPU and RSS
    // must include measurable child-process use.
    let started = start_sampled(
        SpawnCommand::new("/bin/sh")
            .arg("-c")
            .arg("yes > /dev/null & yes > /dev/null & wait"),
    );

    let samples = collect_metrics(&started.events, 3);
    assert!(samples.len() >= 2, "expected at least 2 snapshots");
    for sample in &samples {
        assert_eq!(sample.process_id, ProcessId::new(41));
        assert_eq!(sample.run_id, RunId::new(202));
        assert!(sample.rss_kib > 0, "aggregate resident memory was zero");
        assert!(sample.members_observed >= 1);
    }
    // A busy child guarantees measurable CPU on at least one snapshot.
    assert!(
        samples.iter().any(|sample| sample.cpu_percent > 0.0),
        "busy children produced no measurable CPU"
    );
}

#[test]
fn sampling_tolerates_process_exit_during_enumeration() {
    let mut started = start_sampled(SpawnCommand::new("/bin/sh").arg("-c").arg("exit 0"));
    thread::sleep(Duration::from_millis(60));

    let outcome = started.run.shutdown().expect("race-tolerant cleanup");
    assert!(outcome.intentional_stop);
    // The tree vanished; cleanup still confirms without false failure.
    assert!(outcome.cleanup_confirmed, "outcome: {outcome:?}");
}

#[test]
fn stopped_sampler_cannot_emit_and_final_sample_is_retained() {
    let mut started = start_sampled(
        SpawnCommand::new("/bin/sh")
            .arg("-c")
            .arg("echo ready; while :; do sleep 3600; done"),
    );

    // Wait until at least one sample arrived so we know the sampler ran.
    let before = collect_metrics(&started.events, 1);
    assert!(!before.is_empty(), "no sample arrived before shutdown");
    let highest_sequence_before_stop = before.last().expect("one sample").sequence;

    let outcome = started.run.shutdown().expect("cleanup completed");
    assert!(
        outcome.final_metrics.is_some(),
        "the last valid sample must be retained in the outcome"
    );

    // After shutdown returns, the sampler is joined: no new sequence numbers
    // may appear in the event stream.
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        match started.events.recv_timeout(Duration::from_millis(50)) {
            Ok(event) => {
                if let RunEventKind::Metrics(snapshot) = event.kind {
                    assert!(
                        snapshot.sequence <= highest_sequence_before_stop,
                        "a stopped sampler emitted a later sample ({})",
                        snapshot.sequence
                    );
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
}
