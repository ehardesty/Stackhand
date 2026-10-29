use std::fs;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::*;
use crate::output_settings::MIB;

fn project(yaml: &str) -> Result<EffectiveProject, ConfigError> {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("stackhand-output-{unique}.yaml"));
    fs::write(&path, yaml).unwrap();
    let result = load(&path);
    fs::remove_file(path).unwrap();
    result
}

#[test]
fn byte_settings_round_trip_through_config_show_and_reject_invalid_budgets() {
    let resolved = project("version: 1\nprocesses: {}\nsettings:\n  output:\n    terminal_scrollback_bytes: 8MiB\n    terminal_project_bytes: 64MiB\n    per_process_history_bytes: 2097152\n    global_history_bytes: 1GiB\n").unwrap();
    let limits = resolved.output_settings();
    assert_eq!(limits.terminal_scrollback_bytes, 8 * MIB);
    assert_eq!(limits.per_process_history_bytes, 2 * MIB);
    assert_eq!(limits.global_history_bytes, 1024 * MIB);
    let shown = show::render(&resolved).unwrap();
    assert_eq!(project(&shown).unwrap().output_settings(), limits);

    for setting in [
        "global_history_bytes: -1",
        "global_history_bytes: 99999999999999999999GiB",
        "global_history_bytes: 1MB",
        "global_history_bytes: 0",
        "global_history_bytes: 1MiB",
        "terminal_scrollback_bytes: 256MiB",
        "unknown_budget: 4MiB",
    ] {
        let yaml = format!("version: 1\nprocesses: {{}}\nsettings:\n  output:\n    {setting}\n");
        assert!(project(&yaml).is_err(), "accepted {setting}");
    }
}

#[test]
fn configured_project_reserves_terminal_shares_before_start_and_reports_native_memory() {
    let project = project(
        r#"version: 1
settings:
  output:
    terminal_scrollback_bytes: 2MiB
    terminal_project_bytes: 4MiB
processes:
  active:
    autostart: true
    command: [/bin/sh, -c, 'printf "ready\n"; sleep 30']
  standby:
    autostart: false
    command: [/usr/bin/true]
"#,
    )
    .unwrap();
    let (supervisor, consoles, _output) = crate::supervisor::start(project).unwrap();
    supervisor.command(crate::supervisor::Command::StartAutostart);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut observed = None;
    while Instant::now() < deadline {
        if let Some(snapshot) = supervisor.snapshot() {
            let process = &snapshot.processes[0];
            if let Some(view) = process
                .current_run
                .and_then(|run| consoles.view_process(process.process_id, run))
            {
                observed = view
                    .with(|handle| handle.metrics())
                    .filter(|metrics| metrics.memory.is_some() && metrics.parsed_bytes > 0);
                if observed.is_some() {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    supervisor.command(crate::supervisor::Command::Shutdown {
        deadline: Instant::now() + Duration::from_secs(3),
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stopped = false;
    while Instant::now() < deadline {
        stopped = supervisor.snapshot().is_some_and(|snapshot| {
            snapshot
                .processes
                .iter()
                .all(|process| process.current_run.is_none())
        });
        if stopped {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    supervisor.stop_task();
    assert!(stopped, "terminal test did not clean up its Run");
    let metrics = observed.expect("configured Run did not publish native metrics");
    // Both possible PTY Processes reserve a share, even while standby is idle.
    // One MiB per Process is reserved for page/active-screen headroom.
    assert_eq!(metrics.scrollback_limit_bytes, MIB);
    let memory = metrics.memory.unwrap();
    assert!(memory.page_bytes > 0);
    assert!(memory.resident_bytes > 0);
    assert!(memory.compressed_bytes <= memory.resident_bytes);
}
