//! Shutdown behavior under output pressure and across repeated Runs,
//! proven through the public Run seam with real fixture processes.

use super::*;
use crate::geometry::TerminalGeometry;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(5);

fn quick_ladder(graceful_ms: u64, terminate_ms: u64) -> ShutdownLadder {
    ShutdownLadder {
        graceful_timeout: Duration::from_millis(graceful_ms),
        terminate_timeout: Duration::from_millis(terminate_ms),
        final_deadline: Duration::from_secs(5),
    }
}

struct PipeRunFixture {
    run: OwnedRun,
    events: Receiver<RunEvent>,
    output: Receiver<crate::runtime::pipe::RunOutput>,
    #[allow(dead_code)] // retained for containment evidence in failure output
    root_pid: u32,
}

fn start_pipe(command: SpawnCommand, ladder: ShutdownLadder) -> PipeRunFixture {
    let (events, event_receiver) = mpsc::channel();
    let (output, output_receiver) = mpsc::channel();
    let run = RunRuntime
        .start(RunStartRequest {
            process_id: ProcessId::new(51),
            run_id: RunId::new(301),
            command,
            mode: RunMode::Pipe,
            events,
            output,
            ladder,
            metrics_interval: None,
            on_output_wake: None,
        })
        .expect("pipe run started");
    let root_pid = run.root_pid().expect("root pid").get();
    PipeRunFixture {
        run,
        events: event_receiver,
        output: output_receiver,
        root_pid,
    }
}

fn drain_stdout(fixture: &mut PipeRunFixture) -> String {
    let mut text = String::new();
    while let Ok(chunk) = fixture.output.recv_timeout(WAIT) {
        if chunk.stream == crate::runtime::pipe::OutputStream::Stdout {
            text.push_str(&String::from_utf8_lossy(&chunk.data));
        }
    }
    while let Ok(chunk) = fixture.output.try_recv() {
        if chunk.stream == crate::runtime::pipe::OutputStream::Stdout {
            text.push_str(&String::from_utf8_lossy(&chunk.data));
        }
    }
    text
}

#[test]
fn high_output_pipe_keeps_draining_through_its_complete_ladder() {
    // Ignores interrupt so the full interrupt→terminate ladder runs while
    // the writer keeps producing. Output is fixed-size so no-loss is exact.
    let script = "trap '' INT; echo flood-start; i=0; while [ \"$i\" -lt 4000 ]; do printf 'line-%06d\\n' \"$i\"; i=$((i+1)); done; while :; do sleep 3600; done";
    let mut fixture = start_pipe(
        SpawnCommand::new("/bin/sh").arg("-c").arg(script),
        quick_ladder(200, 200),
    );

    let deadline = Instant::now() + WAIT;
    let mut seen_before_shutdown = String::new();
    loop {
        while let Ok(chunk) = fixture.output.try_recv() {
            if chunk.stream == crate::runtime::pipe::OutputStream::Stdout {
                seen_before_shutdown.push_str(&String::from_utf8_lossy(&chunk.data));
            }
        }
        if seen_before_shutdown.contains("flood-start") || Instant::now() > deadline {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }

    let outcome = fixture.run.shutdown().expect("ladder completed");
    assert!(outcome.cleanup_confirmed, "outcome: {outcome:?}");

    let rest = drain_stdout(&mut fixture);
    let combined = format!("{seen_before_shutdown}\n{rest}");
    let numbers: std::collections::BTreeSet<u32> = combined
        .lines()
        .filter_map(|line| line.strip_prefix("line-").and_then(|v| v.parse().ok()))
        .collect();
    // One line may straddle a chunk boundary between the two phases.
    assert!(
        numbers.len() >= 3_999 && numbers.contains(&3_999),
        "output was lost during shutdown: saw {} of 4000 lines",
        numbers.len()
    );
    // Every reader, writer, and owner task joined or reported failure.
    assert!(outcome.task_join_failures.is_empty());
}

#[test]
fn noisy_run_does_not_delay_another_runs_interrupt_and_shutdown() {
    // Run A floods without ever stopping on its own.
    let script = "echo go; i=0; while :; do printf 'noise-%06d\\n' \"$i\"; i=$((i+1)); done";
    let mut noisy = start_pipe(
        SpawnCommand::new("/bin/sh").arg("-c").arg(script),
        quick_ladder(100, 100),
    );
    let deadline = Instant::now() + WAIT;
    let mut ready = false;
    while Instant::now() < deadline {
        if let Ok(chunk) = noisy.output.try_recv()
            && chunk.stream == crate::runtime::pipe::OutputStream::Stdout
            && String::from_utf8_lossy(&chunk.data).contains("go")
        {
            ready = true;
            break;
        }
    }
    assert!(ready, "noisy fixture never produced output");

    // Run B must receive its interrupt and complete shutdown promptly even
    // while A saturates its pipes.
    let quiet_command = SpawnCommand::new("/bin/sh").arg("-c").arg("sleep 300");
    let (events_b, _log_b) = mpsc::channel();
    let (output_b, _out_log_b) = mpsc::channel();
    let mut quiet = RunRuntime
        .start(RunStartRequest {
            process_id: ProcessId::new(52),
            run_id: RunId::new(302),
            command: quiet_command,
            mode: RunMode::Pipe,
            events: events_b,
            output: output_b,
            ladder: quick_ladder(100, 100),
            metrics_interval: None,
            on_output_wake: None,
        })
        .expect("quiet run started");

    let started_at = Instant::now();
    let outcome = quiet.shutdown().expect("quiet run shut down");
    let elapsed = started_at.elapsed();

    assert!(outcome.intentional_stop);
    assert!(outcome.cleanup_confirmed);
    // The whole ladder budget is ~200 ms plus polling slack; a delayed
    // interrupt would blow far past this bound.
    assert!(
        elapsed < Duration::from_secs(2),
        "quiet run's shutdown took {elapsed:?} under output pressure"
    );
    match outcome
        .stage_results
        .iter()
        .find(|stage| stage.stage == "interrupt")
    {
        Some(stage) => assert!(stage.ok, "quiet run's interrupt failed"),
        None => panic!("interrupt stage missing"),
    }

    noisy.run.shutdown().expect("noisy run cleaned up");
    let _ = noisy.events.try_recv();
}

#[test]
fn repeated_cycles_do_not_leak_threads_or_file_descriptors() {
    fn thread_count() -> usize {
        #[cfg(target_os = "macos")]
        {
            // One line per thread after the header row.
            let output = std::process::Command::new("/bin/ps")
                .args(["-M", "-p", &std::process::id().to_string()])
                .output()
                .expect("ps -M works on this host");
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .skip(1)
                .count()
        }
        #[cfg(target_os = "linux")]
        {
            let status = std::fs::read_to_string("/proc/self/status").unwrap();
            status
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Threads:")
                        .and_then(|v| v.trim().parse().ok())
                })
                .expect("Linux reports thread count in /proc/self/status")
        }
    }

    fn open_file_descriptors() -> usize {
        #[cfg(target_os = "macos")]
        {
            std::fs::read_dir("/dev/fd")
                .map(|entries| entries.count())
                .unwrap_or(0)
        }
        #[cfg(target_os = "linux")]
        {
            std::fs::read_dir("/proc/self/fd")
                .map(|entries| entries.count())
                .unwrap_or(0)
        }
    }

    let threads_before = thread_count();
    let fds_before = open_file_descriptors();

    const PIPE_CYCLES: u32 = 25;
    for cycle in 0..PIPE_CYCLES {
        let (events, _log) = mpsc::channel();
        let mut run = RunRuntime
            .start(RunStartRequest {
                process_id: ProcessId::new(cycle + 1),
                run_id: RunId::new(u64::from(cycle) + 400),
                command: SpawnCommand::new("/bin/sh").arg("-c").arg("true"),
                mode: RunMode::Pipe,
                events,
                output: mpsc::channel().0,
                ladder: quick_ladder(50, 50),
                metrics_interval: Some(Duration::from_millis(40)),
                on_output_wake: None,
            })
            .expect("cycle run started");
        let outcome = run.wait().expect("natural completion");
        let outcome = run.shutdown().unwrap_or(outcome);
        assert!(
            outcome.cleanup_confirmed && outcome.task_join_failures.is_empty(),
            "cycle {cycle}: {outcome:?}"
        );
    }

    const PTY_CYCLES: u32 = 5;
    for cycle in 0..PTY_CYCLES {
        let geometry = TerminalGeometry::DEFAULT;
        let (events, _log) = mpsc::channel();
        let mut run = RunRuntime
            .start(RunStartRequest {
                process_id: ProcessId::new(cycle + 1),
                run_id: RunId::new(u64::from(cycle) + 500),
                command: SpawnCommand::new("/bin/sh").arg("-c").arg("true"),
                mode: RunMode::Pty {
                    initial_geometry: geometry,
                },
                events,
                output: mpsc::channel().0,
                ladder: quick_ladder(50, 50),
                metrics_interval: Some(Duration::from_millis(40)),
                on_output_wake: None,
            })
            .expect("pty cycle run started");
        let outcome = run.wait().expect("pty natural completion");
        assert!(
            outcome.cleanup_confirmed && outcome.task_join_failures.is_empty(),
            "pty cycle {cycle}: {outcome:?}"
        );
    }

    // Threads include per-run reader/sampler/owner tasks; every one joined.
    // Sibling tests running concurrently inflate instantaneous counts, so
    // instead of one absolute comparison we wait for counts to converge back
    // to the baseline: a real leak never converges, transient sibling noise
    // always does. Bounded OS-state polling with an explicit deadline, not an
    // arbitrary sleep.
    const CONVERGENCE_DEADLINE: Duration = Duration::from_secs(15);
    const THREAD_SLACK: usize = 4;
    const FD_SLACK: usize = 6;
    let deadline = Instant::now() + CONVERGENCE_DEADLINE;
    loop {
        let threads_after = thread_count();
        let fds_after = open_file_descriptors();
        if threads_after <= threads_before + THREAD_SLACK && fds_after <= fds_before + FD_SLACK {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "unbounded growth suspected: threads {threads_before} -> {threads_after}, \
             fds {fds_before} -> {fds_after} after {} ms of settling",
            CONVERGENCE_DEADLINE.as_millis()
        );
        thread::sleep(Duration::from_millis(250));
    }
}
