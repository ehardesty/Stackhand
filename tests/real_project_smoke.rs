use std::fs;
#[cfg(unix)]
use std::net::TcpListener;
use std::process::Command;
#[cfg(unix)]
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use stackhand::model::{
    Autostart, CommandForm, EffectiveProject, Enabled, InputPolicy, ProcessKind, ProcessSpec,
    ReadinessConfig, ReadinessProbe, TerminalMode,
};

#[cfg(unix)]
#[test]
fn startup_timeout_confirms_real_process_tree_cleanup() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("stackhand-timeout-fixture-{unique}"));
    fs::create_dir_all(&dir).expect("fixture directory creates");
    let failed_port = TcpListener::bind(("127.0.0.1", 0))
        .expect("a local port binds")
        .local_addr()
        .expect("local address is available")
        .port();
    let process = ProcessSpec {
        name: "slow".into(),
        kind: ProcessKind::Service,
        enabled: Enabled::Yes,
        autostart: Autostart::No,
        success_exit_codes: vec![0],
        command: CommandForm::Direct {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "sleep 60 & child=$!; trap 'kill \"$child\" 2>/dev/null; exit 0' INT TERM; printf '%s\\n' \"$child\"; wait \"$child\"".into(),
            ],
        },
        working_dir: dir.clone(),
        env: Vec::new(),
        terminal_mode: TerminalMode::Pipe,
        input_policy: InputPolicy::Disabled,
        dependencies: Vec::new(),
        readiness: Some(ReadinessConfig {
            probe: ReadinessProbe::Tcp {
                host: "127.0.0.1".into(),
                port: failed_port,
            },
            initial_delay: Duration::ZERO,
            interval: Duration::from_secs(1),
            timeout: Duration::from_millis(50),
            success_threshold: 1,
            failure_threshold: 1,
            startup_timeout: Some(Duration::from_millis(200)),
        }),
    };
    let project = EffectiveProject::new(vec![process]).expect("timeout project is valid");
    let (supervisor, _consoles, outputs) =
        stackhand::supervisor::start(project).expect("Supervisor starts");
    supervisor.command(stackhand::supervisor::Command::Start("slow".into()));

    let wait_deadline = Instant::now() + Duration::from_secs(5);
    let mut root_pid = None;
    let snapshot = loop {
        let snapshot = supervisor.snapshot().expect("Supervisor serves snapshots");
        let slow = snapshot.named("slow").expect("the fixture defines slow");
        root_pid = root_pid.or(slow.root_pid);
        if slow.lifecycle == stackhand::supervisor::Lifecycle::Stopped
            && slow.current_run.is_none()
            && slow.failure.as_ref().is_some_and(|failure| {
                failure.kind == stackhand::supervisor::FailureKind::Readiness
            })
        {
            break snapshot;
        }
        assert!(
            Instant::now() < wait_deadline,
            "startup timeout did not finish: {slow:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let slow = snapshot.named("slow").expect("the fixture defines slow");
    assert!(
        slow.failure
            .as_ref()
            .is_some_and(|failure| failure.detail.contains("startup timeout"))
    );
    let root_pid = root_pid.expect("the process tree reported a root PID");
    let child_pid = outputs
        .for_process_id(slow.process_id)
        .expect("slow has retained output")
        .snapshot()
        .chunks
        .iter()
        .filter_map(|chunk| match chunk {
            stackhand::supervisor::RetainedChunk::Data { text, .. } => Some(text.as_str()),
            stackhand::supervisor::RetainedChunk::Marker { .. } => None,
        })
        .flat_map(str::lines)
        .find_map(|line| line.trim().parse::<u32>().ok())
        .expect("the child PID reached retained output");

    supervisor.stop_task();
    assert!(unsafe { libc::kill(root_pid as libc::pid_t, 0) } < 0);
    assert!(unsafe { libc::kill(child_pid as libc::pid_t, 0) } < 0);
    fs::remove_dir_all(dir).ok();
}

#[test]
fn stackhand_repository_runs_as_a_small_real_project() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("stackhand-real-smoke-{unique}"));
    fs::create_dir_all(&dir).expect("smoke directory creates");
    let config = dir.join("stackhand.yaml");
    let repository = env!("CARGO_MANIFEST_DIR");
    let cargo = env!("CARGO");
    fs::write(
        &config,
        format!(
            "version: 1\n\
             processes:\n\
             \x20 - name: inspect\n\
             \x20   kind: one-shot\n\
             \x20   terminal: pipe\n\
             \x20   working_dir: \"{repository}\"\n\
             \x20   command:\n\
             \x20     program: \"{cargo}\"\n\
             \x20     args: [\"metadata\", \"--no-deps\", \"--format-version\", \"1\"]\n\
             \x20 - name: hold\n\
             \x20   depends_on: [{{name: inspect, condition: completed_successfully}}]\n\
             \x20   terminal: pipe\n\
             \x20   command:\n\
             \x20     program: /bin/sleep\n\
             \x20     args: [\"60\"]\n"
        ),
    )
    .expect("smoke configuration writes");

    let output = Command::new(env!("CARGO_BIN_EXE_stackhand"))
        .arg("--fixture-smoke")
        .arg(&config)
        .output()
        .expect("smoke fixture runs");
    assert!(
        output.status.success(),
        "smoke failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("real-project-cycles-ok"), "{stdout}");
    assert!(stdout.contains("real-project-smoke-ok"), "{stdout}");
    fs::remove_dir_all(dir).ok();
}
