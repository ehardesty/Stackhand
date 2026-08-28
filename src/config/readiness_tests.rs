use super::*;
use crate::config::load;
use std::fs;

fn write_and_load(label: &str, yaml: &str) -> Result<crate::model::EffectiveProject, ConfigError> {
    let dir = std::env::temp_dir().join(format!("stackhand-config-readiness-{label}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("config directory creates");
    let path = dir.join("stackhand.yaml");
    fs::write(&path, yaml).expect("config writes");
    let project = load(&path);
    let _ = fs::remove_dir_all(&dir);
    project
}

#[test]
fn http_readiness_parses_the_url_into_its_connect_target() {
    let project = write_and_load(
        "readiness-http-ok",
        "version: 1\nprocesses:\n  - name: web\n    command: {program: /bin/sleep, args: [\"1\"]}\n    ready:\n      http:\n        url: \"http://localhost:8080/healthz?probe=1\"\n",
    )
    .expect("valid http readiness");
    let readiness = project.processes()[0]
        .readiness
        .clone()
        .expect("readiness parses");
    let check = &readiness.checks[0];
    assert_eq!(
        check.probe,
        ReadinessProbe::Http {
            host: "localhost".into(),
            port: 8080,
            path: "/healthz?probe=1".into(),
        }
    );
    assert_eq!(check.interval, Duration::from_millis(1_000));
    assert_eq!(check.timeout, Duration::from_millis(2_000));

    // The default port and path come from the URL.
    let bare = write_and_load(
        "readiness-http-bare",
        "version: 1\nprocesses:\n  - name: web\n    command: {program: /bin/true}\n    ready:\n      http: {url: \"http://example.test\"}\n",
    )
    .expect("a bare http URL is valid");
    let readiness = bare.processes()[0].readiness.clone().expect("parses");
    assert_eq!(
        readiness.checks[0].probe,
        ReadinessProbe::Http {
            host: "example.test".into(),
            port: 80,
            path: "/".into(),
        }
    );
}

#[test]
fn invalid_http_readiness_urls_are_rejected_clearly() {
    let base = "version: 1\nprocesses:\n  - name: web\n    command: {program: /bin/true}\n    ready:\n      http: {url: \"";
    let cases = [
        ("https", "https://example.test/\"}"),
        ("no scheme", "example.test/healthz\"}"),
        ("no host", "http:///healthz\"}"),
        ("bad port", "http://example.test:0/\"}"),
        ("non-numeric port", "http://example.test:none/\"}"),
        ("userinfo", "http://user@example.test/\"}"),
        ("ipv6 literal", "http://[::1]:8080/\"}"),
    ];
    for (label, tail) in cases {
        let error =
            write_and_load(label, &format!("{base}{tail}")).expect_err("an invalid URL must fail");
        assert!(
            error.message.contains("invalid http URL") || error.message.contains("not supported"),
            "{label}: {}",
            error.message
        );
    }
}

#[test]
fn tcp_readiness_parses_with_bounded_defaults() {
    let project = write_and_load(
        "readiness-tcp-ok",
        "version: 1\nprocesses:\n  - name: db\n    command: {program: /bin/sleep, args: [\"1\"]}\n    ready:\n      tcp:\n        host: 127.0.0.1\n        port: 5432\n",
    )
    .expect("valid tcp readiness");
    let readiness = project.processes()[0]
        .readiness
        .clone()
        .expect("readiness parses");
    let check = &readiness.checks[0];
    assert_eq!(
        check.probe,
        ReadinessProbe::Tcp {
            host: "127.0.0.1".into(),
            port: 5432
        }
    );
    assert_eq!(check.interval, Duration::from_millis(1_000));
    assert_eq!(check.timeout, Duration::from_millis(2_000));
}

#[test]
fn tcp_readiness_accepts_common_fields_and_every_duration_unit() {
    let project = write_and_load(
        "readiness-tcp-fields",
        "version: 1\nprocesses:\n  - name: db\n    command: {program: /bin/sleep, args: [\"1\"]}\n    ready:\n      tcp: {host: localhost, port: 1}\n      initial_delay: 250ms\n      interval: 2s\n      timeout: 3m\n      success_threshold: 2\n      failure_threshold: 3\n      startup_timeout: 4h\n",
    )
    .expect("valid common readiness fields");
    let readiness = project.processes()[0]
        .readiness
        .clone()
        .expect("readiness parses");
    let check = &readiness.checks[0];
    assert_eq!(check.initial_delay, Duration::from_millis(250));
    assert_eq!(check.interval, Duration::from_secs(2));
    assert_eq!(check.timeout, Duration::from_secs(3 * 60));
    assert_eq!(check.success_threshold, 2);
    assert_eq!(check.failure_threshold, 3);
    assert_eq!(
        readiness.startup_timeout,
        Some(Duration::from_secs(4 * 60 * 60))
    );
}

#[test]
fn initial_delay_may_be_zero() {
    let project = write_and_load(
        "readiness-zero-delay",
        "version: 1\nprocesses:\n  - name: db\n    command: {program: /bin/true}\n    ready:\n      tcp: {host: h, port: 1}\n      initial_delay: 0s\n",
    )
    .expect("zero initial delay is valid");
    assert_eq!(
        project.processes()[0]
            .readiness
            .as_ref()
            .expect("readiness parses")
            .checks[0]
            .initial_delay,
        Duration::ZERO
    );
}

#[test]
fn invalid_readiness_values_are_rejected_clearly() {
    let base = "version: 1\nprocesses:\n  - name: db\n    command: {program: /bin/true}\n    ready:\n      tcp: {host: h, port: 1}\n";
    let cases = [
        ("zero interval", "      interval: 0s\n", "interval"),
        ("zero timeout", "      timeout: 0s\n", "timeout"),
        (
            "zero startup timeout",
            "      startup_timeout: 0s\n",
            "startup_timeout",
        ),
        (
            "zero success threshold",
            "      success_threshold: 0\n",
            "success_threshold",
        ),
        (
            "zero failure threshold",
            "      failure_threshold: 0\n",
            "failure_threshold",
        ),
        ("duration without suffix", "      interval: 1\n", "suffix"),
        ("negative duration", "      timeout: '-1s'\n", "nonnegative"),
        (
            "fractional duration",
            "      timeout: 1.5s\n",
            "whole number",
        ),
        (
            "unknown scheduling field",
            "      attempts: 1\n",
            "unknown field",
        ),
        (
            "unknown check field",
            "      http: {url: 'http://h/', mode: fast}\n",
            "unknown field",
        ),
        (
            "both forms",
            "      http: {url: 'http://h/'}\n",
            "exactly one",
        ),
    ];
    for (label, block, expected) in cases {
        let error = write_and_load(label, &format!("{base}{block}"))
            .expect_err("an invalid readiness block must fail");
        assert!(
            error.message.contains(expected),
            "{label}: {}",
            error.message
        );
    }
    for (label, block, expected) in [
        ("port zero", "      tcp: {host: h, port: 0}\n", "port"),
        ("empty host", "      tcp: {host: '', port: 1}\n", "host"),
        ("no form", "      tcp: null\n", "exactly one"),
    ] {
        let yaml = format!(
            "version: 1\nprocesses:\n  - name: db\n    command: {{program: /bin/true}}\n    ready:\n{block}"
        );
        let error = write_and_load(label, &yaml).expect_err("the block must fail");
        assert!(error.message.contains(expected), "{label}: {error}");
    }
}

#[test]
fn duration_overflow_is_rejected() {
    let error = write_and_load(
        "readiness-duration-overflow",
        "version: 1\nprocesses:\n  - name: db\n    command: {program: /bin/true}\n    ready:\n      tcp: {host: h, port: 1}\n      startup_timeout: 18446744073709551616h\n",
    )
    .expect_err("an overflowing duration must fail");
    assert!(error.message.contains("overflows"), "{error}");
}

#[test]
fn removed_readiness_spellings_name_the_replacements() {
    let old_block = write_and_load(
        "removed-readiness-block",
        "version: 1\nprocesses:\n  - name: db\n    command: {program: /bin/true}\n    readiness:\n      tcp: {host: h, port: 1}\n",
    )
    .expect_err("the temporary block name must be rejected");
    assert!(
        old_block.message.contains("unknown field `readiness`"),
        "{old_block}"
    );
    assert!(
        old_block.message.contains("use `ready` instead"),
        "{old_block}"
    );

    let old_interval = write_and_load(
        "removed-interval-field",
        "version: 1\nprocesses:\n  - name: db\n    command: {program: /bin/true}\n    ready:\n      tcp: {host: h, port: 1}\n      interval_ms: 1s\n",
    )
    .expect_err("interval_ms must be rejected");
    assert!(
        old_interval.message.contains("unknown field `interval_ms`"),
        "{old_interval}"
    );
    assert!(
        old_interval.message.contains("use `interval` instead"),
        "{old_interval}"
    );

    let old_timeout = write_and_load(
        "removed-timeout-field",
        "version: 1\nprocesses:\n  - name: db\n    command: {program: /bin/true}\n    ready:\n      tcp: {host: h, port: 1}\n      timeout_ms: 1s\n",
    )
    .expect_err("timeout_ms must be rejected");
    assert!(
        old_timeout.message.contains("unknown field `timeout_ms`"),
        "{old_timeout}"
    );
    assert!(
        old_timeout.message.contains("use `timeout` instead"),
        "{old_timeout}"
    );
}

#[test]
fn all_readiness_parses_independent_children_and_one_parent_deadline() {
    let project = write_and_load(
        "readiness-all-ok",
        "version: 1\nprocesses:\n  - name: api\n    command: {program: /bin/sleep, args: [\"1\"]}\n    ready:\n      all:\n        - tcp: {host: localhost, port: 1}\n          initial_delay: 250ms\n          interval: 2s\n          timeout: 3s\n          success_threshold: 2\n          failure_threshold: 3\n        - http: {url: \"http://example.test/health\"}\n          initial_delay: 4ms\n          interval: 5s\n          timeout: 6s\n          success_threshold: 3\n          failure_threshold: 4\n      startup_timeout: 1m\n",
    )
    .expect("valid all readiness");
    let readiness = project.processes()[0]
        .readiness
        .as_ref()
        .expect("readiness parses");
    assert_eq!(readiness.checks.len(), 2);
    assert_eq!(readiness.startup_timeout, Some(Duration::from_secs(60)));
    assert_eq!(
        readiness.checks[0].initial_delay,
        Duration::from_millis(250)
    );
    assert_eq!(readiness.checks[0].interval, Duration::from_secs(2));
    assert_eq!(readiness.checks[0].timeout, Duration::from_secs(3));
    assert_eq!(readiness.checks[0].success_threshold, 2);
    assert_eq!(readiness.checks[0].failure_threshold, 3);
    assert_eq!(
        readiness.checks[1].probe,
        ReadinessProbe::Http {
            host: "example.test".into(),
            port: 80,
            path: "/health".into(),
        }
    );
    assert_eq!(readiness.checks[1].initial_delay, Duration::from_millis(4));
    assert_eq!(readiness.checks[1].interval, Duration::from_secs(5));
    assert_eq!(readiness.checks[1].timeout, Duration::from_secs(6));
}

#[test]
fn all_readiness_rejects_invalid_composite_forms_clearly() {
    let base =
        "version: 1\nprocesses:\n  - name: api\n    command: {program: /bin/true}\n    ready:\n";
    let cases = [
        ("empty", "      all: []\n", "at least two child checks"),
        (
            "one child",
            "      all:\n        - tcp: {host: h, port: 1}\n",
            "at least two child checks",
        ),
        (
            "nested",
            "      all:\n        - all:\n            - tcp: {host: h, port: 1}\n            - tcp: {host: h, port: 2}\n        - tcp: {host: h, port: 3}\n",
            "nested 'all'",
        ),
        (
            "any",
            "      any:\n        - tcp: {host: h, port: 1}\n        - tcp: {host: h, port: 2}\n",
            "'any' readiness form is not supported",
        ),
        (
            "parent scheduling",
            "      all:\n        - tcp: {host: h, port: 1}\n        - tcp: {host: h, port: 2}\n      interval: 1s\n",
            "on each child",
        ),
    ];
    for (label, block, expected) in cases {
        let error = write_and_load(label, &format!("{base}{block}"))
            .expect_err("an invalid all readiness block must fail");
        assert!(error.message.contains(expected), "{label}: {error}");
    }
}

#[test]
fn all_readiness_rejects_child_startup_deadlines() {
    let error = write_and_load(
        "readiness-all-child-timeout",
        "version: 1\nprocesses:\n  - name: api\n    command: {program: /bin/true}\n    ready:\n      all:\n        - tcp: {host: h, port: 1}\n          startup_timeout: 1s\n        - tcp: {host: h, port: 2}\n",
    )
    .expect_err("a child cannot own the composite startup deadline");
    assert!(error.message.contains("parent 'ready' block"), "{error}");
}

#[test]
fn exec_readiness_parses_direct_and_shell_commands_with_overrides() {
    let direct = write_and_load(
        "exec-direct",
        "version: 1\nprocesses:\n  - name: web\n    working_dir: /tmp\n    env: {BASE: process}\n    command: {program: /bin/sleep, args: [\"1\"]}\n    ready:\n      exec:\n        command: {program: /usr/bin/test, args: [\"-f\", \"ready\"]}\n        working_dir: /tmp\n        env: {CHECK: probe}\n        success_exit_codes: [0, 2]\n",
    )
    .expect("valid direct exec readiness");
    let probe = &direct.processes()[0]
        .readiness
        .as_ref()
        .expect("readiness parses")
        .checks[0]
        .probe;
    assert_eq!(
        probe,
        &ReadinessProbe::Exec {
            command: crate::model::CommandForm::Direct {
                program: "/usr/bin/test".into(),
                args: vec!["-f".into(), "ready".into()],
            },
            working_dir: Some(PathBuf::from("/tmp")),
            env: vec![("CHECK".into(), "probe".into())],
            success_exit_codes: vec![0, 2],
        }
    );

    let shell = write_and_load(
        "exec-shell",
        "version: 1\nprocesses:\n  - name: web\n    command: {program: /bin/sleep, args: [\"1\"]}\n    ready:\n      exec:\n        command: {shell: \"test \\\"$CHECK\\\" = probe\"}\n        env: {CHECK: probe}\n",
    )
    .expect("valid shell exec readiness");
    let probe = &shell.processes()[0]
        .readiness
        .as_ref()
        .expect("readiness parses")
        .checks[0]
        .probe;
    assert_eq!(
        probe,
        &ReadinessProbe::Exec {
            command: crate::model::CommandForm::Shell {
                text: "test \"$CHECK\" = probe".to_string(),
            },
            working_dir: None,
            env: vec![("CHECK".into(), "probe".into())],
            success_exit_codes: vec![0],
        }
    );
}

#[test]
fn exec_readiness_rejects_missing_invalid_and_unusable_configuration() {
    let base = "version: 1\nprocesses:\n  - name: web\n    command: {program: /bin/true}\n    ready:\n      exec:\n";
    let cases = [
        ("missing-command", "        working_dir: /tmp\n", "requires"),
        (
            "both-command-forms",
            "        command: {program: /bin/true, shell: 'true'}\n",
            "exactly one",
        ),
        (
            "zero-success-codes",
            "        command: {program: /bin/true}\n        success_exit_codes: []\n",
            "at least one",
        ),
        (
            "duplicate-success-codes",
            "        command: {program: /bin/true}\n        success_exit_codes: [0, 0]\n",
            "repeated",
        ),
        (
            "out-of-range-success-code",
            "        command: {program: /bin/true}\n        success_exit_codes: [256]\n",
            "0 through 255",
        ),
        (
            "missing-working-directory",
            "        command: {program: /bin/true}\n        working_dir: /path/that/does/not/exist\n",
            "working directory",
        ),
    ];
    for (label, block, expected) in cases {
        let error = write_and_load(label, &format!("{base}{block}"))
            .expect_err("invalid exec readiness must fail");
        assert!(error.message.contains(expected), "{label}: {error}");
    }
}

#[test]
fn all_readiness_accepts_exec_as_one_independent_child() {
    let project = write_and_load(
        "readiness-all-exec",
        "version: 1\nprocesses:\n  - name: api\n    command: {program: /bin/sleep, args: [\"1\"]}\n    ready:\n      all:\n        - exec:\n            command: {program: /bin/true}\n        - tcp: {host: localhost, port: 1}\n",
    )
    .expect("exec can be an all child");
    let checks = &project.processes()[0]
        .readiness
        .as_ref()
        .expect("readiness parses")
        .checks;
    assert_eq!(checks.len(), 2);
    assert!(matches!(checks[0].probe, ReadinessProbe::Exec { .. }));
}

#[test]
fn log_readiness_accepts_one_nonempty_literal() {
    let project = write_and_load(
        "readiness-log-ok",
        "version: 1\nprocesses:\n  - name: web\n    command: {program: /bin/sleep, args: [\"1\"]}\n    ready:\n      log:\n        contains: \"Listening on :8080\"\n",
    )
    .expect("valid log readiness");
    let check = &project.processes()[0]
        .readiness
        .as_ref()
        .expect("readiness parses")
        .checks[0];
    assert_eq!(
        check.probe,
        ReadinessProbe::Log {
            contains: "Listening on :8080".into(),
        }
    );
    assert_eq!(check.initial_delay, Duration::ZERO);
    assert_eq!(check.interval, Duration::from_secs(1));
}

#[test]
fn log_readiness_rejects_empty_literals_and_regex_options() {
    let empty = write_and_load(
        "readiness-log-empty",
        "version: 1\nprocesses:\n  - name: web\n    command: {program: /bin/true}\n    ready:\n      log: {contains: \"\"}\n",
    )
    .expect_err("an empty log literal must fail");
    assert!(
        empty.message.contains("log contains must not be empty"),
        "{empty}"
    );

    let regex = write_and_load(
        "readiness-log-regex",
        "version: 1\nprocesses:\n  - name: web\n    command: {program: /bin/true}\n    ready:\n      log: {contains: ready, regex: true}\n",
    )
    .expect_err("regex log configuration must fail");
    assert!(regex.message.contains("unknown field `regex`"), "{regex}");
}

#[test]
fn all_readiness_accepts_log_as_one_independent_child() {
    let project = write_and_load(
        "readiness-all-log",
        "version: 1\nprocesses:\n  - name: web\n    command: {program: /bin/sleep, args: [\"1\"]}\n    ready:\n      all:\n        - log: {contains: ready}\n        - tcp: {host: localhost, port: 1}\n",
    )
    .expect("log can be an all child");
    let checks = &project.processes()[0]
        .readiness
        .as_ref()
        .expect("readiness parses")
        .checks;
    assert_eq!(checks.len(), 2);
    assert_eq!(
        checks[0].probe,
        ReadinessProbe::Log {
            contains: "ready".into()
        }
    );
    assert_eq!(
        checks[1].probe,
        ReadinessProbe::Tcp {
            host: "localhost".into(),
            port: 1
        }
    );
}

#[test]
fn readiness_on_a_one_shot_is_rejected() {
    let error = write_and_load(
        "readiness-one-shot",
        "version: 1\nprocesses:\n  - name: setup\n    kind: one-shot\n    command: {program: /bin/true}\n    ready:\n      tcp: {host: 127.0.0.1, port: 1}\n",
    )
    .expect_err("a One-shot must reject readiness");
    assert!(error.message.contains("Services"), "{}", error.message);
}
