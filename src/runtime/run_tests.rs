//! Tests for the Run ownership seam. They exercise only public behavior
//! through `RunRuntime::start`, the returned `OwnedRun`, and its handles.

use super::*;
use crate::geometry::TerminalGeometry;
use std::sync::mpsc;
use std::time::Duration;

mod pty_seam_tests {

    use super::*;
    use std::thread;
    use std::time::{Duration, Instant};

    const WAIT: Duration = Duration::from_secs(5);

    fn start_marker_run() -> (OwnedRun, mpsc::Receiver<RunEvent>) {
        let (events, receiver) = mpsc::channel();
        let command = SpawnCommand::new("/bin/sh")
            .arg("-c")
            .arg("printf 'run-ready\\n'; IFS= read -r line; printf 'run-done\\n'");
        let run = RunRuntime
            .start(RunStartRequest {
                process_id: ProcessId::new(7),
                run_id: RunId::new(42),
                command,
                mode: RunMode::Pty {
                    initial_geometry: TerminalGeometry::DEFAULT,
                },
                events,
                output: mpsc::channel().0,
                ladder: Default::default(),
                on_output_wake: None,
            })
            .expect("fixture run started");
        (run, receiver)
    }

    fn wait_for_output(handle: &TerminalHandle<'_>, marker: &str) -> bool {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if handle.snapshot().text().contains(marker) {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        false
    }

    #[test]
    fn run_reports_identity_and_joins_on_explicit_cleanup() {
        let mut run = RunRuntime
            .start(RunStartRequest {
                process_id: ProcessId::new(3),
                run_id: RunId::new(9),
                command: SpawnCommand::new("/bin/sh").arg("-c").arg("sleep 30"),
                mode: RunMode::Pty {
                    initial_geometry: TerminalGeometry::DEFAULT,
                },
                events: mpsc::channel().0,
                output: mpsc::channel().0,
                ladder: Default::default(),
                on_output_wake: None,
            })
            .unwrap();

        assert_eq!(run.process_id(), ProcessId::new(3));
        assert_eq!(run.run_id(), RunId::new(9));
        assert!(run.root_pid().is_some());

        run.shutdown().expect("run cleaned up");
        // Repeated shutdown observes the first cleanup instead of repeating.
        run.shutdown().expect("repeated cleanup stayed successful");
    }

    #[test]
    fn natural_exit_and_explicit_cleanup_both_join_terminal_tasks() {
        let (mut run, receiver) = start_marker_run();
        assert_eq!(run.run_id(), RunId::new(42));

        let handle = run.terminal().expect("PTY fixture");
        assert!(
            wait_for_output(&handle, "run-ready"),
            "fixture output did not appear"
        );
        handle.send_raw(vec![0x04]);

        let spawned = receiver.recv_timeout(WAIT).expect("spawn event arrived");
        assert_eq!(spawned.run_id, RunId::new(42));
        assert!(matches!(
            spawned.kind,
            RunEventKind::Spawned { root_pid: Some(_) }
        ));

        assert!(
            wait_for_output(&handle, "run-done"),
            "root process did not reach natural exit"
        );
        // Cleanup after a natural root exit still finalizes the TerminalSession
        // and joins every terminal task.
        run.shutdown().expect("run joined after natural exit");

        let final_event = receiver.recv_timeout(WAIT).expect("final event arrived");
        assert_eq!(final_event.run_id, RunId::new(42));
        assert_eq!(final_event.kind, RunEventKind::ShutdownComplete);
    }

    #[test]
    fn every_low_volume_event_carries_the_requested_run_id() {
        let (_run, receiver) = start_marker_run();
        while let Ok(event) = receiver.try_recv() {
            assert_eq!(event.run_id, RunId::new(42));
        }
    }
}

mod pipe_tests {

    use super::*;
    use crate::runtime::pipe::{OutputStream, RunOutput};
    use std::sync::mpsc::{Receiver, TryRecvError};
    use std::thread;

    const WAIT: Duration = Duration::from_secs(10);

    fn start_pipe(command: SpawnCommand) -> (OwnedRun, Receiver<RunEvent>, Receiver<RunOutput>) {
        let (events, event_receiver) = mpsc::channel();
        let (output, output_receiver) = mpsc::channel();
        let run = RunRuntime
            .start(RunStartRequest {
                process_id: ProcessId::new(11),
                run_id: RunId::new(77),
                command,
                mode: RunMode::Pipe,
                events,
                output,
                ladder: Default::default(),
                on_output_wake: None,
            })
            .expect("pipe run started");
        (run, event_receiver, output_receiver)
    }

    fn drain_output(receiver: &Receiver<RunOutput>) -> Vec<RunOutput> {
        let mut chunks = Vec::new();
        while let Ok(chunk) = receiver.try_recv() {
            chunks.push(chunk);
        }
        chunks
    }

    fn text(chunks: &[RunOutput], stream: OutputStream) -> String {
        String::from_utf8_lossy(
            &chunks
                .iter()
                .filter(|chunk| chunk.stream == stream)
                .flat_map(|chunk| chunk.data.clone())
                .collect::<Vec<u8>>(),
        )
        .into_owned()
    }

    #[test]
    fn pipe_mode_starts_a_direct_command_and_reaps_natural_exit() {
        let (mut run, events, output) =
            start_pipe(SpawnCommand::new("/bin/echo").arg("hello-direct"));

        let exit = run.wait().expect("direct command completed");
        assert_eq!(exit.exit_code, Some(0));

        let chunks = drain_output(&output);
        assert_eq!(text(&chunks, OutputStream::Stdout), "hello-direct\n");
        assert!(chunks.iter().all(|chunk| chunk.run_id == RunId::new(77)));

        let spawned = events.recv_timeout(WAIT).expect("spawn event");
        assert_eq!(spawned.run_id, RunId::new(77));
        assert!(matches!(spawned.kind, RunEventKind::Spawned { .. }));
        let exited = events.recv_timeout(WAIT).expect("exit event");
        assert_eq!(exited.kind, RunEventKind::Exited { code: Some(0) });

        // The root process was reaped by wait(); cleanup stays successful.
        run.shutdown().expect("run joined");
    }

    #[test]
    fn pipe_mode_preserves_stream_identity_for_shell_commands() {
        let (mut run, _events, output) = start_pipe(
            SpawnCommand::new("/bin/sh")
                .arg("-c")
                .arg("printf to-out; printf to-err >&2"),
        );

        run.wait().expect("shell command completed");
        let chunks = drain_output(&output);
        assert!(text(&chunks, OutputStream::Stdout).contains("to-out"));
        assert!(text(&chunks, OutputStream::Stderr).contains("to-err"));
        assert!(!text(&chunks, OutputStream::Stdout).contains("to-err"));
        run.shutdown().expect("run joined");
    }

    #[test]
    fn pipe_mode_rejects_resize_without_harming_the_run() {
        let (mut run, _events, _output) =
            start_pipe(SpawnCommand::new("/bin/sh").arg("-c").arg("sleep 30"));

        let rejection = run.resize(TerminalGeometry::DEFAULT);
        assert_eq!(rejection, Err(ResizeRejected::Unsupported));

        // The Run is still healthy and controllable after the rejection.
        run.shutdown().expect("run stopped after rejected resize");
    }

    #[test]
    fn high_output_pipe_completes_and_keeps_bytes_out_of_the_event_sink() {
        let lines = 20_000u32;
        let (mut run, events, output) = start_pipe(
            SpawnCommand::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "i=0; while [ \"$i\" -lt {lines} ]; do printf 'line-%06d\\n' \"$i\"; i=$((i+1)); done"
                )),
        );

        let exit = run.wait().expect("high-output run completed");
        assert_eq!(exit.exit_code, Some(0));
        thread::sleep(Duration::from_millis(50));
        let chunks = drain_output(&output);
        let body = text(&chunks, OutputStream::Stdout);
        let expected_last = format!("line-{:06}", lines - 1);
        assert!(
            body.contains(&expected_last),
            "final line missing from drained output"
        );
        assert_eq!(
            body.lines().count(),
            usize::try_from(lines).unwrap(),
            "every output line must reach the high-volume path"
        );

        // Only lifecycle events may exist on the low-volume sink.
        loop {
            match events.try_recv() {
                Ok(event) => assert!(matches!(
                    event.kind,
                    RunEventKind::Spawned { .. } | RunEventKind::Exited { .. }
                )),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        run.shutdown().expect("run joined");
    }
}

mod process_tree_tests {

    use super::*;
    use crate::runtime::pipe::OutputStream;
    use crate::runtime::process_tree::{SemanticSignal, UnixProcessTree};
    use std::sync::mpsc::Receiver;
    use std::thread;
    use std::time::Instant;

    const WAIT: Duration = Duration::from_secs(10);

    struct StartedRun {
        run: OwnedRun,
        #[allow(dead_code)] // kept for symmetry with other fixtures; read by callers as needed
        events: Receiver<RunEvent>,
        output: Receiver<crate::runtime::pipe::RunOutput>,
        root_pid: u32,
    }

    fn start_pipe(command: SpawnCommand) -> StartedRun {
        let (events, event_receiver) = mpsc::channel();
        let (output, output_receiver) = mpsc::channel();
        let run = RunRuntime
            .start(RunStartRequest {
                process_id: ProcessId::new(21),
                run_id: RunId::new(88),
                command,
                mode: RunMode::Pipe,
                events,
                output,
                ladder: Default::default(),
                on_output_wake: None,
            })
            .expect("pipe run started");
        let root_pid = run.root_pid().expect("pipe mode reports a root pid").get();
        StartedRun {
            run,
            events: event_receiver,
            output: output_receiver,
            root_pid,
        }
    }

    /// Read live stdout until every marker is present. Never calls wait()
    /// or shutdown(): the Run stays active while markers accumulate.
    fn read_stdout_until(started: &mut StartedRun, markers: &[&str]) -> String {
        let mut text = String::new();
        let deadline = Instant::now() + WAIT;
        loop {
            while let Ok(chunk) = started.output.try_recv() {
                if chunk.stream == OutputStream::Stdout {
                    text.push_str(&String::from_utf8_lossy(&chunk.data));
                }
            }
            if markers.iter().all(|marker| text.contains(marker)) {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "markers {markers:?} not seen; accumulated {text:?}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Collect whatever remains after shutdown or confirmed completion.
    fn drain_stdout(started: &mut StartedRun) -> String {
        let mut text = String::new();
        while let Ok(chunk) = started.output.recv_timeout(WAIT) {
            if chunk.stream == OutputStream::Stdout {
                text.push_str(&String::from_utf8_lossy(&chunk.data));
            }
        }
        while let Ok(chunk) = started.output.try_recv() {
            if chunk.stream == OutputStream::Stdout {
                text.push_str(&String::from_utf8_lossy(&chunk.data));
            }
        }
        text
    }

    fn pids_alive(pids: &[u32]) -> Vec<u32> {
        UnixProcessTree::confirm_gone(pids)
    }

    #[test]
    fn direct_executable_receives_each_semantic_signal() {
        // The busy-wait keeps the fixture alive without depending on stdin.
        let script = "trap 'echo caught-int; exit 3' INT; trap 'echo caught-term; exit 4' TERM; echo ready; while :; do sleep 3600; done";
        let cases = [
            (SemanticSignal::Interrupt, "caught-int", 3),
            (SemanticSignal::Terminate, "caught-term", 4),
        ];
        for (semantic, marker, code) in cases {
            let mut started = start_pipe(SpawnCommand::new("/bin/sh").arg("-c").arg(script));

            // Wait for the trap to be installed via the readiness marker.
            read_stdout_until(&mut started, &["ready"]);

            match semantic {
                SemanticSignal::Interrupt => started.run.interrupt().expect("interrupt"),
                SemanticSignal::Terminate => started.run.terminate().expect("terminate"),
                SemanticSignal::Kill => started.run.kill().expect("kill"),
            }

            let exit = started.run.wait().expect("exit observed");
            assert_eq!(exit.exit_code, Some(code), "{semantic:?} was not delivered");
            let output = drain_stdout(&mut started);
            assert!(
                output.contains(marker),
                "signal {semantic:?} did not reach its trap: {output:?}"
            );
            started.run.shutdown().expect("cleanup");
        }

        // SIGKILL cannot be trapped; it must end an unresponsive sleep.
        let mut started = start_pipe(SpawnCommand::new("/bin/sh").arg("-c").arg("sleep 60"));
        started.run.kill().expect("kill delivered");
        started.run.wait().expect("SIGKILL ended the sleep");
        started.run.shutdown().expect("cleanup");
    }

    #[test]
    fn shell_wrapper_without_exec_stops_child_and_grandchild_together() {
        let script = "\
sleep 300 & echo CHILD=$!;\
sh -c 'sleep 300 & echo GRAND=$!; wait' &\
wait";
        let mut started = start_pipe(SpawnCommand::new("/bin/sh").arg("-c").arg(script));

        // Read live output first; the fixture blocks on `wait`, so calling
        // run.wait() before shutdown would block forever by design.
        let output = read_stdout_until(&mut started, &["CHILD=", "GRAND="]);
        let child_pid: u32 = output
            .lines()
            .find_map(|line| {
                line.strip_prefix("CHILD=")
                    .and_then(|v| v.trim().parse().ok())
            })
            .expect("child pid reported");
        let grandchild_pid: u32 = output
            .lines()
            .find_map(|line| {
                line.strip_prefix("GRAND=")
                    .and_then(|v| v.trim().parse().ok())
            })
            .expect("grandchild pid reported");

        let tracked = vec![started.root_pid, child_pid, grandchild_pid];
        started.run.shutdown().expect("complete cleanup");

        // Drain only after cleanup so no final output is lost.
        let _rest = drain_stdout(&mut started);
        let still_alive = pids_alive(&tracked);
        assert!(
            still_alive.is_empty(),
            "Process Tree members survived cleanup: {still_alive:?}"
        );
    }

    #[test]
    fn exec_wrapper_is_contained_and_signaled() {
        let mut started = start_pipe(SpawnCommand::new("/bin/sh").arg("-c").arg("exec sleep 300"));
        let root = started.root_pid;

        started.run.terminate().expect("terminate delivered");
        let exit = started.run.wait().expect("exec'd root exited");
        assert_ne!(
            exit.exit_code,
            Some(0),
            "SIGTERM should have ended the sleep"
        );
        started.run.shutdown().expect("cleanup");
        assert!(pids_alive(&[root]).is_empty(), "root survived cleanup");
    }

    #[test]
    fn root_exit_alone_does_not_report_a_clean_process_tree() {
        // The root shell exits immediately but leaves one descendant running.
        // That descendant inherits stdout and stderr, so reader EOF (and
        // therefore Run completion) must not arrive before containment work.
        let mut started = start_pipe(
            SpawnCommand::new("/bin/sh")
                .arg("-c")
                .arg("sleep 300 & echo LEFT=$!"),
        );

        // Read LEFT= while the Run is active.
        let output = read_stdout_until(&mut started, &["LEFT="]);
        let descendant: u32 = output
            .lines()
            .find_map(|line| {
                line.strip_prefix("LEFT=")
                    .and_then(|v| v.trim().parse().ok())
            })
            .expect("descendant pid reported");

        // Root exit is not Run completion: the owned group still holds the
        // descendant, and it is observable without reaping the root.
        let tree = UnixProcessTree::from_root(started.root_pid);
        let members = tree
            .remaining_members_excluding_root()
            .expect("group enumeration");
        assert!(
            members.contains(&descendant),
            "descendant vanished before we could observe it"
        );

        // Escalation removes the survivor while the group identity holds.
        started.run.kill().expect("tree kill");
        let deadline = Instant::now() + WAIT;
        while !pids_alive(&[descendant]).is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(pids_alive(&[descendant]).is_empty());

        // Only now does cleanup reap the root and join the drains.
        started.run.shutdown().expect("cleanup");
        // Final output was already consumed by the live read; draining just
        // collects whatever the readers flushed during cleanup.
        let _rest = drain_stdout(&mut started);
        let tracked = vec![started.root_pid, descendant];
        let still_alive = pids_alive(&tracked);
        assert!(
            still_alive.is_empty(),
            "root or descendant survived cleanup: {still_alive:?}"
        );
    }

    #[test]
    fn signals_never_reach_an_unrelated_process() {
        // Start an independent long-running process outside any Run.
        let mut unrelated = std::process::Command::new("/bin/sleep")
            .arg("300")
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("unrelated process started");
        let unrelated_pid = unrelated.id();

        let mut started = start_pipe(SpawnCommand::new("/bin/sh").arg("-c").arg("sleep 300"));
        started.run.kill().expect("owned tree killed");
        started.run.shutdown().expect("cleanup");

        let alive = pids_alive(&[unrelated_pid]);
        let _ = unrelated.kill();
        let _ = unrelated.wait();
        assert_eq!(
            alive,
            vec![unrelated_pid],
            "the unrelated process was disturbed by Run signals"
        );
    }

    #[test]
    fn signaling_an_already_empty_tree_is_harmless() {
        let mut started = start_pipe(SpawnCommand::new("/bin/sh").arg("-c").arg("true"));
        started.run.wait().expect("natural completion");
        // The whole tree is gone now; every semantic action stays harmless.
        started.run.interrupt().expect("idempotent interrupt");
        started.run.terminate().expect("idempotent terminate");
        started.run.kill().expect("idempotent kill");
        started.run.shutdown().expect("cleanup");
    }

    #[test]
    fn pty_mode_runs_use_the_same_tree_policy() {
        let geometry = TerminalGeometry::DEFAULT;
        let (events, _log) = mpsc::channel();
        let mut run = RunRuntime
            .start(RunStartRequest {
                process_id: ProcessId::new(22),
                run_id: RunId::new(89),
                command: SpawnCommand::new("/bin/sh").arg("-c").arg("sleep 300"),
                mode: RunMode::Pty {
                    initial_geometry: geometry,
                },
                events,
                output: mpsc::channel().0,
                ladder: Default::default(),
                on_output_wake: None,
            })
            .expect("pty run started");
        let root = run.root_pid().expect("pty root pid");

        run.interrupt().expect("tree interrupt through PTY seam");
        let exit = run.wait().expect("PTY root exited after interrupt");
        assert_ne!(exit.exit_code, None);
        run.shutdown().expect("cleanup joins terminal tasks");
        assert!(pids_alive(&[root.get()]).is_empty());
    }
}
