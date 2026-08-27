use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The whole fixture binary must finish this long. Every internal wait is
/// already bounded; this caps the test so a binary deadlock cannot hang
/// the suite.
const BINARY_WAIT: Duration = Duration::from_secs(90);

fn unique_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("stackhand-fixture-{label}-{nanos}"));
    fs::create_dir_all(&dir).expect("fixture directory creates");
    dir
}

/// A focused-input PTY Service: a small Perl reader accumulates received
/// input one byte at a time and prints each CR/NL-terminated line as hex;
/// a background tick loop keeps the console output climbing. The reader
/// lives in Perl rather than a `read -n 1` loop because macOS /bin/sh
/// (bash 3.2) loses queued input after a trapped signal interrupts its
/// `read` builtin. The tree ignores SIGINT so the child Ctrl-C proof
/// (a 0x03 byte that must reach the reader as data) holds even when the
/// Supervisor's stop escalation interrupts the group.
const FOCUSED_SCRIPT: &str = "trap '' INT; stty raw -echo; printf 'focused-ready\\r\\n'; (trap '' INT; i=0; while :; do printf 'tick-%04d\\r\\n' $i; i=$((i+1)); sleep 0.5; done) & (trap '' INT; while :; do printf 'win-%s\\r\\n' \"$(stty size < /dev/tty)\"; sleep 1; done) & exec perl -e 'select STDOUT; $| = 1; binmode STDIN; my $n = 0; my $buf = \"\"; while (defined(my $b = getc(STDIN))) { if ($b eq \"\\r\" or $b eq \"\\n\") { $n = $n + 1; if (length($buf) > 0) { print \"input-hex-$n:\", unpack(\"H*\", $buf), \"\\r\\n\"; $buf = \"\"; } next; } $buf = $buf . $b; }'";

/// An input-disabled PTY Service: it ticks, and its reader would print any
/// input it ever received as `mute-input-hex` — which the input gate must
/// keep from ever happening.
const MUTE_SCRIPT: &str = "trap '' INT; stty raw -echo; printf 'mute-ready\\r\\n'; (trap '' INT; i=0; while :; do printf 'mute-tick-%04d\\r\\n' $i; i=$((i+1)); sleep 0.5; done) & (trap '' INT; while :; do printf 'win-%s\\r\\n' \"$(stty size < /dev/tty)\"; sleep 1; done) & exec perl -e 'select STDOUT; $| = 1; binmode STDIN; my $n = 0; my $buf = \"\"; while (defined(my $b = getc(STDIN))) { if ($b eq \"\\r\" or $b eq \"\\n\") { $n = $n + 1; if (length($buf) > 0) { print \"mute-input-hex-$n:\", unpack(\"H*\", $buf), \"\\r\\n\"; $buf = \"\"; } next; } $buf = $buf . $b; }'";

/// A pipe Service that ticks fast into the bounded per-Process output
/// module.
const PIPED_SCRIPT: &str =
    "i=0; while :; do printf 'pipe-tick-%04d\\n' $i; i=$((i+1)); sleep 0.1; done";

/// A pipe One-shot: one line per attempt, then a clean exit.
const ONEOFF_SCRIPT: &str = "printf 'oneoff-run ok\\n'";

fn fixture_config() -> String {
    let focused = FOCUSED_SCRIPT.replace('"', "\\\"");
    let mute = MUTE_SCRIPT.replace('"', "\\\"");
    let piped = PIPED_SCRIPT.replace('"', "\\\"");
    let oneoff = ONEOFF_SCRIPT.replace('"', "\\\"");
    format!(
        "version: 1\n\
         processes:\n\
         \x20 - name: focused\n\
         \x20   kind: service\n\
         \x20   terminal: pty\n\
         \x20   input: focused\n\
         \x20   command:\n\
         \x20     program: /bin/sh\n\
         \x20     args: [\"-c\", \"{focused}\"]\n\
         \x20 - name: mute\n\
         \x20   kind: service\n\
         \x20   terminal: pty\n\
         \x20   command:\n\
         \x20     program: /bin/sh\n\
         \x20     args: [\"-c\", \"{mute}\"]\n\
         \x20 - name: piped\n\
         \x20   kind: service\n\
         \x20   terminal: pipe\n\
         \x20   command:\n\
         \x20     program: /bin/sh\n\
         \x20     args: [\"-c\", \"{piped}\"]\n\
         \x20 - name: oneoff\n\
         \x20   kind: one-shot\n\
         \x20   terminal: pipe\n\
         \x20   autostart: false\n\
         \x20   command:\n\
         \x20     program: /bin/sh\n\
         \x20     args: [\"-c\", \"{oneoff}\"]\n",
    )
}

#[test]
fn terminal_operation_across_process_selection() {
    let dir = unique_dir("interaction");
    let config_path = dir.join("stackhand.yaml");
    fs::write(&config_path, fixture_config()).expect("config writes");

    let mut command = StdCommand::new(env!("CARGO_BIN_EXE_stackhand"));
    command.arg("--fixture-interaction");
    command.arg(&config_path);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    // The fixture binary's child shells share its process group: when
    // the binary must be killed, its whole group goes with it so nobody
    // keeps the test pipes open. SAFETY: pre_exec runs setpgid in the
    // child before exec; setpgid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            let status = libc::setpgid(0, 0);
            if status == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let mut child = command.spawn().expect("the fixture binary starts");
    let mut stdout_pipe = child.stdout.take().expect("the stdout pipe is captured");
    let mut stderr_pipe = child.stderr.take().expect("the stderr pipe is captured");

    let deadline = Instant::now() + BINARY_WAIT;
    let status: std::process::ExitStatus = loop {
        match child.try_wait() {
            Ok(Some(observed)) => break observed,
            Ok(None) => {
                if Instant::now() >= deadline {
                    child.kill().ok();
                    // Kill the fixture's whole process group so its
                    // descendant shells release the pipes; the drains
                    // below then reach EOF. SAFETY: the fixture binary
                    // started in its own process group, so the negative
                    // pid addresses only that group, never this test.
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGTERM);
                    }
                    child.wait().ok();
                    let mut timed_out = Vec::new();
                    std::io::Read::read_to_end(&mut stdout_pipe, &mut timed_out).ok();
                    let mut timed_out_err = Vec::new();
                    std::io::Read::read_to_end(&mut stderr_pipe, &mut timed_out_err).ok();
                    panic!(
                        "fixture timed out after {BINARY_WAIT:?}: {} {}",
                        String::from_utf8_lossy(&timed_out),
                        String::from_utf8_lossy(&timed_out_err)
                    );
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(error) => panic!("the fixture binary failed to run: {error}"),
        }
    };
    // The child has exited; drain the pipes to their end.
    let stdout_text = {
        let mut buffer = Vec::new();
        std::io::Read::read_to_end(&mut stdout_pipe, &mut buffer).expect("the stdout pipe drains");
        String::from_utf8_lossy(&buffer).into_owned()
    };
    let stderr_text = {
        let mut buffer = Vec::new();
        std::io::Read::read_to_end(&mut stderr_pipe, &mut buffer).expect("the stderr pipe drains");
        String::from_utf8_lossy(&buffer).into_owned()
    };

    let stdout = stdout_text;
    assert!(status.success(), "fixture failed: {stdout} {stderr_text}");
    // All three Processes are up: two PTY Services (focused input,
    // disabled input) and one fast pipe Service.
    assert!(stdout.contains("interaction-started-ok"), "{stdout}");
    // Input reaches the selected focused PTY through the pane key seam,
    // child Ctrl-C stays child input, and the Ctrl-A leader round-trips.
    assert!(stdout.contains("interaction-input-ok"), "{stdout}");
    // The disabled-input PTY and the pipe pane reject child input
    // visibly; the leader, pipe scrolling, and unavailable selection
    // behave per pane.
    assert!(stdout.contains("interaction-reject-ok"), "{stdout}");
    // Scroll and follow are per Process view, across a real selection move.
    assert!(stdout.contains("interaction-scroll-ok"), "{stdout}");
    // A resize reaches only the selected PTY, and never zero dimensions.
    assert!(stdout.contains("interaction-resize-ok"), "{stdout}");
    // Stop, start, and restart target the selected Service through the
    // Supervisor; a clean stop and restart leave no failure behind.
    assert!(stdout.contains("interaction-lifecycle-ok"), "{stdout}");
    // A One-shot rerun opens the next Run, keeps both attempts' output
    // markers, and records bounded recent Run summaries.
    assert!(stdout.contains("interaction-rerun-ok"), "{stdout}");
    // Selection moves never stop output ingestion for any Process.
    assert!(stdout.contains("interaction-ingest-ok"), "{stdout}");
    assert!(stdout.contains("interaction-shutdown-ok"), "{stdout}");

    fs::remove_dir_all(&dir).ok();
}
