//! Aggregate CPU and resident-memory sampling for one owned Process Tree.
//!
//! The sampler runs outside process I/O work on its own thread, emits at
//! most one bounded [`RunMetrics`] snapshot per configured interval through
//! the low-volume Run event sink, tolerates processes exiting during
//! enumeration, and stops with the Run.
//!
//! Normalization: one fully used logical CPU is 100 percent, so aggregate
//! Process Tree CPU can exceed 100 percent on multi-core machines. Memory is
//! aggregate resident memory of the members the platform adapter identifies
//! as owned. Results are marked best effort when complete membership cannot
//! be proved (a member exited mid-enumeration or the root was invisible).
//!
//! Platform notes:
//! - Linux reads `/proc/<pid>/stat`: scheduler ticks are converted to an
//!   instantaneous rate from the delta between consecutive samples.
//! - macOS reads `ps -axo pid=,pgid=,pcpu=,rss=`; `ps` reports CPU as a
//!   percentage of one core already, so values sum directly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

use crate::runtime::{ProcessId, RunEvent, RunEventKind, RunId};

/// One bounded metrics snapshot for one Run. Every snapshot carries the
/// ProcessId and RunId.
#[derive(Clone, Debug, PartialEq)]
pub struct RunMetrics {
    pub process_id: ProcessId,
    pub run_id: RunId,
    /// Monotonically increasing sequence number for this Run's samples.
    pub sequence: u64,
    /// Aggregate Process Tree CPU. One fully used logical CPU is 100.
    pub cpu_percent: f64,
    /// Aggregate resident memory of observed members, in KiB.
    pub rss_kib: u64,
    /// Number of owned members observed in this snapshot.
    pub members_observed: u32,
    /// True when complete Process Tree membership could not be proved for
    /// this snapshot.
    pub best_effort: bool,
}

type EventSink = Sender<RunEvent>;

/// One sampled member of the owned Process Tree.
struct MemberSample {
    #[allow(dead_code)]
    pid: u32,
    cpu_percent: f64,
    rss_kib: u64,
}

/// Aggregated view of one enumeration pass.
struct TreeObservation {
    members: Vec<MemberSample>,
    best_effort: bool,
}

impl TreeObservation {
    fn aggregate(&self) -> (f64, u64, u32) {
        let mut cpu = 0.0;
        let mut rss = 0u64;
        for member in &self.members {
            cpu += member.cpu_percent;
            rss += member.rss_kib;
        }
        (cpu, rss, self.members.len() as u32)
    }
}

/// Runs on its own thread for the lifetime of one Run.
pub(crate) struct MetricsSampler {
    latest: Arc<Mutex<Option<RunMetrics>>>,
    handle: Option<JoinHandle<()>>,
}

impl MetricsSampler {
    /// Start sampling the Process Tree rooted at `root_pid`.
    pub(crate) fn spawn(
        root_pid: u32,
        process_id: ProcessId,
        run_id: RunId,
        interval: Duration,
        stop: Arc<AtomicBool>,
        events: EventSink,
    ) -> Self {
        let latest = Arc::new(Mutex::new(None::<RunMetrics>));
        let latest_for_thread = Arc::clone(&latest);
        let handle = std::thread::Builder::new()
            .name("run-metrics".to_string())
            .spawn(move || {
                let mut sequence = 0u64;
                // Scheduler ticks seen in the previous pass, for the rate
                // calculation on Linux. Unused on macOS.
                #[cfg(target_os = "linux")]
                let mut previous: std::collections::BTreeMap<u32, u64> =
                    std::collections::BTreeMap::new();
                #[cfg(target_os = "linux")]
                let mut last_pass = Instant::now();
                loop {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::sleep(interval);
                    // Check the gate again after sleeping so a stop during
                    // the sleep never produces a late sample.
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    sequence += 1;
                    #[cfg(target_os = "linux")]
                    let now = Instant::now();
                    #[cfg(target_os = "linux")]
                    let window = now.duration_since(last_pass);
                    #[cfg(target_os = "linux")]
                    {
                        last_pass = now;
                    }
                    let observation = collect_tree(root_pid);

                    let (mut cpu_percent, rss_kib, member_count) = observation.aggregate();
                    #[cfg(target_os = "linux")]
                    {
                        // Convert summed tick deltas into a percentage where
                        // one fully used logical CPU is 100.
                        let mut delta = 0.0f64;
                        for member in &observation.members {
                            let total = member.cpu_percent as u64; // raw ticks carrier
                            let previous_ticks =
                                previous.insert(member.pid, total).unwrap_or(total);
                            delta += total.saturating_sub(previous_ticks) as f64;
                        }
                        let clock_ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
                        let seconds = window.as_secs_f64().max(0.001);
                        cpu_percent = delta / (clock_ticks * seconds) * 100.0;
                    }
                    #[cfg(not(target_os = "linux"))]
                    let _ = &mut cpu_percent; // macOS ps pcpu sums directly.

                    let snapshot = RunMetrics {
                        process_id,
                        run_id,
                        sequence,
                        cpu_percent,
                        rss_kib,
                        members_observed: member_count,
                        best_effort: observation.best_effort,
                    };
                    *latest_for_thread
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(snapshot.clone());
                    // Final gate before emission: once stop is observed, no
                    // later sample may leave the sampler.
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    if events
                        .send(RunEvent {
                            run_id,
                            kind: RunEventKind::Metrics(snapshot),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("run-metrics thread spawns with valid configuration");
        Self {
            latest,
            handle: Some(handle),
        }
    }

    /// Stop the sampler and join its thread. Returns the last valid sample
    /// (if any) and whether the thread joined cleanly.
    pub(crate) fn stop_and_join(self) -> (Option<RunMetrics>, bool) {
        let latest = self
            .latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let joined = self.handle.is_none_or(|handle| handle.join().is_ok());
        (latest, joined)
    }
}

/// Enumerate live members of the owned group once.
fn collect_tree(root_pid: u32) -> TreeObservation {
    #[cfg(target_os = "macos")]
    {
        collect_tree_macos(root_pid)
    }
    #[cfg(target_os = "linux")]
    {
        collect_tree_linux(root_pid)
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
    {
        let _ = root_pid;
        TreeObservation {
            members: Vec::new(),
            best_effort: true,
        }
    }
}

#[cfg(target_os = "macos")]
fn collect_tree_macos(root_pid: u32) -> TreeObservation {
    let output = match std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,pgid=,pcpu=,rss="])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => {
            return TreeObservation {
                members: Vec::new(),
                best_effort: true,
            };
        }
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut observation = TreeObservation {
        members: Vec::new(),
        best_effort: false,
    };
    let mut saw_root = false;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(pgid), Some(pcpu), Some(rss)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(pid), Ok(pgid)) = (pid.parse::<u32>(), pgid.parse::<u32>()) else {
            continue;
        };
        if pgid != root_pid {
            continue;
        }
        if pid == root_pid {
            saw_root = true;
        }
        observation.members.push(MemberSample {
            pid,
            cpu_percent: pcpu.parse().unwrap_or(0.0),
            rss_kib: rss.parse().unwrap_or(0),
        });
    }
    observation.best_effort |= !saw_root;
    observation
}

#[cfg(target_os = "linux")]
fn collect_tree_linux(root_pid: u32) -> TreeObservation {
    const KIB_PER_PAGE: u64 = 4;
    let mut observation = TreeObservation {
        members: Vec::new(),
        best_effort: false,
    };
    let mut saw_root = false;
    let pids: Vec<u32> = std::fs::read_dir("/proc")
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .and_then(|name| name.parse().ok())
                })
                .collect()
        })
        .unwrap_or_default();
    for pid in pids {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue; // Exited mid-enumeration: tolerated.
        };
        let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let pgrp: Option<u32> = fields.get(2).and_then(|value| value.parse().ok());
        if pgrp != Some(root_pid) {
            continue;
        }
        if pid == root_pid {
            saw_root = true;
        }
        let utime: u64 = fields
            .get(11)
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let stime: u64 = fields
            .get(12)
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let rss_pages: u64 = fields
            .get(21)
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        observation.members.push(MemberSample {
            pid,
            // Raw ticks ride along; the sampler converts deltas to percent.
            cpu_percent: (utime + stime) as f64,
            rss_kib: rss_pages.saturating_mul(KIB_PER_PAGE),
        });
    }
    observation.best_effort |= !saw_root;
    observation
}
