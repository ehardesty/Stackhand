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
    recovering_ready: AtomicBool,
    liveness_recover: AtomicBool,
    liveness_restart: AtomicBool,
}

impl HttpStates {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            recovering_ready: AtomicBool::new(true),
            liveness_recover: AtomicBool::new(true),
            liveness_restart: AtomicBool::new(true),
        })
    }

    fn healthy(&self, path: &str) -> bool {
        match path {
            "/recover-ready" => self.recovering_ready.load(Ordering::Acquire),
            "/liveness-recover" => self.liveness_recover.load(Ordering::Acquire),
            "/liveness-restart" => self.liveness_restart.load(Ordering::Acquire),
            "/never-ready" => false,
            _ => true,
        }
    }

    fn apply_checkpoint(&self, checkpoint: &str) {
        match checkpoint {
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
const HELLO: &str =
    "printf 'fixture-marker\\n'; printf 'fixture-token-%s\\n' \"$FIXTURE_TOKEN\"; exec sleep 60";
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

fn fixture_config(
    tcp_port: u16,
    http_port: u16,
    exec_marker: &Path,
    rerun_marker: &Path,
) -> String {
    let exec_marker = yaml_quote(&exec_marker.to_string_lossy());
    let exec_check = yaml_quote(&format!("test -f {exec_marker}"));
    let rerun_marker = yaml_quote(&rerun_marker.to_string_lossy());
    format!(
        r#"version: 1
processes:
  - name: started-dependent
    kind: service
    terminal: pipe
    depends_on:
      - name: started-source
        condition: started
    command:
      shell: {started_dependent}
  - name: started-source
    kind: service
    terminal: pipe
    command:
      program: /bin/sh
      args:
        - "-c"
        - {started_source}
  - name: hello
    kind: service
    terminal: pty
    input: focused
    working_dir: ./web
    depends_on:
      - name: all-ready
        condition: ready
    env:
      FIXTURE_TOKEN: stackhand-env-ok
    command:
      program: /bin/sh
      args:
        - "-c"
        - {hello}
  - name: shelled
    kind: service
    terminal: pty
    input: focused
    command:
      shell: {shelled}
  - name: piped
    kind: service
    terminal: pipe
    command:
      program: /bin/sh
      args:
        - "-c"
        - {piped}
  - name: noisy
    kind: service
    terminal: pipe
    command:
      shell: {noisy}
  - name: manual
    kind: service
    autostart: false
    command:
      program: /bin/sleep
      args: ["60"]
  - name: off
    kind: service
    enabled: false
    command:
      program: /bin/true
  - name: accepted
    kind: one-shot
    terminal: pipe
    success_exit_codes: [42]
    command:
      program: /bin/sh
      args:
        - "-c"
        - {accepted}
  - name: completed-dependent
    kind: service
    terminal: pipe
    depends_on:
      - name: accepted
        condition: completed_successfully
    command:
      program: /bin/sleep
      args: ["60"]
  - name: exited-dependent
    kind: service
    terminal: pipe
    depends_on:
      - name: exited-source
        condition: exited
    command:
      program: /bin/sleep
      args: ["60"]
  - name: exited-source
    kind: one-shot
    terminal: pipe
    command:
      shell: {exited}
  - name: rerun-dependent
    kind: service
    terminal: pipe
    depends_on:
      - name: rerun-setup
        condition: completed_successfully
    command:
      program: /bin/sleep
      args: ["60"]
  - name: rerun-setup
    kind: one-shot
    terminal: pipe
    env:
      RERUN_MARKER: {rerun_marker}
    command:
      shell: {rerun}
  - name: tcp-ready
    kind: service
    terminal: pipe
    ready:
      tcp:
        host: 127.0.0.1
        port: {tcp_port}
      interval: 20ms
      timeout: 250ms
    command:
      program: /bin/sleep
      args: ["60"]
  - name: http-ready
    kind: service
    terminal: pipe
    ready:
      http:
        url: "http://127.0.0.1:{http_port}/http-ready"
      interval: 20ms
      timeout: 250ms
    command:
      program: /bin/sleep
      args: ["60"]
  - name: exec-ready
    kind: service
    terminal: pipe
    ready:
      exec:
        command:
          program: /bin/sh
          args:
            - "-c"
            - {exec_check}
      interval: 20ms
      timeout: 500ms
    command:
      program: /bin/sleep
      args: ["60"]
  - name: log-ready
    kind: service
    terminal: pipe
    ready:
      log:
        contains: fixture-log-ready
      interval: 20ms
      timeout: 500ms
    command:
      shell: {log_ready}
  - name: all-ready
    kind: service
    terminal: pipe
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
    command:
      program: /bin/sleep
      args: ["60"]
  - name: ready-dependent
    kind: service
    terminal: pipe
    depends_on:
      - name: recovering
        condition: ready
    command:
      program: /bin/sleep
      args: ["60"]
  - name: recovering
    kind: service
    terminal: pipe
    ready:
      http:
        url: "http://127.0.0.1:{http_port}/recover-ready"
      interval: 20ms
      timeout: 250ms
      startup_timeout: 5s
    command:
      program: /bin/sleep
      args: ["60"]
  - name: liveness-recover
    kind: service
    terminal: pipe
    liveness:
      http:
        url: "http://127.0.0.1:{http_port}/liveness-recover"
      interval: 20ms
      timeout: 250ms
    command:
      program: /bin/sleep
      args: ["60"]
  - name: liveness-restart
    kind: service
    terminal: pipe
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
    command:
      program: /bin/sleep
      args: ["60"]
  - name: budget
    kind: service
    terminal: pipe
    restart:
      policy: on_failure
      backoff: 25ms
      max_restarts: 2
    command:
      shell: {budget}
  - name: shutdown-restart
    kind: service
    terminal: pipe
    restart:
      policy: on_failure
      backoff: 60s
      max_restarts: 2
    command:
      shell: {shutdown_restart}
  - name: startup-timeout
    kind: service
    terminal: pipe
    ready:
      http:
        url: "http://127.0.0.1:{http_port}/never-ready"
      interval: 20ms
      timeout: 100ms
      startup_timeout: 500ms
    command:
      shell: {timeout}
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
fn one_configured_project_runs_the_complete_milestone_two_path() {
    let dir = unique_dir("milestone-two");
    let nested = dir.join("web");
    fs::create_dir_all(&nested).expect("working directory creates");
    let exec_marker = dir.join("exec-ready.marker");
    fs::write(&exec_marker, "ready").expect("exec marker writes");
    let rerun_marker = dir.join("rerun.marker");
    let tcp = OwnedListener::new(drop);
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
    let config_path = dir.join("stackhand.yaml");
    fs::write(
        &config_path,
        fixture_config(tcp.port(), http.port(), &exec_marker, &rerun_marker),
    )
    .expect("config writes");

    let stdout = support::run_fixture("--fixture-project", &config_path, |line| {
        states.apply_checkpoint(line)
    });
    for checkpoint in [
        "fixture-blocked-ok",
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
        "version: 1\nprocesses:\n  - name: hello\n    comand:\n      program: /bin/true\n",
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
fn duplicate_process_names_start_nothing_and_fail_clearly() {
    let output = run_invalid_project(
        "duplicate-name",
        "version: 1\nprocesses:\n  - name: hello\n    command: {program: /bin/true}\n  - name: hello\n    command: {program: /bin/true}\n",
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
        "version: 2\nprocesses:\n  - name: hello\n    command:\n      program: /bin/true\n",
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unsupported schema version 2"),
        "diagnostic was not clear: {stderr}"
    );
}
