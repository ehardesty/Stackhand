use super::*;
use crate::supervisor::start_with;

#[test]
fn the_task_wrapper_serves_snapshots_until_the_handle_drops() {
    let handle = start_with(
        four_process_project(),
        Box::new(FakeRuntime::default()),
        Box::new(FakeProbes::default()),
        Arc::new(FakeClock::new()),
        crate::geometry::TerminalGeometry::DEFAULT,
    );
    // Wait for the initial snapshot through the bounded public request.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let snapshot = loop {
        if let Some(snapshot) = handle.snapshot() {
            break snapshot;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "snapshot was not served in time"
        );
    };
    assert_eq!(snapshot.processes.len(), 4);
    assert_eq!(snapshot.processes[0].name, "api");

    handle.command(Command::StartAutostart);

    // Events reach the same serialized task from adapter threads.
    handle.deliver_event(spawned("api", 1));
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let Some(snapshot) = handle.snapshot() else {
            panic!("snapshot was not served in time");
        };
        if snapshot.named("api").unwrap().lifecycle == Lifecycle::Running {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "event was not applied in time"
        );
    }
    handle.stop_task();
}
