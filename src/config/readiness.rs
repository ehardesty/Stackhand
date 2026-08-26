//! Readiness configuration: one YAML `readiness` block becomes one
//! validated [`ReadinessConfig`] or a clear per-Process failure. The block
//! carries exactly one probe form plus optional bounded interval and
//! per-attempt timeout overrides.

use std::time::Duration;

use serde::Deserialize;

use crate::model::{ReadinessConfig, ReadinessProbe};

use super::ConfigError;

/// Default milliseconds between failing readiness attempts.
const DEFAULT_INTERVAL_MS: u64 = 1_000;
/// Default milliseconds one readiness attempt may take.
const DEFAULT_TIMEOUT_MS: u64 = 2_000;

/// One `readiness` block: exactly one probe form, plus optional bounded
/// interval and per-attempt timeout overrides.
pub(super) fn build_readiness(
    process_name: &str,
    file: &ReadinessFile,
) -> Result<ReadinessConfig, ConfigError> {
    let fail = |detail: String| {
        Err(ConfigError {
            message: format!("Process '{process_name}': readiness: {detail}"),
        })
    };
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
            let (host, port, path) = parse_http_url(&http.url).map_err(|detail| ConfigError {
                message: format!("Process '{process_name}': readiness: {detail}"),
            })?;
            ReadinessProbe::Http { host, port, path }
        }
        (Some(_), Some(_)) | (None, None) => {
            return fail("define exactly one of 'tcp' or 'http'".to_string());
        }
    };
    let interval = duration_ms("interval_ms", file.interval_ms, DEFAULT_INTERVAL_MS)?;
    let timeout = duration_ms("timeout_ms", file.timeout_ms, DEFAULT_TIMEOUT_MS)?;
    Ok(ReadinessConfig {
        probe,
        interval,
        timeout,
    })
}

/// Resolve one optional millisecond override to a bounded positive
/// Duration.
fn duration_ms(field: &str, value: Option<u64>, default_ms: u64) -> Result<Duration, ConfigError> {
    let millis = value.unwrap_or(default_ms);
    if millis == 0 {
        return Err(ConfigError {
            message: format!("readiness {field} must be at least 1"),
        });
    }
    Ok(Duration::from_millis(millis))
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
    interval_ms: Option<u64>,
    timeout_ms: Option<u64>,
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
            "version: 1\nprocesses:\n  - name: web\n    command: {program: /bin/sleep, args: [\"1\"]}\n    readiness:\n      http:\n        url: \"http://localhost:8080/healthz?probe=1\"\n",
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
            "version: 1\nprocesses:\n  - name: web\n    command: {program: /bin/true}\n    readiness:\n      http: {url: \"http://example.test\"}\n",
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
        let base = "version: 1\nprocesses:\n  - name: web\n    command: {program: /bin/true}\n    readiness:\n      http: {url: \"";
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
            "version: 1\nprocesses:\n  - name: db\n    command: {program: /bin/sleep, args: [\"1\"]}\n    readiness:\n      tcp:\n        host: 127.0.0.1\n        port: 5432\n",
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
    fn tcp_readiness_accepts_interval_and_timeout_overrides() {
        let project = write_and_load(
            "readiness-tcp-overrides",
            "version: 1\nprocesses:\n  - name: db\n    command: {program: /bin/sleep, args: [\"1\"]}\n    readiness:\n      tcp: {host: localhost, port: 1}\n      interval_ms: 250\n      timeout_ms: 750\n",
        )
        .expect("valid overrides");
        let readiness = project.processes()[0]
            .readiness
            .clone()
            .expect("readiness parses");
        assert_eq!(readiness.interval, Duration::from_millis(250));
        assert_eq!(readiness.timeout, Duration::from_millis(750));
    }

    #[test]
    fn invalid_readiness_blocks_are_rejected_clearly() {
        let base = "version: 1\nprocesses:\n  - name: db\n    command: {program: /bin/true}\n    readiness:\n";
        let cases = [
            (
                "both forms",
                "      tcp: {host: h, port: 1}\n      http: {url: \"http://h/\"}\n",
                "exactly one",
            ),
            ("no form", "      interval_ms: 100\n", "exactly one"),
            (
                "unknown field",
                "      tcp: {host: h, port: 1, mode: fast}\n",
                "unknown field",
            ),
            ("port zero", "      tcp: {host: h, port: 0}\n", "port"),
            ("empty host", "      tcp: {host: '', port: 1}\n", "host"),
            (
                "zero interval",
                "      tcp: {host: h, port: 1}\n      interval_ms: 0\n",
                "interval_ms",
            ),
            (
                "zero timeout",
                "      tcp: {host: h, port: 1}\n      timeout_ms: 0\n",
                "timeout_ms",
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
    }

    #[test]
    fn readiness_on_a_one_shot_is_rejected() {
        let error = write_and_load(
            "readiness-one-shot",
            "version: 1\nprocesses:\n  - name: setup\n    kind: one-shot\n    command: {program: /bin/true}\n    readiness:\n      tcp: {host: 127.0.0.1, port: 1}\n",
        )
        .expect_err("a One-shot must reject readiness");
        assert!(error.message.contains("Services"), "{}", error.message);
    }
}
