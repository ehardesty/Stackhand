#![cfg(unix)]

#[path = "support/session.rs"]
mod session;

use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use session::{Project, Terminal, WAIT, wait_until};

#[test]
fn redirected_interactive_start_never_starts_processes() {
    for redirection in ["</dev/null", ">/dev/null", "</dev/null >/dev/null"] {
        let project = Project::new();
        let mut terminal = Terminal::start(&project, &[], Some(redirection));
        assert!(!terminal.wait().success());
        assert!(terminal.text().contains("interactive terminal"));
        project.assert_no_start();
    }
}

#[test]
fn redirected_config_commands_do_not_require_a_terminal() {
    let project = Project::new();
    for subcommand in ["show", "validate"] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_stackhand"))
            .args(["config", subcommand])
            .arg(&project.config)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    project.assert_no_start();
}

#[test]
fn console_ctrl_c_is_data_and_quit_cleans_both_process_trees() {
    let mut project = Project::new();
    let mut terminal = Terminal::start(&project, &[], None);
    project.capture_trees();
    wait_until(
        || terminal.text().contains("session-ready"),
        "PTY console to render",
    );
    terminal.send(b"\t\x03");
    wait_until(
        || fs::read(project.root.join("input")).is_ok_and(|bytes| bytes.contains(&3)),
        "raw Ctrl-C at child",
    );
    thread::sleep(Duration::from_secs(1));
    assert!(terminal.child.try_wait().unwrap().is_none());
    terminal.send(b"\x01q");
    assert!(terminal.wait().success(), "{}", terminal.text());
    project.assert_trees_gone();
}

#[test]
fn term_reports_cleanup_and_cleans_both_process_trees() {
    let mut project = Project::new();
    let mut terminal = Terminal::start(&project, &[], None);
    project.capture_trees();
    unsafe {
        assert_eq!(
            libc::kill(terminal.child.process_id().unwrap() as i32, libc::SIGTERM),
            0
        );
    }
    assert!(terminal.wait().success(), "{}", terminal.text());
    assert!(terminal.text().contains("Project shutdown complete"));
    project.assert_trees_gone();
}

#[test]
fn closing_the_master_cleans_both_process_trees_within_the_session_bound() {
    let mut project = Project::new();
    let mut terminal = Terminal::start(&project, &[], None);
    project.capture_trees();
    let started = Instant::now();
    terminal.hangup();
    terminal.wait();
    project.assert_trees_gone();
    assert!(started.elapsed() < WAIT);
}

#[test]
fn no_autostart_leaves_all_processes_stopped() {
    let project = Project::new();
    let mut terminal = Terminal::start(&project, &["--no-autostart"], None);
    wait_until(|| terminal.text().contains("pty"), "Project UI to render");
    project.assert_no_start();
    terminal.send(b"q");
    assert!(terminal.wait().success(), "{}", terminal.text());
}

#[test]
fn term_cleans_process_trees_while_outer_output_stays_stalled() {
    let mut project = Project::new();
    let mut terminal = Terminal::start(&project, &[], None);
    project.capture_trees();
    wait_until(
        || terminal.text().contains("session-ready"),
        "PTY console to render",
    );
    terminal.pause_output_and_resize();
    thread::sleep(Duration::from_millis(300));
    unsafe {
        assert_eq!(
            libc::kill(terminal.child.process_id().unwrap() as i32, libc::SIGTERM),
            0
        );
    }
    // wait resumes output only after observing application exit.
    assert!(terminal.wait().success());
    project.assert_trees_gone();
}

#[test]
fn oversized_host_paste_never_delivers_its_command_like_suffix() {
    let mut project = Project::new();
    let mut terminal = Terminal::start(&project, &[], None);
    project.capture_trees();
    wait_until(
        || terminal.text().contains("session-ready"),
        "PTY console to render",
    );
    terminal.send(b"\t");
    terminal.send(b"\x1b[200~");
    terminal.send(&vec![b'p'; 64 * 1024 + 1]);
    terminal.send(b"\nexecute-me\r\x03\x1b[201x\x1b[20");
    terminal.send(b"1~x");
    wait_until(
        || fs::read(project.root.join("input")).is_ok_and(|bytes| bytes == b"x"),
        "only the post-paste key to reach the child",
    );
    terminal.send(b"\x11");
    assert!(terminal.wait().success());
    assert_eq!(fs::read(project.root.join("input")).unwrap(), b"x");
    project.assert_trees_gone();
}

#[test]
fn redirected_regular_file_keeps_the_interactive_shutdown_report() {
    let project = Project::new();
    let report = project.root.join("report.txt");
    let redirect = format!("2>'{}'", report.display());
    let mut terminal = Terminal::start(&project, &["--no-autostart"], Some(&redirect));
    wait_until(|| terminal.text().contains("pty"), "Project UI to render");
    terminal.send(b"q");
    assert!(terminal.wait().success());
    assert!(
        fs::read_to_string(report)
            .unwrap()
            .contains("Project shutdown complete")
    );
}
