use std::fs;
#[cfg(unix)]
use std::net::TcpListener;
use std::process::Command;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use stackhand::model::{
    Autostart, CommandForm, EffectiveProject, Enabled, InputPolicy, ProcessKind, ProcessSpec,
    ReadinessCheck, ReadinessConfig, ReadinessProbe, RestartConfig, ShellConfig, TerminalMode,
};

#[cfg(unix)]
fn host_tcp_readiness_endpoint() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("TCP endpoint binds");
    let port = listener
        .local_addr()
        .expect("TCP address is available")
        .port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            drop(stream);
        }
    });
    port
}

#[cfg(unix)]
fn host_switching_http_endpoint(healthy: Arc<AtomicBool>) -> u16 {
    use std::io::Write;

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("HTTP endpoint binds");
    let port = listener
        .local_addr()
        .expect("HTTP address is available")
        .port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let response = if healthy.load(Ordering::Acquire) {
                b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok".as_slice()
            } else {
                b"HTTP/1.0 503 Unavailable\r\n\r\n".as_slice()
            };
            let mut stream = stream;
            let _ = stream.write_all(response);
        }
    });
    port
}

#[cfg(unix)]
#[test]
fn all_readiness_uses_real_tcp_and_http_children_for_failure_and_recovery() {
    let healthy = Arc::new(AtomicBool::new(false));
    let tcp_port = host_tcp_readiness_endpoint();
    let http_port = host_switching_http_endpoint(Arc::clone(&healthy));
    let process = readiness_test_process(tcp_port, http_port);
    let project = EffectiveProject::new(vec![process]).expect("all readiness project is valid");
    let (supervisor, _consoles, _outputs) =
        stackhand::supervisor::start(project).expect("Supervisor starts");
    supervisor.command(stackhand::supervisor::Command::Start("mixed".into()));

    let mixed = wait_for_process(&supervisor, "mixed", |process| {
        process.readiness.as_ref().is_some_and(|readiness| {
            readiness.last_error.as_deref() == Some("all child 2: status 503")
        })
    });
    assert_eq!(mixed.lifecycle, stackhand::supervisor::Lifecycle::Starting);
    assert_eq!(
        mixed.readiness.as_ref().unwrap().kind,
        stackhand::supervisor::ReadinessCheckKind::All
    );
    assert_eq!(
        mixed.readiness.as_ref().unwrap().children[0].state,
        stackhand::supervisor::ReadinessState::Passing
    );
    assert_eq!(
        mixed.readiness.as_ref().unwrap().children[1].state,
        stackhand::supervisor::ReadinessState::Pending
    );

    healthy.store(true, Ordering::Release);
    wait_for_process(&supervisor, "mixed", |process| {
        process.lifecycle == stackhand::supervisor::Lifecycle::Running
            && process.readiness.as_ref().is_some_and(|readiness| {
                readiness.state == stackhand::supervisor::ReadinessState::Passing
            })
    });

    healthy.store(false, Ordering::Release);
    wait_for_process(&supervisor, "mixed", |process| {
        process.lifecycle == stackhand::supervisor::Lifecycle::Running
            && process.readiness.as_ref().is_some_and(|readiness| {
                readiness.state == stackhand::supervisor::ReadinessState::Failing
            })
    });

    healthy.store(true, Ordering::Release);
    wait_for_process(&supervisor, "mixed", |process| {
        process.readiness.as_ref().is_some_and(|readiness| {
            readiness.state == stackhand::supervisor::ReadinessState::Passing
        })
    });

    stop_process(&supervisor, "mixed");
    supervisor.stop_task();
}

#[cfg(unix)]
fn readiness_test_process(tcp_port: u16, http_port: u16) -> ProcessSpec {
    ProcessSpec {
        name: "mixed".into(),
        kind: ProcessKind::Service,
        enabled: Enabled::Yes,
        autostart: Autostart::No,
        success_exit_codes: vec![0],
        restart: RestartConfig::default(),
        command: CommandForm::Direct {
            program: "/bin/sleep".into(),
            args: vec!["60".into()],
        },
        working_dir: std::env::temp_dir(),
        env: Vec::new(),
        terminal_mode: TerminalMode::Pipe,
        input_policy: InputPolicy::Disabled,
        dependencies: Vec::new(),
        readiness: Some(ReadinessConfig {
            checks: vec![
                ReadinessCheck {
                    probe: ReadinessProbe::Tcp {
                        host: "127.0.0.1".into(),
                        port: tcp_port,
                    },
                    initial_delay: Duration::ZERO,
                    interval: Duration::from_millis(20),
                    timeout: Duration::from_millis(100),
                    success_threshold: 1,
                    failure_threshold: 1,
                },
                ReadinessCheck {
                    probe: ReadinessProbe::Http {
                        host: "127.0.0.1".into(),
                        port: http_port,
                        path: "/healthz".into(),
                    },
                    initial_delay: Duration::ZERO,
                    interval: Duration::from_millis(20),
                    timeout: Duration::from_millis(100),
                    success_threshold: 1,
                    failure_threshold: 1,
                },
            ],
            startup_timeout: Some(Duration::from_secs(2)),
        }),
        liveness: None,
    }
}

#[cfg(unix)]
fn exec_service(
    name: &str,
    probe: ReadinessProbe,
    timeout: Duration,
    working_dir: std::path::PathBuf,
    env: Vec<(String, String)>,
) -> ProcessSpec {
    ProcessSpec {
        name: name.into(),
        kind: ProcessKind::Service,
        enabled: Enabled::Yes,
        autostart: Autostart::No,
        success_exit_codes: vec![0],
        restart: RestartConfig::default(),
        command: CommandForm::Direct {
            program: "/bin/sleep".into(),
            args: vec!["60".into()],
        },
        working_dir,
        env,
        terminal_mode: TerminalMode::Pipe,
        input_policy: InputPolicy::Disabled,
        dependencies: Vec::new(),
        readiness: Some(ReadinessConfig {
            checks: vec![ReadinessCheck {
                probe,
                initial_delay: Duration::ZERO,
                interval: Duration::from_millis(20),
                timeout,
                success_threshold: 1,
                failure_threshold: 1,
            }],
            startup_timeout: None,
        }),
        liveness: None,
    }
}

#[cfg(unix)]
fn wait_for_process<F>(
    supervisor: &stackhand::supervisor::SupervisorHandle,
    name: &str,
    ready: F,
) -> stackhand::supervisor::ProcessSnapshot
where
    F: Fn(&stackhand::supervisor::ProcessSnapshot) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = supervisor.snapshot().expect("Supervisor serves snapshots");
        let process = snapshot.named(name).expect("fixture Process exists");
        if ready(process) {
            return process.clone();
        }
        assert!(
            Instant::now() < deadline,
            "{name} did not reach the expected state: {process:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn stop_process(supervisor: &stackhand::supervisor::SupervisorHandle, name: &str) {
    supervisor.command(stackhand::supervisor::Command::Stop(name.into()));
    wait_for_process(supervisor, name, |process| {
        process.lifecycle == stackhand::supervisor::Lifecycle::Stopped
            && process.current_run.is_none()
    });
}

#[cfg(unix)]
fn assert_process_output_has_no_data(
    outputs: &stackhand::supervisor::OutputViews,
    process_id: stackhand::supervisor::ProcessId,
) {
    let snapshot = outputs
        .for_process_id(process_id)
        .expect("fixture Process has output")
        .snapshot();
    assert!(
        snapshot
            .chunks
            .iter()
            .all(|chunk| matches!(chunk, stackhand::supervisor::RetainedChunk::Marker { .. })),
        "exec output must not enter Process output history: {snapshot:?}"
    );
}

#[cfg(unix)]
#[test]
fn exec_readiness_supports_direct_and_explicit_shell_commands() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("stackhand-exec-context-{unique}"));
    fs::create_dir_all(&dir).expect("exec working directory creates");
    fs::write(dir.join("ready.txt"), "ready").expect("exec marker writes");

    let direct = exec_service(
        "direct",
        ReadinessProbe::Exec {
            command: CommandForm::Direct {
                program: "/usr/bin/printf".into(),
                args: vec!["direct-ok".into()],
            },
            working_dir: None,
            env: Vec::new(),
            success_exit_codes: vec![0],
        },
        Duration::from_secs(1),
        dir.clone(),
        vec![("SHELL".into(), "/path/that/is/not/a/shell".into())],
    );
    let shell = exec_service(
        "shell",
        ReadinessProbe::Exec {
            command: CommandForm::Shell {
                text: "test \"$BASE\" = process && test \"$CHECK\" = probe && test -f ready.txt"
                    .to_string(),
            },
            working_dir: None,
            env: vec![("CHECK".into(), "probe".into())],
            success_exit_codes: vec![0],
        },
        Duration::from_secs(1),
        dir.clone(),
        vec![
            ("BASE".into(), "process".into()),
            ("SHELL".into(), "/path/that/is/not/a/shell".into()),
        ],
    );
    let accepted = exec_service(
        "accepted",
        ReadinessProbe::Exec {
            command: CommandForm::Direct {
                program: "/usr/bin/false".into(),
                args: Vec::new(),
            },
            working_dir: None,
            env: Vec::new(),
            success_exit_codes: vec![1],
        },
        Duration::from_secs(1),
        dir.clone(),
        Vec::new(),
    );
    let project = EffectiveProject::with_shell(
        vec![direct, shell, accepted],
        ShellConfig {
            program: "/bin/sh".into(),
            args: vec!["-c".into()],
        },
    )
    .expect("exec context project is valid");
    let (supervisor, _consoles, outputs) =
        stackhand::supervisor::start(project).expect("Supervisor starts");
    for name in ["direct", "shell", "accepted"] {
        supervisor.command(stackhand::supervisor::Command::Start(name.into()));
    }

    let direct = wait_for_process(&supervisor, "direct", |process| {
        process.lifecycle == stackhand::supervisor::Lifecycle::Running
    });
    let shell = wait_for_process(&supervisor, "shell", |process| {
        process.lifecycle == stackhand::supervisor::Lifecycle::Running
    });
    let accepted = wait_for_process(&supervisor, "accepted", |process| {
        process.lifecycle == stackhand::supervisor::Lifecycle::Running
    });
    for process in [&direct, &shell, &accepted] {
        assert_eq!(
            process
                .readiness
                .as_ref()
                .expect("readiness is visible")
                .kind,
            stackhand::supervisor::ReadinessCheckKind::Exec
        );
        assert_eq!(process.metrics, None);
        assert_process_output_has_no_data(&outputs, process.process_id);
    }

    for name in ["direct", "shell", "accepted"] {
        stop_process(&supervisor, name);
    }
    supervisor.stop_task();
    fs::remove_dir_all(dir).ok();
}

#[cfg(unix)]
#[test]
fn exec_readiness_reports_failure_timeout_and_bounded_noisy_output() {
    let dir = std::env::temp_dir();
    let failure = exec_service(
        "failure",
        ReadinessProbe::Exec {
            command: CommandForm::Direct {
                program: "/usr/bin/false".into(),
                args: Vec::new(),
            },
            working_dir: None,
            env: Vec::new(),
            success_exit_codes: vec![0],
        },
        Duration::from_secs(1),
        dir.clone(),
        Vec::new(),
    );
    let noisy = exec_service(
        "noisy",
        ReadinessProbe::Exec {
            command: CommandForm::Shell {
                text: "i=0; while [ \"$i\" -lt 20000 ]; do printf x; i=$((i+1)); done; exit 1"
                    .to_string(),
            },
            working_dir: None,
            env: Vec::new(),
            success_exit_codes: vec![0],
        },
        Duration::from_secs(1),
        dir.clone(),
        Vec::new(),
    );
    let timed = exec_service(
        "timed",
        ReadinessProbe::Exec {
            command: CommandForm::Shell {
                text: "sleep 30".to_string(),
            },
            working_dir: None,
            env: Vec::new(),
            success_exit_codes: vec![0],
        },
        Duration::from_millis(50),
        dir,
        Vec::new(),
    );
    let project =
        EffectiveProject::new(vec![failure, noisy, timed]).expect("exec project is valid");
    let (supervisor, _consoles, outputs) =
        stackhand::supervisor::start(project).expect("Supervisor starts");
    for name in ["failure", "noisy", "timed"] {
        supervisor.command(stackhand::supervisor::Command::Start(name.into()));
    }

    let failure = wait_for_process(&supervisor, "failure", |process| {
        process
            .readiness
            .as_ref()
            .and_then(|readiness| readiness.last_error.as_deref())
            .is_some_and(|error| error.contains("code 1"))
    });
    let noisy = wait_for_process(&supervisor, "noisy", |process| {
        process
            .readiness
            .as_ref()
            .and_then(|readiness| readiness.last_error.as_deref())
            .is_some_and(|error| error.contains("output truncated"))
    });
    let timed = wait_for_process(&supervisor, "timed", |process| {
        process
            .readiness
            .as_ref()
            .and_then(|readiness| readiness.last_error.as_deref())
            .is_some_and(|error| error.contains("timed out"))
    });
    for process in [&failure, &noisy, &timed] {
        assert_eq!(
            process.lifecycle,
            stackhand::supervisor::Lifecycle::Starting
        );
        assert_process_output_has_no_data(&outputs, process.process_id);
    }

    for name in ["failure", "noisy", "timed"] {
        stop_process(&supervisor, name);
    }
    supervisor.stop_task();
}

#[cfg(unix)]
fn wait_until_pid_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        // SAFETY: signal 0 only probes whether this test-owned PID remains.
        if unsafe { libc::kill(pid as libc::pid_t, 0) } < 0 {
            return;
        }
        assert!(Instant::now() < deadline, "PID {pid} was not cleaned up");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn exec_timeout_cleans_the_check_process_tree() {
    let process = exec_service(
        "timeout-tree",
        ReadinessProbe::Exec {
            command: CommandForm::Shell {
                text: "sleep 30 & child=$!; printf 'child-pid:%s' \"$child\"; wait \"$child\""
                    .to_string(),
            },
            working_dir: None,
            env: Vec::new(),
            success_exit_codes: vec![0],
        },
        Duration::from_millis(50),
        std::env::temp_dir(),
        Vec::new(),
    );
    let project = EffectiveProject::new(vec![process]).expect("timeout tree project is valid");
    let (supervisor, _consoles, _outputs) =
        stackhand::supervisor::start(project).expect("Supervisor starts");
    supervisor.command(stackhand::supervisor::Command::Start("timeout-tree".into()));

    let timeout = wait_for_process(&supervisor, "timeout-tree", |process| {
        process
            .readiness
            .as_ref()
            .and_then(|readiness| readiness.last_error.as_deref())
            .is_some_and(|error| error.contains("child-pid:") && error.contains("timed out"))
    });
    let error = timeout
        .readiness
        .as_ref()
        .and_then(|readiness| readiness.last_error.as_deref())
        .expect("timeout keeps its diagnostic");
    let child_pid = error
        .split("child-pid:")
        .nth(1)
        .and_then(|value| {
            value
                .split(|character: char| !character.is_ascii_digit())
                .next()
        })
        .and_then(|value| value.parse::<u32>().ok())
        .expect("timeout diagnostic contains the child PID");
    wait_until_pid_gone(child_pid);

    stop_process(&supervisor, "timeout-tree");
    supervisor.stop_task();
}

#[cfg(unix)]
#[test]
fn canceling_exec_readiness_cleans_the_check_process_tree() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let pid_file = std::env::temp_dir().join(format!("stackhand-exec-cancel-{unique}.pid"));
    let process = exec_service(
        "cancel-tree",
        ReadinessProbe::Exec {
            command: CommandForm::Shell {
                text:
                    "sleep 30 & child=$!; printf '%s' \"$child\" > \"$PID_FILE\"; wait \"$child\""
                        .to_string(),
            },
            working_dir: None,
            env: vec![("PID_FILE".into(), pid_file.to_string_lossy().into_owned())],
            success_exit_codes: vec![0],
        },
        Duration::from_secs(30),
        std::env::temp_dir(),
        Vec::new(),
    );
    let project = EffectiveProject::new(vec![process]).expect("cancel tree project is valid");
    let (supervisor, _consoles, _outputs) =
        stackhand::supervisor::start(project).expect("Supervisor starts");
    supervisor.command(stackhand::supervisor::Command::Start("cancel-tree".into()));

    let deadline = Instant::now() + Duration::from_secs(3);
    let child_pid = loop {
        if let Ok(text) = fs::read_to_string(&pid_file)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            break pid;
        }
        assert!(
            Instant::now() < deadline,
            "exec check did not write its PID"
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    stop_process(&supervisor, "cancel-tree");
    wait_until_pid_gone(child_pid);
    supervisor.stop_task();
    fs::remove_file(pid_file).ok();
}

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
        restart: RestartConfig::default(),
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
            checks: vec![ReadinessCheck {
                probe: ReadinessProbe::Tcp {
                    host: "127.0.0.1".into(),
                    port: failed_port,
                },
                initial_delay: Duration::ZERO,
                interval: Duration::from_secs(1),
                timeout: Duration::from_millis(50),
                success_threshold: 1,
                failure_threshold: 1,
            }],
            startup_timeout: Some(Duration::from_millis(200)),
        }),
        liveness: None,
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
