//! Optional listening-TCP-port discovery for one owned Run.
//!
//! This module owns polling, platform inspection, result bounds, and change
//! detection. Callers only start one observer and receive bounded Run events.

use std::collections::BTreeSet;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::runtime::{ProcessId, RunEvent, RunEventKind, RunId};

const EMPTY_POLL_INTERVAL: Duration = Duration::from_secs(2);
const ACTIVE_POLL_INTERVAL: Duration = Duration::from_secs(5);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(20);
pub(crate) const MAX_DISCOVERED_PORTS: usize = 32;

/// One bounded observation of listening TCP ports owned by a Run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoveredPorts {
    pub ports: Vec<u16>,
    pub omitted: u16,
    /// True when the platform could not inspect every visible Process Tree
    /// member during this observation.
    pub best_effort: bool,
}

/// The Run-owned discovery worker. Dropping an active observer is not enough
/// to stop it; the Run sets the shared stop flag before joining all workers.
pub(crate) struct PortObserver {
    handle: Option<JoinHandle<()>>,
}

impl PortObserver {
    pub(crate) fn spawn(
        root_pid: u32,
        process_id: ProcessId,
        run_id: RunId,
        stop: Arc<AtomicBool>,
        events: Sender<RunEvent>,
    ) -> Self {
        let handle = std::thread::Builder::new()
            .name("run-port-discovery".to_string())
            .spawn(move || {
                let mut previous: Option<DiscoveredPorts> = None;
                while !stop.load(Ordering::Acquire) {
                    if let Ok(observation) = collect(root_pid) {
                        let interval = next_interval(&observation);
                        if previous.as_ref() != Some(&observation) {
                            previous = Some(observation.clone());
                            if events
                                .send(RunEvent {
                                    run_id,
                                    kind: RunEventKind::ListeningPorts {
                                        process_id,
                                        observation,
                                    },
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                        if sleep_until_stop(&stop, interval) {
                            break;
                        }
                    } else if sleep_until_stop(&stop, EMPTY_POLL_INTERVAL) {
                        break;
                    }
                }
            })
            .expect("port-discovery thread spawns with valid configuration");
        Self {
            handle: Some(handle),
        }
    }

    pub(crate) fn stop_and_join(self) -> bool {
        self.handle.is_none_or(|handle| handle.join().is_ok())
    }
}

fn next_interval(observation: &DiscoveredPorts) -> Duration {
    if observation.ports.is_empty() {
        EMPTY_POLL_INTERVAL
    } else {
        ACTIVE_POLL_INTERVAL
    }
}

fn sleep_until_stop(stop: &AtomicBool, duration: Duration) -> bool {
    let deadline = std::time::Instant::now() + duration;
    while std::time::Instant::now() < deadline {
        if stop.load(Ordering::Acquire) {
            return true;
        }
        std::thread::sleep(
            STOP_POLL_INTERVAL.min(deadline.saturating_duration_since(std::time::Instant::now())),
        );
    }
    stop.load(Ordering::Acquire)
}

fn bounded(ports: BTreeSet<u16>, best_effort: bool) -> DiscoveredPorts {
    let omitted = ports.len().saturating_sub(MAX_DISCOVERED_PORTS);
    let ports = ports.into_iter().take(MAX_DISCOVERED_PORTS).collect();
    DiscoveredPorts {
        ports,
        omitted: omitted.min(usize::from(u16::MAX)) as u16,
        best_effort,
    }
}

#[cfg(target_os = "linux")]
fn collect(root_pid: u32) -> io::Result<DiscoveredPorts> {
    use crate::runtime::process_tree::UnixProcessTree;

    let members = UnixProcessTree::from_root(root_pid)
        .remaining_members()
        .map_err(io::Error::other)?;
    let mut socket_inodes = BTreeSet::new();
    let mut best_effort = false;
    for pid in members {
        let entries = match std::fs::read_dir(format!("/proc/{pid}/fd")) {
            Ok(entries) => entries,
            Err(_) => {
                best_effort = true;
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                best_effort = true;
                continue;
            };
            let Ok(target) = std::fs::read_link(entry.path()) else {
                best_effort = true;
                continue;
            };
            if let Some(inode) = socket_inode(target.to_string_lossy().as_ref()) {
                socket_inodes.insert(inode);
            }
        }
    }

    let mut ports = BTreeSet::new();
    collect_linux_table("/proc/net/tcp", &socket_inodes, &mut ports)?;
    collect_linux_table("/proc/net/tcp6", &socket_inodes, &mut ports)?;
    Ok(bounded(ports, best_effort))
}

#[cfg(target_os = "linux")]
fn socket_inode(target: &str) -> Option<u64> {
    target
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

#[cfg(target_os = "linux")]
fn collect_linux_table(
    path: &str,
    owned_inodes: &BTreeSet<u64>,
    ports: &mut BTreeSet<u16>,
) -> io::Result<()> {
    let table = std::fs::read_to_string(path)?;
    for line in table.lines().skip(1) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.get(3) != Some(&"0A") {
            continue;
        }
        let Some(inode) = fields.get(9).and_then(|value| value.parse::<u64>().ok()) else {
            continue;
        };
        if !owned_inodes.contains(&inode) {
            continue;
        }
        let Some(port) = fields
            .get(1)
            .and_then(|address| address.rsplit_once(':'))
            .and_then(|(_, port)| u16::from_str_radix(port, 16).ok())
        else {
            continue;
        };
        if port != 0 {
            ports.insert(port);
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn collect(root_pid: u32) -> io::Result<DiscoveredPorts> {
    let output = std::process::Command::new("/usr/sbin/lsof")
        .args(["-nP", "-a", "-g"])
        .arg(root_pid.to_string())
        .args(["-iTCP", "-sTCP:LISTEN", "-Fpn"])
        .output()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let ports = parse_lsof_ports(&text);
    // lsof returns 1 both for an empty selection and for several recoverable
    // inspection failures. Output that it did produce remains useful.
    let best_effort =
        !output.status.success() && (!output.stdout.is_empty() || !output.stderr.is_empty());
    Ok(bounded(ports, best_effort))
}

#[cfg(target_os = "macos")]
fn parse_lsof_ports(text: &str) -> BTreeSet<u16> {
    text.lines()
        .filter_map(|line| line.strip_prefix('n'))
        .filter_map(|address| address.rsplit_once(':').map(|(_, port)| port))
        .filter_map(|port| port.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .collect()
}

#[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
fn collect(_root_pid: u32) -> io::Result<DiscoveredPorts> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "port discovery is not implemented on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polling_backs_off_only_while_ports_are_present() {
        assert_eq!(
            next_interval(&DiscoveredPorts::default()),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_interval(&DiscoveredPorts {
                ports: vec![5173],
                ..DiscoveredPorts::default()
            }),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn observations_are_sorted_deduplicated_and_bounded() {
        let ports = (1..=40).rev().chain([5, 10]).collect();
        let observation = bounded(ports, false);
        assert_eq!(observation.ports, (1..=32).collect::<Vec<_>>());
        assert_eq!(observation.omitted, 8);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_socket_inode_parser_rejects_other_file_descriptors() {
        assert_eq!(socket_inode("socket:[12345]"), Some(12345));
        assert_eq!(socket_inode("pipe:[12345]"), None);
        assert_eq!(socket_inode("socket:[bad]"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn lsof_parser_handles_ipv4_ipv6_and_wildcards() {
        let ports = parse_lsof_ports("p10\nn127.0.0.1:5173\nn[::1]:5173\nn*:8080\nninvalid\n");
        assert_eq!(ports, BTreeSet::from([5173, 8080]));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn collector_finds_a_real_listener_in_the_owned_process_tree() {
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a local test listener binds");
        let port = listener
            .local_addr()
            .expect("listener has an address")
            .port();
        #[cfg(target_os = "linux")]
        let root = std::process::id();
        #[cfg(target_os = "macos")]
        let root = unsafe { libc::getpgrp() as u32 };

        let observation = collect(root).expect("the platform collector inspects this Process Tree");
        assert!(
            observation.ports.contains(&port),
            "the real listener is discovered: {observation:?}"
        );
    }
}
