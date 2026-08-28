//! Real Run transport tests for live log observation.

use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::geometry::TerminalGeometry;

const MATCH_KEY: u64 = 7;
const MATCH_TEXT: &str = "early-log-marker";

fn start_observed_run(
    mode: RunMode,
    command: SpawnCommand,
) -> (OwnedRun, Receiver<u64>, RunOutputReceiver) {
    let (matched_tx, matched_rx) = mpsc::channel();
    let observer = LiveLogMatcher::new(
        vec![LogPattern {
            key: MATCH_KEY,
            contains: MATCH_TEXT.to_string(),
        }],
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        move |key| {
            let _ = matched_tx.send(key);
        },
    )
    .expect("valid live matcher");
    let output_observer: Arc<dyn RunOutputObserver> = observer;
    let (events, _event_receiver) = mpsc::channel();
    let (output, output_receiver) = output_channel();
    let run = RunRuntime
        .start(RunStartRequest {
            process_id: ProcessId::new(1),
            run_id: RunId::new(1),
            command,
            mode,
            events,
            output,
            ladder: ShutdownLadder::default(),
            metrics_interval: None,
            on_output_wake: None,
            output_observer: Some(output_observer),
        })
        .expect("observed Run started");
    (run, matched_rx, output_receiver)
}

fn assert_early_output_is_observed(mode: RunMode) {
    let command = SpawnCommand::new("/bin/sh")
        .arg("-c")
        .arg("printf 'early-log-marker'; sleep 30");
    let (mut run, matched, _output) = start_observed_run(mode, command);

    assert_eq!(
        matched
            .recv_timeout(Duration::from_secs(5))
            .expect("the first output chunk reaches the matcher"),
        MATCH_KEY
    );
    assert!(
        matched.recv_timeout(Duration::from_millis(100)).is_err(),
        "one literal match must emit one fact"
    );
    run.shutdown().expect("observed Run cleaned up");
}

#[test]
fn pipe_observer_sees_output_from_the_start_of_the_run() {
    assert_early_output_is_observed(RunMode::Pipe);
}

#[test]
fn pty_observer_sees_output_from_the_start_of_the_run() {
    assert_early_output_is_observed(RunMode::Pty {
        initial_geometry: TerminalGeometry::DEFAULT,
    });
}

#[test]
fn observer_can_match_after_output_that_exceeds_retained_history() {
    let matched = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::clone(&matched);
    let observer = LiveLogMatcher::new(
        vec![LogPattern {
            key: MATCH_KEY,
            contains: MATCH_TEXT.to_string(),
        }],
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        move |key| received.lock().unwrap().push(key),
    )
    .expect("valid live matcher");

    // Feed more than the Process output retention limit without retaining it
    // in the matcher. The following marker still matches immediately.
    observer.observe(&vec![b'x'; crate::output::RETAINED_BYTES * 2]);
    observer.observe(MATCH_TEXT.as_bytes());

    assert_eq!(*matched.lock().unwrap(), vec![MATCH_KEY]);
}
