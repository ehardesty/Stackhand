use std::fs;
use std::path::Path;

use super::*;

#[test]
fn checked_in_example_projects_load() {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut paths = fs::read_dir(&examples)
        .expect("the examples directory exists")
        .map(|entry| entry.expect("an example directory entry reads").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "yaml")
        })
        .collect::<Vec<_>>();
    paths.sort();
    assert_eq!(paths.len(), 5, "every documented example is checked");

    for path in paths {
        let yaml = fs::read_to_string(&path).expect("the example YAML reads");
        for temporary_form in [
            "- name:",
            "working_dir:",
            "env:",
            "\n    input:",
            "terminal: pty",
            "terminal: pipe",
            "    depends_on:\n      -",
            "    command:\n      program:",
            "    program:",
        ] {
            assert!(
                !yaml.contains(temporary_form),
                "example Project '{}' must use canonical YAML, not '{temporary_form}'",
                path.display()
            );
        }
        load(&path).unwrap_or_else(|error| {
            panic!("example Project '{}' must load: {error}", path.display())
        });
    }
}

#[test]
fn removed_yaml_forms_name_their_canonical_replacements() {
    let cases = [
        (
            "process-list",
            "version: 1\nprocesses:\n  - name: web\n    command: [/bin/true]\n",
            "processes must be a name-keyed mapping",
            "use 'processes: {name: {...}}'",
        ),
        (
            "dependency-list",
            "version: 1\nprocesses:\n  web:\n    depends_on: [db]\n    command: [/bin/true]\n  db:\n    command: [/bin/true]\n",
            "depends_on must be a name-keyed mapping",
            "use 'depends_on: {process-name: condition}'",
        ),
        (
            "group-list",
            "version: 1\ngroups: [web]\nprocesses:\n  web:\n    command: [/bin/true]\n",
            "groups must be a name-keyed mapping",
            "use 'groups: {Group name: [process-name]}'",
        ),
        (
            "command-map",
            "version: 1\nprocesses:\n  web:\n    command: {program: /bin/true}\n",
            "command must be a sequence",
            "use 'command: [program, arg1, ...]'",
        ),
        (
            "working-directory",
            "version: 1\nprocesses:\n  web:\n    working_dir: .\n    command: [/bin/true]\n",
            "unknown field `working_dir`",
            "use `cwd` instead",
        ),
        (
            "environment",
            "version: 1\nprocesses:\n  web:\n    env: {MODE: test}\n    command: [/bin/true]\n",
            "unknown field `env`",
            "use `environment` instead",
        ),
        (
            "input",
            "version: 1\nprocesses:\n  web:\n    input: focused\n    command: [/bin/true]\n",
            "unknown field `input`",
            "put `input` under the `terminal` mapping instead",
        ),
        (
            "terminal-scalar",
            "version: 1\nprocesses:\n  web:\n    terminal: pty\n    command: [/bin/true]\n",
            "terminal must be a mapping",
            "use 'terminal: {mode: pipe|pty, input: disabled|focused}'",
        ),
    ];

    for (label, yaml, old_form, replacement) in cases {
        let error = write_and_load_schema(label, yaml).expect_err("the temporary form must fail");
        assert!(error.message.contains(old_form), "{label}: {error}");
        assert!(error.message.contains(replacement), "{label}: {error}");
    }
}

fn write_and_load_schema(label: &str, yaml: &str) -> Result<EffectiveProject, ConfigError> {
    let dir = std::env::temp_dir().join(format!("stackhand-config-schema-{label}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("config directory creates");
    let path = dir.join("stackhand.yaml");
    fs::write(&path, yaml).expect("config writes");
    let project = load(&path);
    let _ = fs::remove_dir_all(&dir);
    project
}
