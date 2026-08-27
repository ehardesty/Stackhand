use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command as StdCommand;
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("stackhand-fixture-{label}-{nanos}"));
    fs::create_dir_all(&dir).expect("fixture directory creates");
    dir
}

/// Host one real TCP listener for the fixture's readiness probe. The
/// listener lives in this test process; Stackhand's production TCP probe
/// adapter connects to it over a real loopback socket. Each connection is
/// accepted and closed; nothing is served.
fn host_tcp_listener() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener binds");
    let port = listener.local_addr().expect("local address").port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            drop(stream);
        }
    });
    port
}

/// Host one real HTTP health endpoint for the fixture's HTTP readiness
/// probe. The hand-rolled response proves the production adapter speaks a
/// plain HTTP/1.0 GET against a real loopback socket.
fn host_http_health_endpoint() -> u16 {
    use std::io::Write;
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener binds");
    let port = listener.local_addr().expect("local address").port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok");
        }
    });
    port
}

/// Prints the direct-command marker, then proves inline environment reached
/// the child through `$FIXTURE_TOKEN`, then stays alive as a Service.
const MARKER_SCRIPT: &str = "printf 'fixture-marker-12345\\n'; printf 'fixture-token-%s\\n' \"$FIXTURE_TOKEN\"; exec sleep 60";

/// One pipeline whose output only exists because both stages ran; then the
/// Run stays alive as a Service.
const SHELL_SCRIPT: &str = "echo fixture-pipeline-lower | tr a-z A-Z; exec sleep 60";

/// A pipe-mode Service that proves stdout and stderr keep their identity in
/// the retained output module, then stays alive.
const PIPED_SCRIPT: &str = "sleep 60 & child=$!; printf 'fixture-descendant-pid-%s\\n' \"$child\"; printf 'fixture-pipe-out\\n'; printf 'fixture-pipe-err\\n' 1>&2; wait \"$child\"";

fn fixture_config(tcp_port: u16, http_port: u16) -> String {
    let marker = MARKER_SCRIPT.replace('"', "\\\"");
    let piped = PIPED_SCRIPT.replace('"', "\\\"");
    format!(
        "version: 1\n\
         processes:\n\
         \x20 - name: hello\n\
         \x20   kind: service\n\
         \x20   terminal: pty\n\
         \x20   input: focused\n\
         \x20   working_dir: ./web\n\
         \x20   env:\n\
         \x20     FIXTURE_TOKEN: stackhand-env-ok\n\
         \x20   command:\n\
         \x20     program: /bin/sh\n\
         \x20     args: [\"-c\", \"{marker}\"]\n\
         \x20 - name: shelled\n\
         \x20   kind: service\n\
         \x20   terminal: pty\n\
         \x20   input: focused\n\
         \x20   command:\n\
         \x20     shell: {SHELL_SCRIPT}\n\
         \x20 - name: piped\n\
         \x20   kind: service\n\
         \x20   terminal: pipe\n\
         \x20   command:\n\
         \x20     program: /bin/sh\n\
         \x20     args: [\"-c\", \"{piped}\"]\n\
         \x20 - name: manual\n\
         \x20   kind: service\n\
         \x20   autostart: false\n\
         \x20   command:\n\
         \x20     program: /bin/sleep\n\
         \x20     args: [\"60\"]\n\
         \x20 - name: off\n\
         \x20   enabled: false\n\
         \x20   command:\n\
         \x20     program: /bin/true\n\
         \x20 - name: setup\n\
         \x20   kind: one-shot\n\
         \x20   terminal: pipe\n\
         \x20   command:\n\
         \x20     program: /usr/bin/true\n\
         \x20 - name: gated\n\
         \x20   depends_on: [{{name: setup, condition: completed_successfully}}]\n\
         \x20   terminal: pipe\n\
         \x20   command:\n\
         \x20     program: /bin/sleep\n\
         \x20     args: [\"60\"]\n\
         \x20 - name: tcp-ready\n\
         \x20   kind: service\n\
         \x20   readiness:\n\
         \x20     tcp:\n\
         \x20       host: 127.0.0.1\n\
         \x20       port: {tcp_port}\n\
         \x20   terminal: pipe\n\
         \x20   command:\n\
         \x20     program: /bin/sleep\n\
         \x20     args: [\"60\"]\n\
         \x20 - name: http-ready\n\
         \x20   kind: service\n\
         \x20   readiness:\n\
         \x20     http:\n\
         \x20       url: \"http://127.0.0.1:{http_port}/healthz\"\n\
         \x20   terminal: pipe\n\
         \x20   command:\n\
         \x20     program: /bin/sleep\n\
         \x20     args: [\"60\"]\n",
    )
}

#[test]
fn one_configured_service_runs_end_to_end() {
    let dir = unique_dir("project");
    let nested = dir.join("web");
    fs::create_dir_all(&nested).expect("working directory creates");
    // The readiness target is a real socket in this process; the probed
    // supervised Process itself only sleeps.
    let tcp_port = host_tcp_listener();
    let http_port = host_http_health_endpoint();
    let config_path = dir.join("stackhand.yaml");
    fs::write(&config_path, fixture_config(tcp_port, http_port)).expect("config writes");

    let output = StdCommand::new(env!("CARGO_BIN_EXE_stackhand"))
        .arg("--fixture-project")
        .arg(&config_path)
        .output()
        .expect("the fixture binary runs");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "fixture failed: {stdout} {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("fixture-started-ok"), "{stdout}");
    // The fixture prints this checkpoint only after the direct command's
    // marker, the inline-environment token, and the shell pipeline's
    // transformed output all reached their consoles.
    assert!(stdout.contains("fixture-output-ok"), "{stdout}");
    // Pipe output stayed out of the control plane and landed in the
    // bounded per-Process module with stream identity and the Run marker.
    assert!(stdout.contains("fixture-pipe-output-ok"), "{stdout}");
    // The One-shot completed through natural exit observation and its
    // dependent started only after `completed_successfully` held.
    assert!(stdout.contains("fixture-one-shot-ok"), "{stdout}");
    assert!(stdout.contains("fixture-shutdown-ok"), "{stdout}");

    fs::remove_dir_all(&dir).ok();
}

/// Broken YAML must fail before any Process starts: the fixture prints its
/// startup checkpoint only after every Process reaches its active lifecycle,
/// so an absent checkpoint plus a failed exit means zero Processes ran.
#[test]
fn an_unknown_field_starts_nothing_and_fails_clearly() {
    let dir = unique_dir("unknown-field");
    let config_path = dir.join("stackhand.yaml");
    fs::write(
        &config_path,
        "version: 1\nprocesses:\n  - name: hello\n    comand:\n      program: /bin/true\n",
    )
    .expect("config writes");

    let output = StdCommand::new(env!("CARGO_BIN_EXE_stackhand"))
        .arg("--fixture-project")
        .arg(&config_path)
        .output()
        .expect("the fixture binary runs");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown field"),
        "diagnostic was not clear: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("fixture-started-ok"), "{stdout}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn duplicate_process_names_start_nothing_and_fail_clearly() {
    let dir = unique_dir("duplicate-name");
    let config_path = dir.join("stackhand.yaml");
    fs::write(
        &config_path,
        "version: 1\nprocesses:\n  - name: hello\n    command: {program: /bin/true}\n  - name: hello\n    command: {program: /bin/true}\n",
    )
    .expect("config writes");

    let output = StdCommand::new(env!("CARGO_BIN_EXE_stackhand"))
        .arg("--fixture-project")
        .arg(&config_path)
        .output()
        .expect("the fixture binary runs");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("duplicate Process name 'hello'"),
        "diagnostic was not clear: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("fixture-started-ok"), "{stdout}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_invalid_project_starts_nothing_and_fails_clearly() {
    let dir = unique_dir("invalid");
    let config_path = dir.join("stackhand.yaml");
    fs::write(
        &config_path,
        "version: 2\nprocesses:\n  - name: hello\n    command:\n      program: /bin/true\n",
    )
    .expect("config writes");

    let output = StdCommand::new(env!("CARGO_BIN_EXE_stackhand"))
        .arg("--fixture-project")
        .arg(&config_path)
        .output()
        .expect("the fixture binary runs");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unsupported schema version 2"),
        "diagnostic was not clear: {stderr}"
    );

    fs::remove_dir_all(&dir).ok();
}
