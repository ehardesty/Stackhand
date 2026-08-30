use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEventKind};

use super::interaction::{is_quit, mouse_changes_focus, mouse_starts_console_focus, should_quit};
use super::view_model::{
    format_age, format_cpu, format_rss, metric_precision, process_list_title, process_rows,
    selected_header, status_label,
};
use super::*;
use crate::tui::process_row_at;

#[test]
fn shutdown_result_keeps_failures_and_remaining_pids() {
    let result = ProjectShutdownSnapshot {
        complete: true,
        timed_out: true,
        failures: vec![crate::supervisor::ProcessShutdownFailure {
            process: "api".to_string(),
            detail: "Project shutdown deadline expired".to_string(),
            remaining_pids: vec![41, 42],
        }],
    };

    let error = report_shutdown_result(&result).expect_err("cleanup failure is reported");
    assert_eq!(
        error.to_string(),
        "Project shutdown did not finish cleanly: api: Project shutdown deadline expired (remaining PIDs: [41, 42])"
    );
}

#[test]
fn metric_and_age_labels_use_compact_units() {
    assert_eq!(format_cpu(3.24), "3.2%");
    assert_eq!(format_cpu(12.4), "12%");
    assert_eq!(format_rss(768), "768K");
    assert_eq!(format_rss(180 * 1024), "180M");
    assert_eq!(format_rss(1024 * 1024), "1.0G");
    assert_eq!(metric_precision("12%".to_string(), true), "~12%");
    assert_eq!(metric_precision("12%".to_string(), false), "12%");
    assert_eq!(format_age(59_000), "59s");
    assert_eq!(format_age(61_000), "1m1s");
}

#[test]
fn profile_column_appears_for_pending_and_mixed_process_profiles() {
    let mut first = projection_process();
    first.current_profile = Some("local".to_string());
    first.next_profile = Some("cloud-dev".to_string());
    let mut second = projection_process();
    second.name = "worker".to_string();
    second.process_id = crate::supervisor::ProcessId::new(2);
    second.current_profile = Some("cloud-dev".to_string());
    second.next_profile = Some("cloud-dev".to_string());
    let snapshot = crate::supervisor::ProjectSnapshot {
        processes: vec![first, second],
        selected_profile: Some("cloud-dev".to_string()),
        available_profiles: vec!["cloud-dev".to_string(), "local".to_string()],
        shutdown: None,
        now_ms: 0,
    };

    let rows = process_rows(&snapshot, 0);
    assert_eq!(rows[0].profile.as_deref(), Some("local → cloud-dev"));
    assert_eq!(rows[1].profile.as_deref(), Some("cloud-dev"));
    assert_eq!(
        process_list_title(&snapshot),
        "Processes · Profile: cloud-dev · 1 pending"
    );
}

#[test]
fn profile_column_hides_when_global_profile_describes_every_process() {
    let mut process = projection_process();
    process.current_profile = Some("local".to_string());
    process.next_profile = Some("local".to_string());
    let snapshot = crate::supervisor::ProjectSnapshot {
        processes: vec![process],
        selected_profile: Some("local".to_string()),
        available_profiles: vec!["local".to_string()],
        shutdown: None,
        now_ms: 0,
    };

    assert_eq!(process_rows(&snapshot, 0)[0].profile, None);
    assert_eq!(process_list_title(&snapshot), "Processes · Profile: local");
}

fn projection_process() -> crate::supervisor::ProcessSnapshot {
    crate::supervisor::ProcessSnapshot {
        process_id: crate::supervisor::ProcessId::new(1),
        name: "api".to_string(),
        kind: crate::model::ProcessKind::Service,
        enabled: true,
        autostart: true,
        input_focused: false,
        desired: crate::supervisor::DesiredState::Running,
        lifecycle: crate::supervisor::Lifecycle::Running,
        terminal_mode: crate::model::TerminalMode::Pipe,
        current_run: Some(7),
        current_profile: None,
        next_profile: None,
        root_pid: Some(42),
        run_started_at_ms: Some(1_000),
        failure: None,
        metrics: None,
        blocked_reason: None,
        readiness: None,
        liveness: None,
        restart_backoff: None,
        automatic_restart_budget: crate::supervisor::RestartBudgetStatus {
            automatic_retries_used: 0,
            max_restarts: 2,
            exhausted: false,
        },
        recent_runs: Vec::new(),
    }
}

#[test]
fn tui_projection_keeps_lifecycle_reasons_visible() {
    let mut process = projection_process();

    process.lifecycle = crate::supervisor::Lifecycle::Waiting;
    process.current_run = None;
    process.blocked_reason = Some("all-ready: ready".to_string());
    assert_eq!(status_label(&process), "Waiting (all-ready: ready)");
    assert!(selected_header(&process, 5_000).contains("all-ready: ready"));

    process.lifecycle = crate::supervisor::Lifecycle::Running;
    process.current_run = Some(7);
    process.blocked_reason = None;
    process.readiness = Some(crate::supervisor::ReadinessStatus {
        kind: crate::supervisor::ReadinessCheckKind::Http,
        state: crate::supervisor::ReadinessState::Failing,
        attempts: 3,
        consecutive_successes: 0,
        consecutive_failures: 3,
        last_error: Some("HTTP status 503".to_string()),
        startup_elapsed_ms: 500,
        children: Vec::new(),
    });
    assert!(selected_header(&process, 5_000).contains("readiness attempt 3: HTTP status 503"));

    process.readiness = None;
    process.liveness = Some(crate::supervisor::LivenessStatus {
        kind: crate::supervisor::LivenessCheckKind::Http,
        state: crate::supervisor::LivenessState::Failing,
        attempts: 3,
        consecutive_successes: 0,
        consecutive_failures: 3,
        last_error: Some("HTTP status 503".to_string()),
        elapsed_ms: 500,
        children: Vec::new(),
    });
    assert_eq!(status_label(&process), "Unhealthy");

    process.lifecycle = crate::supervisor::Lifecycle::RestartBackoff;
    process.current_run = None;
    process.liveness = None;
    process.restart_backoff = Some(crate::supervisor::RestartBackoffStatus {
        reason: "unhealthy".to_string(),
        next_attempt_at_ms: 7_500,
    });
    process.automatic_restart_budget.automatic_retries_used = 1;
    assert_eq!(status_label(&process), "Restarting (unhealthy)");
    let header = selected_header(&process, 5_000);
    assert!(header.contains("automatic retries 1/2"));
    assert!(header.contains("next restart at 7500ms"));

    process.lifecycle = crate::supervisor::Lifecycle::Stopped;
    process.restart_backoff = None;
    process.failure = Some(crate::supervisor::FailureSummary {
        kind: crate::supervisor::FailureKind::RestartLimit,
        detail: "Restart limit exhausted".to_string(),
    });
    assert_eq!(status_label(&process), "Failed (Restart limit exhausted)");
    assert!(selected_header(&process, 5_000).contains("Restart limit exhausted"));
}

#[test]
fn plain_q_quits_only_from_process_list_focus() {
    let plain_q = KeyEvent::new(KeyCode::Char('q'), crossterm::event::KeyModifiers::NONE);
    let ctrl_q = KeyEvent::new(KeyCode::Char('q'), crossterm::event::KeyModifiers::CONTROL);
    let list = crate::tui::ConsoleViewState::default();
    let console = crate::tui::ConsoleViewState {
        mode: crate::tui::ConsoleViewMode::Console,
        ..list
    };

    assert!(is_quit(plain_q, list));
    assert!(!is_quit(plain_q, console));
    assert!(is_quit(ctrl_q, list));
    assert!(is_quit(ctrl_q, console));
    assert!(!should_quit(plain_q, list, true));
    assert!(should_quit(ctrl_q, console, true));
}

#[test]
fn only_a_mouse_press_changes_keyboard_focus() {
    assert!(mouse_changes_focus(MouseEventKind::Down(MouseButton::Left)));
    assert!(!mouse_changes_focus(MouseEventKind::ScrollUp));
    assert!(!mouse_changes_focus(MouseEventKind::Drag(
        MouseButton::Left
    )));
    assert!(!mouse_changes_focus(MouseEventKind::Up(MouseButton::Left)));
    assert!(mouse_starts_console_focus(
        MouseEventKind::Down(MouseButton::Left),
        crate::tui::ConsoleViewMode::ProcessList
    ));
    assert!(!mouse_starts_console_focus(
        MouseEventKind::Down(MouseButton::Left),
        crate::tui::ConsoleViewMode::Copy
    ));
}

#[test]
fn process_row_hit_testing_excludes_the_header_borders_and_empty_rows() {
    let list = ratatui::layout::Rect::new(0, 0, 40, 6);

    assert_eq!(process_row_at(list, 0, 3, 0), None);
    assert_eq!(process_row_at(list, 1, 3, 0), None);
    assert_eq!(process_row_at(list, 2, 3, 0), Some(0));
    assert_eq!(process_row_at(list, 4, 3, 0), Some(2));
    assert_eq!(process_row_at(list, 5, 3, 0), None);
}

#[test]
fn process_row_hit_testing_tracks_the_visible_table_offset() {
    let list = ratatui::layout::Rect::new(0, 0, 40, 6);

    assert_eq!(process_row_at(list, 2, 8, 7), Some(5));
    assert_eq!(process_row_at(list, 4, 8, 7), Some(7));
}

#[test]
fn rapid_resize_uses_only_the_last_valid_geometry() {
    let started = Instant::now();
    let mut pending = PendingResize::default();
    pending.update(TerminalGeometry::new(120, 40).unwrap(), started);
    pending.update(
        TerminalGeometry::new(1, 1).unwrap(),
        started + Duration::from_millis(2),
    );
    pending.update(
        TerminalGeometry::new(73, 19).unwrap(),
        started + Duration::from_millis(4),
    );

    assert_eq!(
        pending.take_ready(started + Duration::from_millis(19)),
        None
    );
    assert_eq!(
        pending.take_ready(started + Duration::from_millis(20)),
        TerminalGeometry::new(73, 19)
    );
    assert_eq!(
        pending.take_ready(started + Duration::from_millis(21)),
        None
    );
}
