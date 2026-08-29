use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod support;

use support::{OwnedListener, yaml_quote};

fn unique_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("stackhand-fixture-{label}-{nanos}"));
    fs::create_dir_all(&dir).expect("fixture directory creates");
    dir
}

/// Mutable endpoint states controlled by checkpoint lines from the executable
/// fixture. The supervised Project sees only real HTTP responses.
struct HttpStates {
    initialization_ready: AtomicBool,
    recovering_ready: AtomicBool,
    liveness_recover: AtomicBool,
    liveness_restart: AtomicBool,
}

impl HttpStates {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            initialization_ready: AtomicBool::new(false),
            recovering_ready: AtomicBool::new(true),
            liveness_recover: AtomicBool::new(true),
            liveness_restart: AtomicBool::new(true),
        })
    }

    fn healthy(&self, path: &str) -> bool {
        match path {
            "/initialization-ready" => self.initialization_ready.load(Ordering::Acquire),
            "/recover-ready" => self.recovering_ready.load(Ordering::Acquire),
            "/liveness-recover" => self.liveness_recover.load(Ordering::Acquire),
            "/liveness-restart" => self.liveness_restart.load(Ordering::Acquire),
            _ => true,
        }
    }

    fn apply_checkpoint(&self, checkpoint: &str) {
        match checkpoint {
            "fixture-initialization-ready" => {
                self.initialization_ready.store(true, Ordering::Release)
            }
            "fixture-readiness-ready" => self.recovering_ready.store(false, Ordering::Release),
            "fixture-readiness-failing" => self.recovering_ready.store(true, Ordering::Release),
            "fixture-liveness-ready" => self.liveness_recover.store(false, Ordering::Release),
            "fixture-liveness-failing" => self.liveness_recover.store(true, Ordering::Release),
            "fixture-unhealthy-restart-ready" => {
                self.liveness_restart.store(false, Ordering::Release)
            }
            "fixture-unhealthy-restart-backoff" => {
                self.liveness_restart.store(true, Ordering::Release)
            }
            _ => {}
        }
    }
}

const STARTED_SOURCE: &str = "printf 'fixture-started-source\\n'; exec sleep 60";
const STARTED_DEPENDENT: &str = "printf 'fixture-started-dependent\\n'; exec sleep 60";
const HELLO: &str = "printf 'fixture-marker\\n'; printf 'fixture-token-%s\\n' \"$FIXTURE_TOKEN\"; printf 'fixture-env-value-%s\\n' \"$FIXTURE_VALUE\"; printf 'fixture-project-only-%s\\n' \"$PROJECT_ONLY\"; printf 'fixture-process-only-%s\\n' \"$PROCESS_ONLY\"; printf 'fixture-local-precedence-%s\\n' \"$LOCAL_PRECEDENCE\"; printf 'fixture-process-precedence-%s\\n' \"$PROCESS_PRECEDENCE\"; printf 'fixture-profile-value-%s\\n' \"$PROFILE_VALUE\"; printf 'fixture-local-value-%s\\n' \"$LOCAL_VALUE\"; exec sleep 60";
const SHELLED: &str = "echo fixture-pipeline-lower | tr a-z A-Z; exec sleep 60";
const PIPED: &str = "sleep 60 & child=$!; printf 'fixture-descendant-pid-%s\\n' \"$child\"; printf 'fixture-pipe-out\\n'; printf 'fixture-pipe-err\\n' 1>&2; wait \"$child\"";
const NOISY: &str = "i=0; while [ \"$i\" -lt 2000 ]; do printf 'fixture-noisy-%s\\n' \"$i\"; i=$((i+1)); done; exec sleep 60";
const ACCEPTED: &str = "printf 'fixture-accepted\\n'; exit 42";
const EXITED: &str = "printf 'fixture-exited\\n'; exit 7";
const RERUN: &str = "if [ -e \"$RERUN_MARKER\" ]; then printf 'fixture-rerun-success\\n'; exit 0; else : > \"$RERUN_MARKER\"; printf 'fixture-rerun-failure\\n'; exit 7; fi";
const BUDGET: &str = "printf 'fixture-budget-run\\n'; exit 7";
const SHUTDOWN_RESTART: &str = "printf 'fixture-shutdown-restart-run\\n'; exit 7";
const TIMEOUT: &str = "sleep 60 & child=$!; printf 'fixture-timeout-descendant-pid-%s\\n' \"$child\"; wait \"$child\"";
const LOG_READY: &str = "printf 'fixture-log-ready\\n'; exec sleep 60";
const DIRECT: &str = "fixture-direct-command";

fn fixture_config(
    tcp_port: u16,
    http_port: u16,
    never_ready_port: u16,
    exec_marker: &Path,
    rerun_marker: &Path,
) -> String {
    let exec_marker = yaml_quote(&exec_marker.to_string_lossy());
    let exec_check = yaml_quote(&format!("test -f {exec_marker}"));
    let rerun_marker = yaml_quote(&rerun_marker.to_string_lossy());
    format!(
        r#"version: 1
env_files:
  - project.env
  - project.local.env
processes:
  started-dependent:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    depends_on:
      started-source: started
    shell: {started_dependent}
  started-source:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    command: [/bin/sh, "-c", {started_source}]
  hello:
    kind: service
    terminal:
      mode: pty
      input: focused
    cwd: ./web
    env_files:
      - hello.env
    depends_on:
      all-ready: ready
    environment:
      FIXTURE_TOKEN: stackhand-env-ok
      FIXTURE_VALUE: inline-value
    command: [/bin/sh, "-c", {hello}]
  shelled:
    kind: service
    terminal:
      mode: pty
      input: focused
    shell: {shelled}
  piped:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    command: [/bin/sh, "-c", {piped}]
  noisy:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    shell: {noisy}
  manual:
    kind: service
    autostart: false
    command: [/bin/sleep, "60"]
  off:
    kind: service
    enabled: false
    command: [/usr/bin/true]
  optional:
    kind: one-shot
    enabled: false
    terminal:
      mode: pipe
      input: disabled
    command: [/bin/sh, "-c", "printf fixture-optional-enabled; exit 0"]
  direct:
    kind: one-shot
    terminal:
      mode: pipe
      input: disabled
    command: [/bin/echo, {direct}]
  accepted:
    kind: one-shot
    terminal:
      mode: pipe
      input: disabled
    success_exit_codes: [42]
    command: [/bin/sh, "-c", {accepted}]
  completed-dependent:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    depends_on:
      accepted: completed_successfully
    command: [/bin/sleep, "60"]
  exited-dependent:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    depends_on:
      exited-source: exited
    command: [/bin/sleep, "60"]
  exited-source:
    kind: one-shot
    terminal:
      mode: pipe
      input: disabled
    shell: {exited}
  rerun-dependent:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    depends_on:
      rerun-setup: completed_successfully
    command: [/bin/sleep, "60"]
  rerun-setup:
    kind: one-shot
    terminal:
      mode: pipe
      input: disabled
    environment:
      RERUN_MARKER: {rerun_marker}
    shell: {rerun}
  tcp-ready:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    ready:
      tcp:
        host: 127.0.0.1
        port: {tcp_port}
      interval: 20ms
      timeout: 250ms
    command: [/bin/sleep, "60"]
  http-ready:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    ready:
      http:
        url: "http://127.0.0.1:{http_port}/http-ready"
      interval: 20ms
      timeout: 250ms
    command: [/bin/sleep, "60"]
  exec-ready:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    ready:
      exec:
        command: [/bin/sh, "-c", {exec_check}]
      interval: 20ms
      timeout: 500ms
    command: [/bin/sleep, "60"]
  log-ready:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    ready:
      log:
        contains: fixture-log-ready
      interval: 20ms
      timeout: 500ms
    shell: {log_ready}
  all-ready:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    ready:
      all:
        - tcp:
            host: 127.0.0.1
            port: {tcp_port}
          interval: 20ms
          timeout: 250ms
        - http:
            url: "http://127.0.0.1:{http_port}/all-ready"
          interval: 20ms
          timeout: 250ms
    command: [/bin/sleep, "60"]
  ready-dependent:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    depends_on:
      recovering: ready
    command: [/bin/sleep, "60"]
  recovering:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    ready:
      http:
        url: "http://127.0.0.1:{http_port}/recover-ready"
      interval: 20ms
      timeout: 250ms
      startup_timeout: 5s
    command: [/bin/sleep, "60"]
  liveness-recover:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    liveness:
      http:
        url: "http://127.0.0.1:{http_port}/liveness-recover"
      interval: 20ms
      timeout: 250ms
    command: [/bin/sleep, "60"]
  liveness-restart:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    restart:
      policy: on_failure
      backoff: 50ms
      max_restarts: 1
      on_unhealthy: true
    liveness:
      http:
        url: "http://127.0.0.1:{http_port}/liveness-restart"
      interval: 20ms
      timeout: 250ms
    command: [/bin/sleep, "60"]
  budget:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    restart:
      policy: on_failure
      backoff: 25ms
      max_restarts: 2
    shell: {budget}
  shutdown-restart:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    restart:
      policy: on_failure
      backoff: 60s
      max_restarts: 2
    shell: {shutdown_restart}
  startup-timeout:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    ready:
      http:
        url: "http://127.0.0.1:{never_ready_port}/never-ready"
      interval: 20ms
      timeout: 100ms
      startup_timeout: 500ms
    shell: {timeout}
  initialization-gate:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    ready:
      http:
        url: "http://127.0.0.1:{http_port}/initialization-ready"
      interval: 20ms
      timeout: 100ms
    command: [/bin/sleep, "60"]
  initialization:
    kind: one-shot
    terminal:
      mode: pipe
      input: disabled
    depends_on:
      initialization-gate: ready
    command: [/bin/sh, "-c", "printf fixture-initialization; exit 0"]
  initialized-dependent:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    depends_on:
      initialization: completed_successfully
    command: [/bin/sleep, "60"]
profiles:
  profile-fixture:
    overrides:
      hello:
        environment:
          PROFILE_VALUE: first-profile
  profile-final:
    enable:
      - optional
    overrides:
      hello:
        environment:
          PROFILE_VALUE: second-profile
"#,
        started_dependent = yaml_quote(STARTED_DEPENDENT),
        started_source = yaml_quote(STARTED_SOURCE),
        hello = yaml_quote(HELLO),
        shelled = yaml_quote(SHELLED),
        piped = yaml_quote(PIPED),
        noisy = yaml_quote(NOISY),
        accepted = yaml_quote(ACCEPTED),
        exited = yaml_quote(EXITED),
        rerun = yaml_quote(RERUN),
        log_ready = yaml_quote(LOG_READY),
        budget = yaml_quote(BUDGET),
        shutdown_restart = yaml_quote(SHUTDOWN_RESTART),
        timeout = yaml_quote(TIMEOUT),
        direct = yaml_quote(DIRECT),
    )
}

fn run_invalid_project(label: &str, config: &str) -> std::process::Output {
    let dir = unique_dir(label);
    let config_path = dir.join("stackhand.yaml");
    fs::write(&config_path, config).expect("config writes");

    let output = StdCommand::new(env!("CARGO_BIN_EXE_stackhand"))
        .arg("--fixture-project")
        .arg(&config_path)
        .output()
        .expect("the fixture binary runs");
    assert!(
        !output.status.success(),
        "invalid Project unexpectedly succeeded"
    );

    fs::remove_dir_all(&dir).ok();
    output
}

#[test]
fn discovered_layered_project_runs_the_complete_project_path() {
    let dir = unique_dir("milestone-three-layered");
    let nested = dir.join("web");
    fs::create_dir_all(&nested).expect("working directory creates");
    let exec_marker = dir.join("exec-ready.marker");
    fs::write(&exec_marker, "ready").expect("exec marker writes");
    let rerun_marker = dir.join("rerun.marker");
    let tcp = OwnedListener::new(drop);
    let never_ready = OwnedListener::new(|mut stream| {
        let _ = stream.write_all(b"HTTP/1.0 503 Unavailable\r\nContent-Length: 3\r\n\r\nbad");
    });
    let states = HttpStates::new();
    let http_states = Arc::clone(&states);
    let http = OwnedListener::new(move |mut stream| {
        let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
        let mut request = [0; 512];
        let bytes = stream.read(&mut request).unwrap_or(0);
        let request_text = String::from_utf8_lossy(&request[..bytes]);
        let path = request_text.split_whitespace().nth(1).unwrap_or("/");
        let response = if http_states.healthy(path) {
            b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok".as_slice()
        } else {
            b"HTTP/1.0 503 Unavailable\r\nContent-Length: 3\r\n\r\nbad".as_slice()
        };
        let _ = stream.write_all(response);
    });
    fs::write(
        dir.join("project.env"),
        "FIXTURE_VALUE=project-file\nPROJECT_ONLY=project-file\nLOCAL_PRECEDENCE=project-file\n",
    )
    .expect("project environment file writes");
    fs::write(
        dir.join("project.local.env"),
        "FIXTURE_VALUE=project-local-file\nLOCAL_PRECEDENCE=local-file\nPROCESS_PRECEDENCE=local-file\n",
    )
    .expect("later project environment file writes");
    fs::write(
        dir.join("hello.env"),
        "FIXTURE_VALUE=process-file\nPROCESS_ONLY=process-file\nPROCESS_PRECEDENCE=process-file\n",
    )
    .expect("Process environment file writes");
    let config_path = dir.join("stackhand.yaml");
    fs::write(
        &config_path,
        fixture_config(
            tcp.port(),
            http.port(),
            never_ready.port(),
            &exec_marker,
            &rerun_marker,
        ),
    )
    .expect("config writes");
    fs::write(
        dir.join("stackhand.local.yaml"),
        "processes:\n  hello:\n    environment:\n      LOCAL_VALUE: local-override\n",
    )
    .expect("local override writes");

    let stdout = support::run_discovered_fixture_with_profiles(
        "--fixture-project",
        &nested,
        &["profile-fixture", "profile-final"],
        |line| states.apply_checkpoint(line),
    );
    for checkpoint in [
        "fixture-blocked-ok",
        "fixture-initialization-ok",
        "fixture-started-ok",
        "fixture-output-ok",
        "fixture-pipe-output-ok",
        "fixture-startup-timeout-ok",
        "fixture-readiness-recovered",
        "fixture-liveness-recovered",
        "fixture-unhealthy-restart-recovered",
        "fixture-restart-budget-ok",
        "fixture-rerun-recovered",
        "fixture-shutdown-ok",
    ] {
        assert!(
            stdout.contains(checkpoint),
            "{checkpoint} missing: {stdout}"
        );
    }

    fs::remove_dir_all(&dir).ok();
}

/// Broken YAML must fail before any Process starts.
#[test]
fn an_unknown_field_starts_nothing_and_fails_clearly() {
    let output = run_invalid_project(
        "unknown-field",
        "version: 1\nprocesses:\n  hello:\n    comand: true\n",
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown field"),
        "diagnostic was not clear: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("fixture-started-ok"), "{stdout}");
}

#[test]
fn a_temporary_process_collection_starts_nothing_and_names_the_replacement() {
    let output = run_invalid_project(
        "temporary-process-list",
        "version: 1\nprocesses:\n  - name: hello\n    command: [/usr/bin/true]\n",
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("processes must be a name-keyed mapping")
            && stderr.contains("use 'processes: {name: {...}}'"),
        "diagnostic was not clear: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("fixture-started-ok"), "{stdout}");
}

#[test]
fn duplicate_process_names_start_nothing_and_fail_clearly() {
    let output = run_invalid_project(
        "duplicate-name",
        "version: 1\nprocesses:\n  hello:\n    command: [/usr/bin/true]\n  hello:\n    command: [/usr/bin/true]\n",
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("duplicate Process name 'hello'"),
        "diagnostic was not clear: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("fixture-started-ok"), "{stdout}");
}

#[test]
fn an_invalid_project_starts_nothing_and_fails_clearly() {
    let output = run_invalid_project(
        "invalid",
        "version: 2\nprocesses:\n  hello:\n    command: [/usr/bin/true]\n",
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unsupported schema version 2"),
        "diagnostic was not clear: {stderr}"
    );
}

/// An invalid profile layer must fail before the resolved Project reaches the Supervisor.
#[test]
fn an_invalid_layered_project_starts_nothing() {
    let dir = unique_dir("invalid-layered");
    let marker = dir.join("started.marker");
    let command = yaml_quote(&format!(
        "printf fixture-invalid-layered-started > {}",
        marker.display()
    ));
    let config = format!(
        r#"version: 1
processes:
  marker:
    kind: service
    terminal:
      mode: pipe
      input: disabled
    command: [/bin/sh, "-c", {command}]
profiles:
  broken:
    overrides:
      marker:
        terminal:
          mode: invalid
"#
    );
    fs::write(dir.join("stackhand.yaml"), config).expect("config writes");

    let output = StdCommand::new(env!("CARGO_BIN_EXE_stackhand"))
        .env("SHELL", "/path/that/does/not/exist")
        .current_dir(&dir)
        .args(["--fixture-project", "--profile", "broken"])
        .output()
        .expect("the fixture binary runs");
    assert!(
        !output.status.success(),
        "invalid layered Project succeeded"
    );
    assert!(
        !marker.exists(),
        "invalid layered Project started a Process"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("broken") && stderr.contains("terminal"),
        "layered diagnostic was not clear: {stderr}"
    );

    fs::remove_dir_all(&dir).ok();
}
