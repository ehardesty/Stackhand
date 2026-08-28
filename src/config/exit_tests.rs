use super::*;
use std::fs;

fn write_and_load(label: &str, yaml: &str) -> Result<EffectiveProject, ConfigError> {
    let dir = std::env::temp_dir().join(format!("stackhand-config-{label}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("config directory creates");
    let path = dir.join("stackhand.yaml");
    fs::write(&path, yaml).expect("config writes");
    let project = load(&path);
    let _ = fs::remove_dir_all(&dir);
    project
}

#[test]
fn success_exit_codes_are_unique_and_in_the_operating_system_range() {
    let accepted = write_and_load(
        "success-codes-ok",
        "version: 1\nprocesses:\n  - name: setup\n    kind: one-shot\n    success_exit_codes: [0, 2, 130]\n    command: {program: /bin/true}\n",
    )
    .expect("valid success exit codes");
    assert_eq!(accepted.processes()[0].success_exit_codes, vec![0, 2, 130]);

    for (label, codes, expected) in [
        ("success-codes-duplicate", "[0, 2, 0]", "unique"),
        ("success-codes-negative", "[-1]", "0 through 255"),
        ("success-codes-too-large", "[256]", "0 through 255"),
        ("success-codes-empty", "[]", "at least one"),
    ] {
        let error = write_and_load(
            label,
            &format!(
                "version: 1\nprocesses:\n  - name: setup\n    kind: one-shot\n    success_exit_codes: {codes}\n    command: {{program: /bin/true}}\n"
            ),
        )
        .expect_err("invalid success exit codes must fail");
        assert!(error.message.contains(expected), "{label}: {error}");
    }
}

#[test]
fn exited_condition_is_valid_only_on_one_shot_dependencies() {
    let accepted = write_and_load(
        "deps-exited-ok",
        "version: 1\nprocesses:\n  - name: web\n    depends_on: [{name: setup, condition: exited}]\n    command: {program: /bin/true}\n  - name: setup\n    kind: one-shot\n    command: {program: /bin/true}\n",
    )
    .expect("a One-shot dependency accepts exited");
    assert_eq!(
        accepted.processes()[0].dependencies[0].condition,
        crate::model::DependencyCondition::Exited
    );

    let error = write_and_load(
        "deps-exited-service",
        "version: 1\nprocesses:\n  - name: web\n    depends_on: [{name: db, condition: exited}]\n    command: {program: /bin/true}\n  - name: db\n    command: {program: /bin/true}\n",
    )
    .expect_err("a Service dependency must reject exited");
    assert!(error.message.contains("'exited'"), "{error}");
}
