use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn stackhand_repository_runs_as_a_small_real_project() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("stackhand-real-smoke-{unique}"));
    fs::create_dir_all(&dir).expect("smoke directory creates");
    let config = dir.join("stackhand.yaml");
    let repository = env!("CARGO_MANIFEST_DIR");
    let cargo = env!("CARGO");
    fs::write(
        &config,
        format!(
            "version: 1\n\
             processes:\n\
             \x20 - name: inspect\n\
             \x20   kind: one-shot\n\
             \x20   terminal: pipe\n\
             \x20   working_dir: \"{repository}\"\n\
             \x20   command:\n\
             \x20     program: \"{cargo}\"\n\
             \x20     args: [\"metadata\", \"--no-deps\", \"--format-version\", \"1\"]\n\
             \x20 - name: hold\n\
             \x20   depends_on: [{{name: inspect, condition: completed_successfully}}]\n\
             \x20   terminal: pipe\n\
             \x20   command:\n\
             \x20     program: /bin/sleep\n\
             \x20     args: [\"60\"]\n"
        ),
    )
    .expect("smoke configuration writes");

    let output = Command::new(env!("CARGO_BIN_EXE_stackhand"))
        .arg("--fixture-smoke")
        .arg(&config)
        .output()
        .expect("smoke fixture runs");
    assert!(
        output.status.success(),
        "smoke failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("real-project-cycles-ok"), "{stdout}");
    assert!(stdout.contains("real-project-smoke-ok"), "{stdout}");
    fs::remove_dir_all(dir).ok();
}
