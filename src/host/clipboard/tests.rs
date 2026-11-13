use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use crate::console::ConsoleInteraction;
use crate::geometry::TerminalGeometry;
use crate::host::SessionStop;
use crate::runtime::PtyIo;
use crate::terminal::TerminalSession;
use crate::tui::ConsoleWarning;

#[derive(Default, Clone)]
struct Wire(Arc<Mutex<Vec<u8>>>);

impl Write for Wire {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl OutputSink for Wire {
    fn wait_writable(&mut self, _: Duration) -> io::Result<()> {
        Ok(())
    }
}

fn backend(environment: &[(&str, &str)]) -> ClipboardBackend {
    ClipboardBackend::from_environment(|key| {
        environment
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| (*value).into())
    })
    .unwrap()
}

#[test]
fn ssh_environment_overrides_remote_display_and_explicit_backend_overrides_ssh() {
    for key in ["SSH_TTY", "SSH_CONNECTION", "SSH_CLIENT"] {
        assert_eq!(
            backend(&[(key, "remote"), ("DISPLAY", ":0")]),
            ClipboardBackend::Osc52
        );
        for (value, expected) in [
            ("native", ClipboardBackend::Native),
            ("osc52", ClipboardBackend::Osc52),
            ("off", ClipboardBackend::Off),
        ] {
            assert_eq!(
                backend(&[(key, "remote"), ("STACKHAND_CLIPBOARD", value)]),
                expected
            );
        }
    }
    assert_eq!(backend(&[("DISPLAY", ":0")]), ClipboardBackend::Native);
    #[cfg(target_os = "macos")]
    assert_eq!(backend(&[]), ClipboardBackend::Native);
    #[cfg(target_os = "linux")]
    assert_eq!(backend(&[]), ClipboardBackend::Osc52);
    assert!(ClipboardBackend::from_environment(|_| Some("unknown".into())).is_err());
}

#[test]
fn empty_oversized_and_disabled_copies_do_not_mutate_output_or_initialize_native_clipboard() {
    let wire = Wire::default();
    let mut output = HostOutput::new(wire.clone(), SessionStop::default());
    for backend in [
        ClipboardBackend::Native,
        ClipboardBackend::Osc52,
        ClipboardBackend::Off,
    ] {
        let mut service = ClipboardService::new(backend);
        assert_eq!(service.copy(String::new(), &mut output), CopyOutcome::Empty);
        assert_eq!(
            service.copy("é".repeat(32_769), &mut output),
            CopyOutcome::TooLarge
        );
        assert!(service.native.is_none());
    }
    let mut disabled = ClipboardService::new(ClipboardBackend::Off);
    assert_eq!(
        disabled.copy("private".into(), &mut output),
        CopyOutcome::Disabled
    );
    assert!(wire.0.lock().unwrap().is_empty());

    let mut osc52 = ClipboardService::new(ClipboardBackend::Osc52);
    assert_eq!(
        osc52.copy("next".into(), &mut output),
        CopyOutcome::Submitted
    );
    assert_eq!(*wire.0.lock().unwrap(), b"\x1b]52;c;bmV4dA==\x1b\\");
}

fn finish_copy(
    console: &mut ConsoleInteraction,
    service: &mut ClipboardService,
    output: &mut HostOutput<Wire>,
) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !console.poll_requests(|text| service.copy(text, output)) {
        assert!(Instant::now() < deadline, "copy request did not complete");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn terminal_and_logs_user_copies_share_owned_output_and_failure_keeps_terminal_selection() {
    let (reader, mut peer) = UnixStream::pair().unwrap();
    let session = TerminalSession::spawn(
        PtyIo {
            reader: Box::new(reader),
            writer: Box::new(io::sink()),
            resizer: Box::new(|_, _| Ok(())),
        },
        TerminalGeometry::DEFAULT,
        || {},
    )
    .unwrap();
    peer.write_all(b"selected text").unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while session.snapshot().text() != "selected text" {
        assert!(
            Instant::now() < deadline,
            "child output did not reach terminal owner"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let stopping = AtomicBool::new(false);
    let handle = crate::runtime::handle_for_test(&session, &stopping);
    let key = |code| KeyEvent::new(KeyCode::Char(code), KeyModifiers::NONE);
    let mut console = ConsoleInteraction::default();
    console.handle_key(key('v'), &handle, 20);
    console.handle_key(key('a'), &handle, 20);
    console.handle_key(key('c'), &handle, 20);
    let wire = Wire::default();
    let stop = SessionStop::default();
    stop.stop();
    let mut cancelled = HostOutput::new(wire.clone(), stop);
    let mut service = ClipboardService::new(ClipboardBackend::Osc52);
    finish_copy(&mut console, &mut service, &mut cancelled);
    assert_eq!(
        console.view().warning,
        Some(ConsoleWarning::ClipboardFailed)
    );
    assert!(wire.0.lock().unwrap().is_empty());
    assert!(
        service.native.is_none(),
        "OSC52 failure must not fall back to native"
    );

    console.handle_key(key('c'), &handle, 20);
    let mut output = HostOutput::new(wire.clone(), SessionStop::default());
    finish_copy(&mut console, &mut service, &mut output);
    assert_eq!(console.view().warning, None);
    console.copy_logs("logs text".into());
    finish_copy(&mut console, &mut service, &mut output);
    assert_eq!(
        *wire.0.lock().unwrap(),
        b"\x1b]52;c;c2VsZWN0ZWQgdGV4dA==\x1b\\\x1b]52;c;bG9ncyB0ZXh0\x1b\\"
    );
    drop(peer);
    session.shutdown().unwrap();
}

#[test]
fn logs_copy_rejects_empty_and_oversized_text_and_keeps_only_one_pending_intent() {
    let mut console = ConsoleInteraction::default();
    console.copy_logs(String::new());
    assert_eq!(console.view().warning, Some(ConsoleWarning::NoLogsToCopy));
    console.copy_logs("x".repeat(65_537));
    assert_eq!(console.view().warning, Some(ConsoleWarning::CopyTooLarge));
    assert!(!console.poll_requests(|_| panic!("rejected copy must not reach clipboard")));

    console.copy_logs("first".into());
    console.copy_logs("second".into());
    assert_eq!(console.view().warning, Some(ConsoleWarning::CopyBusy));
    let wire = Wire::default();
    let mut output = HostOutput::new(wire.clone(), SessionStop::default());
    let mut service = ClipboardService::new(ClipboardBackend::Osc52);
    finish_copy(&mut console, &mut service, &mut output);
    assert!(!console.poll_requests(|_| panic!("only one copy may be pending")));
    assert_eq!(*wire.0.lock().unwrap(), b"\x1b]52;c;Zmlyc3Q=\x1b\\");
}
