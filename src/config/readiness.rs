//! Readiness configuration: one YAML `ready` block becomes one validated
//! [`ReadinessConfig`] or a clear per-Process failure. The block carries
//! exactly one TCP or HTTP probe and the common scheduling fields.

use std::time::Duration;

use serde::Deserialize;

use crate::model::{ReadinessConfig, ReadinessProbe};

use super::ConfigError;

const DURATION_FORMAT_ERROR: &str = "use a nonnegative whole number with an ms, s, m, or h suffix";
const DEFAULT_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_SUCCESS_THRESHOLD: u32 = 1;
const DEFAULT_FAILURE_THRESHOLD: u32 = 1;

/// One `ready` block: exactly one probe form and optional common scheduling
/// fields.
pub(super) fn build_readiness(
    process_name: &str,
    file: &ReadinessFile,
) -> Result<ReadinessConfig, ConfigError> {
    let fail = |detail: String| Err(ready_error(process_name, detail));
    let probe = match (&file.tcp, &file.http) {
        (Some(tcp), None) => {
            if tcp.host.is_empty() {
                return fail("tcp host must not be empty".to_string());
            }
            if tcp.port == 0 {
                return fail("tcp port must be between 1 and 65535".to_string());
            }
            ReadinessProbe::Tcp {
                host: tcp.host.clone(),
                port: tcp.port,
            }
        }
        (None, Some(http)) => {
            let (host, port, path) =
                parse_http_url(&http.url).map_err(|detail| ready_error(process_name, detail))?;
            ReadinessProbe::Http { host, port, path }
        }
        (Some(_), Some(_)) | (None, None) => {
            return fail("define exactly one of 'tcp' or 'http'".to_string());
        }
    };
    let initial_delay = duration_or_default(
        process_name,
        "initial_delay",
        file.initial_delay.as_deref(),
        Duration::ZERO,
    )?;
    let interval = positive_duration(
        process_name,
        "interval",
        file.interval.as_deref(),
        DEFAULT_INTERVAL,
    )?;
    let timeout = positive_duration(
        process_name,
        "timeout",
        file.timeout.as_deref(),
        DEFAULT_TIMEOUT,
    )?;
    let startup_timeout = file
        .startup_timeout
        .as_deref()
        .map(|value| {
            positive_duration(process_name, "startup_timeout", Some(value), Duration::ZERO)
        })
        .transpose()?;
    let success_threshold = configured_threshold(
        process_name,
        "success_threshold",
        file.success_threshold,
        DEFAULT_SUCCESS_THRESHOLD,
    )?;
    let failure_threshold = configured_threshold(
        process_name,
        "failure_threshold",
        file.failure_threshold,
        DEFAULT_FAILURE_THRESHOLD,
    )?;
    Ok(ReadinessConfig {
        probe,
        initial_delay,
        interval,
        timeout,
        success_threshold,
        failure_threshold,
        startup_timeout,
    })
}

fn ready_error(process_name: &str, detail: impl Into<String>) -> ConfigError {
    ConfigError {
        message: format!("Process '{process_name}': ready: {}", detail.into()),
    }
}

fn duration_or_default(
    process_name: &str,
    field: &str,
    value: Option<&str>,
    default: Duration,
) -> Result<Duration, ConfigError> {
    value
        .map(|value| {
            parse_duration(value)
                .map_err(|detail| ready_error(process_name, format!("{field}: {detail}")))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn positive_duration(
    process_name: &str,
    field: &str,
    value: Option<&str>,
    default: Duration,
) -> Result<Duration, ConfigError> {
    let duration = duration_or_default(process_name, field, value, default)?;
    if duration.is_zero() {
        return Err(ready_error(
            process_name,
            format!("{field} must be greater than zero"),
        ));
    }
    Ok(duration)
}

fn configured_threshold(
    process_name: &str,
    field: &str,
    value: Option<u32>,
    default: u32,
) -> Result<u32, ConfigError> {
    let threshold = value.unwrap_or(default);
    if threshold == 0 {
        return Err(ready_error(
            process_name,
            format!("{field} must be a positive whole number"),
        ));
    }
    Ok(threshold)
}

/// Parse a nonnegative whole-number duration with one supported suffix.
fn parse_duration(value: &str) -> Result<Duration, String> {
    let (digits, nanos_per_unit) = if let Some(digits) = value.strip_suffix("ms") {
        (digits, 1_000_000_u128)
    } else if let Some(digits) = value.strip_suffix('s') {
        (digits, 1_000_000_000_u128)
    } else if let Some(digits) = value.strip_suffix('m') {
        (digits, 60_u128 * 1_000_000_000)
    } else if let Some(digits) = value.strip_suffix('h') {
        (digits, 60_u128 * 60 * 1_000_000_000)
    } else {
        return Err(DURATION_FORMAT_ERROR.to_string());
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DURATION_FORMAT_ERROR.to_string());
    }
    let amount = digits
        .parse::<u128>()
        .map_err(|_| "the duration overflows the supported range".to_string())?;
    let nanos = amount
        .checked_mul(nanos_per_unit)
        .ok_or_else(|| "the duration overflows the supported range".to_string())?;
    let seconds = nanos / 1_000_000_000;
    if seconds > u64::MAX as u128 {
        return Err("the duration overflows the supported range".to_string());
    }
    Ok(Duration::new(
        seconds as u64,
        (nanos % 1_000_000_000) as u32,
    ))
}

/// Parse one plain-`http` readiness URL into its connect target: host,
/// port (defaulting to 80), and request path (defaulting to `/`). Only
/// what a raw HTTP/1.0 GET can reach is accepted; anything else fails with
/// one clear reason.
fn parse_http_url(url: &str) -> Result<(String, u16, String), String> {
    let Some(rest) = url.strip_prefix("http://") else {
        return Err(if url.starts_with("https://") {
            "https URLs are not supported for readiness probes; use a plain http URL".to_string()
        } else {
            format!("invalid http URL '{url}': it must start with 'http://'")
        });
    };
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() || authority.starts_with('[') || authority.contains('@') {
        return Err(format!(
            "invalid http URL '{url}': the host is missing or unsupported"
        ));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port_text)) => {
            let Ok(port) = port_text.parse::<u16>() else {
                return Err(format!(
                    "invalid http URL '{url}': the port must be between 1 and 65535"
                ));
            };
            if port == 0 {
                return Err(format!(
                    "invalid http URL '{url}': the port must be between 1 and 65535"
                ));
            }
            (host.to_string(), port)
        }
        None => (authority.to_string(), 80),
    };
    if host.is_empty() {
        return Err(format!("invalid http URL '{url}': the host is empty"));
    }
    Ok((host, port, path))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadinessFile {
    tcp: Option<TcpProbeFile>,
    http: Option<HttpProbeFile>,
    initial_delay: Option<String>,
    interval: Option<String>,
    timeout: Option<String>,
    success_threshold: Option<u32>,
    failure_threshold: Option<u32>,
    startup_timeout: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TcpProbeFile {
    host: String,
    port: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HttpProbeFile {
    url: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load;
    use std::fs;

    fn write_and_load(
        label: &str,
        yaml: &str,
    ) -> Result<crate::model::EffectiveProject, ConfigError> {
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
        assert_eq!(
            readiness.probe,
            ReadinessProbe::Http {
                host: "localhost".into(),
                port: 8080,
                path: "/healthz?probe=1".into(),
            }
        );
        assert_eq!(readiness.interval, Duration::from_millis(1_000));
        assert_eq!(readiness.timeout, Duration::from_millis(2_000));

        // The default port and path come from the URL.
        let bare = write_and_load(
            "readiness-http-bare",
            "version: 1\nprocesses:\n  - name: web\n    command: {program: /bin/true}\n    ready:\n      http: {url: \"http://example.test\"}\n",
        )
        .expect("a bare http URL is valid");
        let readiness = bare.processes()[0].readiness.clone().expect("parses");
        assert_eq!(
            readiness.probe,
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
            let error = write_and_load(label, &format!("{base}{tail}"))
                .expect_err("an invalid URL must fail");
            assert!(
                error.message.contains("invalid http URL")
                    || error.message.contains("not supported"),
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
        assert_eq!(
            readiness.probe,
            ReadinessProbe::Tcp {
                host: "127.0.0.1".into(),
                port: 5432
            }
        );
        assert_eq!(readiness.interval, Duration::from_millis(1_000));
        assert_eq!(readiness.timeout, Duration::from_millis(2_000));
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
        assert_eq!(readiness.initial_delay, Duration::from_millis(250));
        assert_eq!(readiness.interval, Duration::from_secs(2));
        assert_eq!(readiness.timeout, Duration::from_secs(3 * 60));
        assert_eq!(readiness.success_threshold, 2);
        assert_eq!(readiness.failure_threshold, 3);
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
    fn readiness_on_a_one_shot_is_rejected() {
        let error = write_and_load(
            "readiness-one-shot",
            "version: 1\nprocesses:\n  - name: setup\n    kind: one-shot\n    command: {program: /bin/true}\n    ready:\n      tcp: {host: 127.0.0.1, port: 1}\n",
        )
        .expect_err("a One-shot must reject readiness");
        assert!(error.message.contains("Services"), "{}", error.message);
    }
}
