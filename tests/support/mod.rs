use std::io::BufRead;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(5);
const FIXTURE_WAIT: Duration = Duration::from_secs(120);

#[allow(dead_code)]
enum FixtureSource<'a> {
    Explicit(&'a Path),
    Discovered(&'a Path),
}

/// Owns a loopback listener worker and joins it when the test drops the listener.
pub struct OwnedListener {
    port: u16,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl OwnedListener {
    pub fn new(on_connection: impl Fn(TcpStream) + Send + 'static) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("endpoint binds");
        listener
            .set_nonblocking(true)
            .expect("endpoint accepts nonblocking mode");
        let port = listener
            .local_addr()
            .expect("endpoint address is available")
            .port();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => on_connection(stream),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if worker_stop.load(Ordering::Acquire) {
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
            stop,
            worker: Some(worker),
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for OwnedListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[allow(dead_code)]
pub fn run_fixture(
    fixture_flag: &str,
    config_path: &Path,
    on_checkpoint: impl FnMut(&str),
) -> String {
    run_fixture_command(
        fixture_flag,
        FixtureSource::Explicit(config_path),
        None,
        on_checkpoint,
    )
}

#[allow(dead_code)]
pub fn run_fixture_with_profile(
    fixture_flag: &str,
    config_path: &Path,
    profile: Option<&str>,
    on_checkpoint: impl FnMut(&str),
) -> String {
    run_fixture_command(
        fixture_flag,
        FixtureSource::Explicit(config_path),
        profile,
        on_checkpoint,
    )
}

#[allow(dead_code)]
pub fn run_discovered_fixture_with_profile(
    fixture_flag: &str,
    start_directory: &Path,
    profile: Option<&str>,
    on_checkpoint: impl FnMut(&str),
) -> String {
    run_fixture_command(
        fixture_flag,
        FixtureSource::Discovered(start_directory),
        profile,
        on_checkpoint,
    )
}

fn run_fixture_command(
    fixture_flag: &str,
    source: FixtureSource<'_>,
    profile: Option<&str>,
    mut on_checkpoint: impl FnMut(&str),
) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_stackhand"));
    command
        .env("SHELL", "/path/that/does/not/exist")
        .arg(fixture_flag);
    match source {
        FixtureSource::Explicit(config_path) => {
            command.arg(config_path);
        }
        FixtureSource::Discovered(start_directory) => {
            command.current_dir(start_directory);
        }
    }
    if let Some(profile) = profile {
        command.arg("--profile").arg(profile);
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("fixture process runs");
    let stdout = child.stdout.take().expect("fixture stdout is piped");
    let (line_tx, line_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + FIXTURE_WAIT;
    let mut output = String::new();
    let timed_out = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break true;
        }
        match line_rx.recv_timeout(remaining) {
            Ok(line) => {
                on_checkpoint(&line);
                output.push_str(&line);
                output.push('\n');
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break false,
            Err(mpsc::RecvTimeoutError::Timeout) => break true,
        }
    };
    if timed_out {
        child.kill().ok();
    }
    let result = child.wait_with_output().expect("fixture process exits");
    let _ = reader.join();
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        !timed_out && result.status.success(),
        "fixture failed: timed_out={timed_out} status={}\nstdout:\n{output}\nstderr:\n{stderr}",
        result.status
    );
    output
}

pub fn yaml_quote(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            _ => escaped.push(character),
        }
    }
    format!("\"{escaped}\"")
}
