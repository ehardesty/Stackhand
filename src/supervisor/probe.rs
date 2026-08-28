//! The production readiness probe adapter. It performs one bounded TCP
//! connection or one bounded HTTP GET per request on its own worker thread
//! and reports exactly one [`SeamEvent::Readiness`] for the requested
//! identities. Network waits and socket errors never touch the Supervisor
//! control task.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::model::ReadinessProbe;
use crate::runtime::{ProcessId, RunId};
use crate::supervisor::seam::{AttemptId, ProbeIntent, ProbeSeam, SeamEvent, SeamSender, WorkId};

/// Longest allowed diagnostic string; adapter error text stays bounded.
const MAX_DIAGNOSTIC_CHARS: usize = 200;
/// Largest response prefix read for status parsing; response bodies are
/// never retained.
const HTTP_RESPONSE_CAP_BYTES: u64 = 1024;

type ProbeKey = (ProcessId, RunId, WorkId, AttemptId);

/// Performs real network readiness attempts off the Supervisor control
/// task. Cancellation closes the logical admission gate immediately; the
/// bounded socket operation may still take its configured timeout to return.
pub(crate) struct RealProbes {
    active: Arc<Mutex<HashMap<ProbeKey, Arc<AtomicBool>>>>,
}

impl Default for RealProbes {
    fn default() -> Self {
        Self {
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl ProbeSeam for RealProbes {
    fn probe(&self, intent: ProbeIntent, events: &SeamSender) {
        let key = (
            intent.process_id,
            intent.run_id,
            intent.work_id,
            intent.attempt_id,
        );
        let canceled = Arc::new(AtomicBool::new(false));
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, Arc::clone(&canceled));
        let active = Arc::clone(&self.active);
        let events = events.clone();
        std::thread::Builder::new()
            .name("readiness-probe".to_string())
            .spawn(move || {
                if !canceled.load(Ordering::Acquire) {
                    let attempt = match &intent.probe {
                        ReadinessProbe::Tcp { host, port } => {
                            tcp_attempt(host, *port, intent.timeout)
                        }
                        ReadinessProbe::Http { host, port, path } => {
                            http_attempt(host, *port, path, intent.timeout)
                        }
                        ReadinessProbe::Exec { .. } => super::exec::attempt(&intent, &canceled),
                    };
                    if !canceled.load(Ordering::Acquire) {
                        let (passing, diagnostic) = match attempt {
                            Ok(()) => (true, None),
                            Err(diagnostic) => (false, Some(diagnostic)),
                        };
                        events.send(SeamEvent::Readiness {
                            process_id: intent.process_id,
                            run_id: intent.run_id,
                            work_id: intent.work_id,
                            attempt_id: intent.attempt_id,
                            passing,
                            diagnostic,
                        });
                    }
                }
                active
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&key);
            })
            .expect("readiness probe thread spawns with valid configuration");
    }

    fn cancel(&self, process_id: ProcessId, run_id: RunId, work_id: WorkId) {
        let active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for ((process, run, work, _), canceled) in active.iter() {
            if *process == process_id && *run == run_id && *work == work_id {
                canceled.store(true, Ordering::Release);
            }
        }
    }
}

/// One bounded TCP readiness attempt: resolve the endpoint and connect
/// within `timeout`.
pub(crate) fn tcp_attempt(host: &str, port: u16, timeout: Duration) -> Result<(), String> {
    connect(host, port, timeout).map(|_| ())
}

/// One bounded HTTP readiness attempt: connect, send one HTTP/1.0 `GET`,
/// and require a 2xx status line within `timeout`. Redirects are never
/// followed; at most [`HTTP_RESPONSE_CAP_BYTES`] are ever read.
pub(crate) fn http_attempt(
    host: &str,
    port: u16,
    path: &str,
    timeout: Duration,
) -> Result<(), String> {
    let mut stream = connect(host, port, timeout)?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| describe_io(&error, timeout))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| describe_io(&error, timeout))?;
    // HTTP/1.0 closes after one response, so no keep-alive bookkeeping is
    // needed; Connection: close keeps 1.1 servers honest too.
    let request = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|error| describe_io(&error, timeout))?;
    let mut reader = stream.take(HTTP_RESPONSE_CAP_BYTES);
    let mut head = Vec::new();
    loop {
        let mut chunk = [0u8; 256];
        let read = reader
            .read(&mut chunk)
            .map_err(|error| describe_io(&error, timeout))?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..read]);
        if head.contains(&b'\n') || head.len() as u64 >= HTTP_RESPONSE_CAP_BYTES {
            break;
        }
    }
    parse_status_line(&head)
}

/// Require a valid status line with a successful 2xx code. Only the first
/// line is inspected; whatever else arrived inside the read cap is ignored.
fn parse_status_line(head: &[u8]) -> Result<(), String> {
    let line = head.split(|byte| *byte == b'\n').next().unwrap_or_default();
    let line = String::from_utf8_lossy(line);
    let mut parts = line.split_whitespace();
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/") {
        return Err("invalid HTTP response".to_string());
    }
    let status = parts
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| "invalid HTTP response".to_string())?;
    if (200..300).contains(&status) {
        return Ok(());
    }
    if (300..400).contains(&status) {
        return Err(format!("status {status} (redirects are not followed)"));
    }
    Err(format!("status {status}"))
}

/// Resolve the endpoint and connect within `timeout`.
fn connect(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, String> {
    let address = format!("{host}:{port}")
        .to_socket_addrs()
        .map_err(|error| bound(format!("could not resolve '{host}': {error}")))?
        .next()
        .ok_or_else(|| format!("host '{host}' resolved to no addresses"))?;
    TcpStream::connect_timeout(&address, timeout).map_err(|error| describe_io(&error, timeout))
}

/// Map one socket error onto a short diagnostic; timeouts get a uniform
/// message that names the configured budget.
fn describe_io(error: &std::io::Error, timeout: Duration) -> String {
    match error.kind() {
        ErrorKind::TimedOut | ErrorKind::WouldBlock => {
            format!("timed out after {} ms", timeout.as_millis())
        }
        _ => bound(error.to_string()),
    }
}

fn bound(diagnostic: String) -> String {
    if diagnostic.chars().count() <= MAX_DIAGNOSTIC_CHARS {
        return diagnostic;
    }
    let truncated: String = diagnostic.chars().take(MAX_DIAGNOSTIC_CHARS).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{ProcessId, RunId};
    use std::io::Write as IoWrite;
    use std::net::TcpListener;

    const TIMEOUT: Duration = Duration::from_secs(2);

    /// Bind one real listener on an ephemeral localhost port and serve it
    /// on a detached thread; each connection is accepted and closed.
    fn spawn_listener() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener binds");
        let port = listener.local_addr().expect("local address").port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                drop(stream);
            }
        });
        port
    }

    /// Bind and immediately release one ephemeral port; nothing listens
    /// there any more, so a connect attempt is refused.
    fn refused_port() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener binds");
        listener.local_addr().expect("local address").port()
    }

    /// Serve exactly one hand-rolled HTTP response from a detached thread
    /// and return the ephemeral port it listened on.
    fn spawn_http_server(response: &'static [u8]) -> u16 {
        serve_one(response, Duration::ZERO)
    }

    /// Like [`spawn_http_server`], but the server accepts first and waits
    /// before responding (or never responds while it holds the socket).
    fn serve_one(response: &'static [u8], respond_after: Duration) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener binds");
        let port = listener.local_addr().expect("local address").port();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            if !respond_after.is_zero() {
                std::thread::sleep(respond_after);
            }
            let _ = stream.write_all(response);
        });
        port
    }

    /// Serve one response per connection so a single real endpoint can move
    /// from failing to passing across successive readiness attempts.
    fn serve_responses(responses: Vec<&'static [u8]>) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener binds");
        let port = listener.local_addr().expect("local address").port();
        std::thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.write_all(response);
            }
        });
        port
    }

    #[test]
    fn tcp_attempt_passes_against_a_real_listener() {
        let port = spawn_listener();
        tcp_attempt("127.0.0.1", port, TIMEOUT).expect("a live listener passes");
    }

    #[test]
    fn tcp_refusal_fails_with_a_bounded_diagnostic() {
        let port = refused_port();
        let error = tcp_attempt("127.0.0.1", port, TIMEOUT).expect_err("a closed port fails");
        assert!(!error.is_empty());
        assert!(error.chars().count() <= MAX_DIAGNOSTIC_CHARS);
    }

    #[test]
    fn an_unresolvable_host_fails_with_a_bounded_diagnostic() {
        let error = tcp_attempt("no-such-host.invalid", 80, Duration::from_millis(100))
            .expect_err("an unknown host fails");
        assert!(error.contains("no-such-host.invalid"), "{error}");
    }

    #[test]
    fn http_2xx_passes_and_ignores_a_large_body() {
        // A body far larger than the read cap proves the probe stops at the
        // status line and never retains response data.
        let mut response = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\n".to_vec();
        response.extend(std::iter::repeat_n(b'x', 256 * 1024));
        let response: &'static [u8] = Box::leak(response.into_boxed_slice());
        let port = spawn_http_server(response);
        http_attempt("127.0.0.1", port, "/healthz", TIMEOUT).expect("a 2xx passes");
    }

    #[test]
    fn http_non_2xx_fails_with_the_status_in_its_diagnostic() {
        let port = spawn_http_server(b"HTTP/1.0 500 Internal Server Error\r\n\r\n");
        let error = http_attempt("127.0.0.1", port, "/healthz", TIMEOUT).expect_err("a 5xx fails");
        assert_eq!(error, "status 500");
    }

    #[test]
    fn http_redirects_are_not_followed() {
        let port = spawn_http_server(b"HTTP/1.0 302 Found\r\nLocation: /elsewhere\r\n\r\n");
        let error = http_attempt("127.0.0.1", port, "/healthz", TIMEOUT)
            .expect_err("a redirect fails without being followed");
        assert!(
            error.contains("302") && error.contains("not followed"),
            "{error}"
        );
    }

    #[test]
    fn http_connection_refusal_fails_with_a_bounded_diagnostic() {
        let port = refused_port();
        let error = http_attempt("127.0.0.1", port, "/", TIMEOUT).expect_err("refusal fails");
        assert!(!error.is_empty());
        assert!(error.chars().count() <= MAX_DIAGNOSTIC_CHARS);
    }

    #[test]
    fn http_attempt_covers_failure_timeout_and_recovery_on_real_endpoints() {
        let failed_port = spawn_http_server(b"HTTP/1.0 503 Unavailable\r\n\r\n");
        let failure = http_attempt("127.0.0.1", failed_port, "/healthz", TIMEOUT)
            .expect_err("a failing endpoint reports failure");
        assert_eq!(failure, "status 503");

        let silent_listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener binds");
        let silent_port = silent_listener.local_addr().expect("local address").port();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = silent_listener.accept() {
                std::thread::sleep(Duration::from_secs(2));
                drop(stream);
            }
        });
        let timeout = http_attempt(
            "127.0.0.1",
            silent_port,
            "/healthz",
            Duration::from_millis(50),
        )
        .expect_err("a silent endpoint times out");
        assert!(timeout.contains("timed out"), "{timeout}");

        let recovering_port = serve_responses(vec![
            b"HTTP/1.0 503 Unavailable\r\n\r\n",
            b"HTTP/1.0 200 OK\r\n\r\n",
        ]);
        http_attempt("127.0.0.1", recovering_port, "/healthz", TIMEOUT)
            .expect_err("the first recovery attempt fails");
        http_attempt("127.0.0.1", recovering_port, "/healthz", TIMEOUT)
            .expect("the next attempt recovers");
    }

    #[test]
    fn http_read_timeout_fails_quickly_with_a_timeout_diagnostic() {
        // The server accepts but stays silent; the configured budget must
        // end the attempt with a timeout diagnostic.
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener binds");
        let port = listener.local_addr().expect("local address").port();
        std::thread::spawn(move || {
            // Keep the accepted socket open and silent; dropping it early
            // would send a reset instead of letting the probe time out.
            if let Ok((stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(2));
                drop(stream);
            }
        });
        let started = std::time::Instant::now();
        let error = http_attempt("127.0.0.1", port, "/", Duration::from_millis(50))
            .expect_err("silence times out");
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1), "must fail fast");
    }

    #[test]
    fn http_garbage_responses_fail_as_invalid() {
        let port = spawn_http_server(b"hello, is anybody there\r\n");
        let error = http_attempt("127.0.0.1", port, "/", TIMEOUT).expect_err("garbage fails");
        assert_eq!(error, "invalid HTTP response");
    }

    #[test]
    fn the_probe_seam_reports_one_event_for_an_http_request() {
        let port = spawn_http_server(b"HTTP/1.0 200 OK\r\n\r\n");
        let (tx, rx) = crossbeam_channel::unbounded();
        RealProbes::default().probe(
            ProbeIntent {
                process_id: ProcessId::new(7),
                run_id: RunId::new(3),
                work_id: WorkId::new(1),
                attempt_id: crate::supervisor::seam::AttemptId::new(1),
                probe: ReadinessProbe::Http {
                    host: "127.0.0.1".into(),
                    port,
                    path: "/healthz".into(),
                },
                timeout: TIMEOUT,
                exec_context: None,
            },
            &SeamSender::new(tx),
        );
        match rx.recv_timeout(TIMEOUT) {
            Ok(SeamEvent::Readiness {
                process_id,
                run_id,
                passing,
                diagnostic,
                ..
            }) => {
                assert_eq!(process_id, ProcessId::new(7));
                assert_eq!(run_id, RunId::new(3));
                assert!(passing);
                assert_eq!(diagnostic, None);
            }
            other => panic!("expected one passing Readiness event, got {other:?}"),
        }
    }
}
