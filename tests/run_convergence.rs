#![cfg(unix)]

use std::io::Write;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use stackhand::model::{
    Autostart, CommandForm, EffectiveProject, Enabled, InputPolicy, ProcessKind, ProcessSpec,
    ReadinessCheck, ReadinessConfig, ReadinessProbe, RestartConfig, RestartPolicy, ShellConfig,
    TerminalMode,
};
use stackhand::supervisor::{
    Command, DesiredState, FailureKind, Lifecycle, OutputViews, ProcessId, ProcessSnapshot,
    ProjectSnapshot, RECENT_RUNS, SupervisorHandle,
};

const POLL: Duration = Duration::from_millis(10);
const PROCESS_WAIT: Duration = Duration::from_secs(8);
const RESOURCE_WAIT: Duration = Duration::from_secs(10);
const THREAD_SLACK: usize = 8;
const FD_SLACK: usize = 10;

static RESOURCE_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn resource_test_guard() -> std::sync::MutexGuard<'static, ()> {
    RESOURCE_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct BlockedHttpEndpoint {
    port: u16,
    accepted: mpsc::Receiver<()>,
    release: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl BlockedHttpEndpoint {
    fn new() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("HTTP endpoint binds");
        listener
            .set_nonblocking(true)
            .expect("HTTP endpoint accepts nonblocking mode");
        let port = listener
            .local_addr()
            .expect("HTTP endpoint address is available")
            .port();
        let (accepted_tx, accepted) = mpsc::channel();
        let release = Arc::new(AtomicBool::new(false));
        let worker_release = Arc::clone(&release);
        let worker = thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = accepted_tx.send(());
                        while !worker_release.load(Ordering::Acquire) {
                            thread::sleep(POLL);
                        }
                        let _ = stream.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok");
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if worker_release.load(Ordering::Acquire) {
                            return;
                        }
                        thread::sleep(POLL);
                    }
                    Err(_) => return,
                }
            }
        });
        Self {
            port,
            accepted,
            release,
            worker: Some(worker),
        }
    }

    fn wait_until_accepted(&self) {
        self.accepted
            .recv_timeout(PROCESS_WAIT)
            .expect("readiness probe reached the blocked endpoint");
    }

    fn release(&self) {
        self.release.store(true, Ordering::Release);
    }
}

impl Drop for BlockedHttpEndpoint {
    fn drop(&mut self) {
        self.release();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn service(
    name: &str,
    command: CommandForm,
    readiness: Option<ReadinessConfig>,
    restart: RestartConfig,
) -> ProcessSpec {
    ProcessSpec {
        name: name.to_string(),
        kind: ProcessKind::Service,
        enabled: Enabled::Yes,
        autostart: Autostart::No,
        success_exit_codes: vec![0],
        restart,
        command,
        working_dir: std::env::temp_dir(),
        env: Vec::new(),
        terminal_mode: TerminalMode::Pipe,
        input_policy: InputPolicy::Disabled,
        dependencies: Vec::new(),
        readiness,
        liveness: None,
    }
}

fn plain_service(name: &str) -> ProcessSpec {
    service(
        name,
        CommandForm::Direct {
            program: "/bin/sleep".into(),
            args: vec!["60".into()],
        },
        None,
        RestartConfig::default(),
    )
}

fn blocked_service(name: &str, port: u16, command: &str) -> ProcessSpec {
    service(
        name,
        CommandForm::Shell {
            text: command.to_string(),
        },
        Some(ReadinessConfig {
            checks: vec![ReadinessCheck {
                probe: ReadinessProbe::Http {
                    host: "127.0.0.1".to_string(),
                    port,
                    path: "/healthz".to_string(),
                },
                initial_delay: Duration::ZERO,
                interval: Duration::from_millis(20),
                timeout: Duration::from_secs(5),
                success_threshold: 1,
                failure_threshold: 1,
            }],
            startup_timeout: Some(Duration::from_secs(10)),
        }),
        RestartConfig::default(),
    )
}

fn wait_for_process<F>(supervisor: &SupervisorHandle, name: &str, ready: F) -> ProcessSnapshot
where
    F: Fn(&ProcessSnapshot) -> bool,
{
    let deadline = Instant::now() + PROCESS_WAIT;
    loop {
        let snapshot = supervisor
            .snapshot()
            .expect("Supervisor serves snapshots while the fixture runs");
        let process = snapshot.named(name).expect("fixture Process exists");
        if ready(process) {
            return process.clone();
        }
        assert!(
            Instant::now() < deadline,
            "{name} did not reach the expected state: {process:?}"
        );
        thread::sleep(POLL);
    }
}

fn wait_for_output(outputs: &OutputViews, process_id: ProcessId, marker: &str) {
    let output = outputs
        .for_process_id(process_id)
        .expect("fixture Process has retained output");
    let deadline = Instant::now() + PROCESS_WAIT;
    loop {
        let snapshot = output.snapshot();
        if snapshot.chunks.iter().any(|chunk| {
            matches!(
                chunk,
                stackhand::supervisor::RetainedChunk::Data { text, .. } if text.contains(marker)
            )
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "output marker {marker:?} was not retained: {snapshot:?}"
        );
        thread::sleep(POLL);
    }
}

fn assert_snapshot_is_responsive(supervisor: &SupervisorHandle) -> ProjectSnapshot {
    let started = Instant::now();
    let snapshot = supervisor
        .snapshot()
        .expect("Supervisor serves a snapshot during blocked adapter work");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "a blocked adapter delayed the snapshot: {:?}",
        started.elapsed()
    );
    snapshot
}

fn assert_stays_stopped(supervisor: &SupervisorHandle, name: &str) {
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        let snapshot = supervisor
            .snapshot()
            .expect("Supervisor serves snapshots after cancellation");
        let process = snapshot.named(name).expect("fixture Process exists");
        assert_eq!(process.lifecycle, Lifecycle::Stopped);
        assert_eq!(process.current_run, None);
        assert_eq!(process.readiness, None);
        if Instant::now() >= deadline {
            return;
        }
        thread::sleep(POLL);
    }
}

fn resource_counts() -> (Option<usize>, Option<usize>) {
    (thread_count(), open_fd_count())
}

fn wait_for_resource_convergence(before: (Option<usize>, Option<usize>)) {
    let deadline = Instant::now() + RESOURCE_WAIT;
    loop {
        let after = resource_counts();
        let threads_ok = within_slack(before.0, after.0, THREAD_SLACK);
        let fds_ok = within_slack(before.1, after.1, FD_SLACK);
        if threads_ok && fds_ok {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "resource counts did not converge: before {before:?}, after {after:?}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn within_slack(before: Option<usize>, after: Option<usize>, slack: usize) -> bool {
    match (before, after) {
        (Some(before), Some(after)) => after <= before.saturating_add(slack),
        _ => true,
    }
}

fn open_fd_count() -> Option<usize> {
    std::fs::read_dir("/dev/fd").ok().map(Iterator::count)
}

#[cfg(target_os = "macos")]
fn thread_count() -> Option<usize> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-M", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    output.status.success().then(|| {
        output
            .stdout
            .split(|byte| *byte == b'\n')
            .count()
            .saturating_sub(2)
    })
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

#[test]
fn blocked_readiness_cancellation_keeps_control_responsive_and_drains_output() {
    let _resource_guard = resource_test_guard();
    const OUTPUT_MARKER: &str = "blocked-readiness-output-marker";

    let endpoint = BlockedHttpEndpoint::new();
    let noisy = blocked_service(
        "noisy",
        endpoint.port,
        "while :; do printf 'blocked-readiness-output-marker\\n'; done",
    );
    let quiet = plain_service("quiet");
    let project = EffectiveProject::with_shell(vec![noisy, quiet], ShellConfig::default())
        .expect("blocked-readiness Project is valid");
    let (supervisor, consoles, outputs) =
        stackhand::supervisor::start(project).expect("Supervisor starts");

    supervisor.command(Command::Start("noisy".to_string()));
    endpoint.wait_until_accepted();
    let noisy = wait_for_process(&supervisor, "noisy", |process| {
        process
            .readiness
            .as_ref()
            .is_some_and(|readiness| readiness.attempts >= 1)
    });
    wait_for_output(&outputs, noisy.process_id, OUTPUT_MARKER);

    supervisor.command(Command::Start("quiet".to_string()));
    wait_for_process(&supervisor, "quiet", |process| {
        process.lifecycle == Lifecycle::Running
    });

    let stop_started = Instant::now();
    supervisor.command(Command::Stop("noisy".to_string()));
    let stopping = assert_snapshot_is_responsive(&supervisor);
    let noisy = stopping.named("noisy").expect("noisy Process exists");
    assert_eq!(noisy.desired, DesiredState::Stopped);
    assert!(stop_started.elapsed() < Duration::from_secs(2));

    wait_for_process(&supervisor, "noisy", |process| {
        process.lifecycle == Lifecycle::Stopped && process.current_run.is_none()
    });
    assert_stays_stopped(&supervisor, "noisy");
    assert_eq!(
        supervisor
            .snapshot()
            .expect("quiet snapshot exists")
            .named("quiet")
            .expect("quiet Process exists")
            .lifecycle,
        Lifecycle::Running,
        "one noisy Process must not delay another Process's lifecycle control"
    );

    // Release the real endpoint only after the Run has stopped. Any response
    // that arrives now is a late result from the canceled readiness attempt.
    endpoint.release();
    drop(endpoint);
    assert_stays_stopped(&supervisor, "noisy");
    wait_for_process(&supervisor, "quiet", |process| {
        process.lifecycle == Lifecycle::Running
    });

    supervisor.command(Command::Stop("quiet".to_string()));
    wait_for_process(&supervisor, "quiet", |process| {
        process.lifecycle == Lifecycle::Stopped && process.current_run.is_none()
    });
    supervisor.stop_task();
    drop(consoles);
    drop(outputs);
}

#[test]
fn repeated_failed_restarts_bound_history_and_resources() {
    let _resource_guard = resource_test_guard();
    const MAX_RESTARTS: u32 = 12;
    let before = resource_counts();
    let process = service(
        "flaky",
        CommandForm::Shell {
            text: "exit 1".to_string(),
        },
        None,
        RestartConfig {
            policy: RestartPolicy::OnFailure,
            backoff: Duration::from_millis(10),
            max_restarts: MAX_RESTARTS,
            on_unhealthy: false,
        },
    );
    let project = EffectiveProject::new(vec![process]).expect("restart Project is valid");
    let (supervisor, consoles, outputs) =
        stackhand::supervisor::start(project).expect("Supervisor starts");
    supervisor.command(Command::Start("flaky".to_string()));

    let flaky = wait_for_process(&supervisor, "flaky", |process| {
        process.lifecycle == Lifecycle::Stopped
            && process.current_run.is_none()
            && process
                .failure
                .as_ref()
                .is_some_and(|failure| failure.kind == FailureKind::RestartLimit)
    });
    assert_eq!(flaky.desired, DesiredState::Running);
    assert_eq!(flaky.recent_runs.len(), RECENT_RUNS);
    assert_eq!(
        flaky.automatic_restart_budget.automatic_retries_used,
        MAX_RESTARTS
    );
    assert!(flaky.automatic_restart_budget.exhausted);

    supervisor.stop_task();
    drop(consoles);
    drop(outputs);
    wait_for_resource_convergence(before);
}
