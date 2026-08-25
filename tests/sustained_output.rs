use stackhand::stress::{run_blocked_input_output_fixture, run_sustained_output_fixture};

#[test]
fn sustained_output_keeps_the_terminal_responsive_and_bounded() {
    let report = run_sustained_output_fixture().expect("sustained output fixture must pass");

    println!("sustained-output-report: {report:?}");
    assert!(report.redraw_notifications <= report.wake_requests);
    assert!(report.active_snapshots > 0);
    assert!(report.scrolled_snapshots > 0);
    assert!(report.output_callbacks_during_unfocused > 0);
    assert!(report.history_bytes > 0);
}

#[test]
fn blocked_input_does_not_starve_noisy_output() {
    let report = run_blocked_input_output_fixture().expect("blocked-input fixture must pass");

    println!("blocked-input-output-report: {report:?}");
    assert!(report.pending_pastes_during_flood > 0);
    assert!(report.output_bytes_after > report.output_bytes_before);
    assert!(report.wakes_after > report.wakes_before);
}
