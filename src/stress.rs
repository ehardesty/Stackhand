//! Deterministic stress evidence for the terminal output path.
//!
//! The production supervisor will eventually own a Process output history and
//! a per-Run terminal actor. This module keeps the scheduling contract small
//! and executable while that larger model is still a prototype.

use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail, ensure};

use crate::fixtures::start_fixture_run;
use crate::geometry::TerminalGeometry;
use crate::runtime::SpawnCommand;
use crate::terminal::{
    OUTPUT_HISTORY_BYTES, OUTPUT_HISTORY_CHUNKS, PASTE_LIMIT_BYTES, PasteCompletion,
    PasteRejection, TerminalEvent,
};

/// A redraw request gate. Many output chunks can set the gate, but only the
/// transition from clean to pending produces a notification.
#[derive(Debug, Default)]
pub struct RedrawGate {
    pending: AtomicBool,
    requests: AtomicUsize,
    notifications: AtomicUsize,
}

impl RedrawGate {
    /// Request a redraw and return whether this request produced a new wake.
    pub fn request(&self) -> bool {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if !self.pending.swap(true, Ordering::AcqRel) {
            self.notifications.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Acknowledge one pending redraw before copying the owned snapshot.
    ///
    /// The caller must copy the snapshot after this operation. A concurrent
    /// request sets the flag again and remains pending for the next draw.
    pub fn take(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }

    #[cfg(test)]
    fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }

    pub fn notifications(&self) -> usize {
        self.notifications.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StressReport {
    pub wake_requests: usize,
    pub redraw_notifications: usize,
    pub snapshots: usize,
    pub active_snapshots: usize,
    pub scrolled_snapshots: usize,
    pub output_callbacks_during_unfocused: usize,
    pub peak_rss_delta_kib: Option<u64>,
    pub history_bytes: usize,
    pub history_evicted_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockedInputReport {
    pub accepted_pastes: usize,
    pub pending_pastes_during_flood: usize,
    pub output_bytes_before: usize,
    pub output_bytes_after: usize,
    pub wakes_before: usize,
    pub wakes_after: usize,
}

const STRESS_TIMEOUT: Duration = Duration::from_secs(5);
const STRESS_MEMORY_LIMIT_KIB: u64 = 128 * 1_024;
static STRESS_FIXTURE_LOCK: Mutex<()> = Mutex::new(());

/// Run one bounded PTY output flood and return measured scheduling evidence.
///
/// The fixture deliberately changes view state while output is being
/// produced. It also sends input after a terminal device-status query has been
/// emitted. The terminal owner must preserve response-before-input ordering,
/// continue draining while no snapshots are taken, and shut down cleanly.
pub fn run_sustained_output_fixture() -> Result<StressReport> {
    let _fixture_guard = STRESS_FIXTURE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let geometry = TerminalGeometry::DEFAULT;
    let command = SpawnCommand::new("/bin/sh").arg("-c").arg(
        r#"stty raw -echo
printf 'stress-start\r\n'
sleep 0.05
printf '\033[6nstress-query-ready\r\n'
sleep 0.1
(
  i=0
  while [ "$i" -lt 30000 ]; do
    printf 'flood-%06d\r\n' "$i"
    i=$((i + 1))
    if [ $((i % 20)) -eq 0 ]; then
      sleep 0.001
    fi
  done
) &
producer=$!
reply=
while :; do
  char=$(dd bs=1 count=1 2>/dev/null)
  reply="${reply}${char}"
  [ "$char" = R ] && break
done
IFS= read -r probe
kill "$producer" 2>/dev/null
wait "$producer" 2>/dev/null
reply_hex=$(printf '%s' "$reply" | od -An -tx1 | tr -d ' \n')
probe_hex=$(printf '%s' "$probe" | od -An -tx1 | tr -d ' \n')
printf '\r\nstress-ack:%s:%s\r\n' "$reply_hex" "$probe_hex""#,
    );

    let baseline_rss = resident_set_kib(std::process::id());
    let redraw = Arc::new(RedrawGate::default());
    let wake_count = Arc::clone(&redraw);
    let mut run = start_fixture_run(
        command,
        geometry,
        Some(Box::new(move || {
            wake_count.request();
        })),
    )?;
    let session = run.terminal().expect("stress fixture is PTY-mode");
    let mut peak_rss_kib = baseline_rss;
    let mut snapshots = 0;
    let mut active_snapshots = 0;
    let mut scrolled_snapshots = 0;
    let mut output_callbacks_before_unfocused = 0;
    let mut output_callbacks_after_unfocused = 0;
    let mut sent_probe = false;
    let mut saw_query_ready = false;
    let mut scrolled = false;
    let mut unfocused = false;
    let mut unfocused_at = None;
    let mut latest_text = String::new();
    let started = Instant::now();
    let deadline = started + STRESS_TIMEOUT;

    let fixture_result = (|| {
        while Instant::now() < deadline {
            if let Some(rss) = resident_set_kib(std::process::id()) {
                peak_rss_kib = Some(peak_rss_kib.map_or(rss, |peak| peak.max(rss)));
            }
            while let Some(event) = session.poll_event() {
                match event {
                    TerminalEvent::Failed(error) => bail!("terminal owner failed: {error}"),
                    TerminalEvent::InputBackpressure {
                        attempted_bytes,
                        pending_bytes,
                        limit_bytes,
                    } => bail!(
                        "stress fixture input was rejected: attempted {attempted_bytes} bytes with {pending_bytes} of {limit_bytes} pending"
                    ),
                    // The shell may finish immediately after printing the
                    // acknowledgement. The owned buffer remains available
                    // until shutdown, so inspect it before treating exit as
                    // a failed stress run.
                    TerminalEvent::Exited => {}
                    TerminalEvent::StateChanged => {}
                    TerminalEvent::OutputTruncated => {}
                }
            }

            let elapsed = started.elapsed();
            if saw_query_ready && !scrolled && elapsed >= Duration::from_millis(50) {
                session.scroll_lines(-400);
                scrolled = true;
            }
            if !unfocused && elapsed >= Duration::from_millis(500) {
                let _ = session.send_focus(false);
                output_callbacks_before_unfocused = redraw.requests();
                unfocused_at = Some(Instant::now());
                unfocused = true;
            }
            if !sent_probe && saw_query_ready && elapsed >= Duration::from_millis(750) {
                // The child already emitted CSI 6 n. This line is accepted
                // after Ghostty's response in the same serialized writer.
                let _ = session.send_raw(b"probe\n".to_vec());
                session.follow_live();
                sent_probe = true;
            }

            let can_snapshot = !unfocused
                || unfocused_at
                    .is_some_and(|started| started.elapsed() >= Duration::from_millis(90));
            if can_snapshot && (redraw.take() || session.is_dirty()) {
                let snapshot = session.snapshot();
                latest_text = snapshot.text();
                saw_query_ready |= latest_text.contains("stress-query-ready");
                snapshots += 1;
                if !scrolled {
                    active_snapshots += 1;
                } else if !unfocused {
                    scrolled_snapshots += 1;
                }
            }

            if unfocused {
                output_callbacks_after_unfocused = redraw.requests();
            }

            if latest_text.contains("stress-ack:") {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }

        ensure!(scrolled, "stress fixture did not enter scroll mode");
        ensure!(unfocused, "stress fixture did not enter unfocused mode");
        ensure!(sent_probe, "stress fixture did not send probe input");
        ensure!(
            saw_query_ready,
            "stress fixture did not parse the terminal query"
        );
        ensure!(snapshots > 0, "stress fixture did not produce a snapshot");
        ensure!(active_snapshots > 0, "active pane did not drain output");
        ensure!(scrolled_snapshots > 0, "scrolled pane did not drain output");
        ensure!(
            output_callbacks_after_unfocused > output_callbacks_before_unfocused,
            "unfocused pane stopped receiving output callbacks"
        );
        ensure!(
            latest_text.contains("stress-ack:"),
            "input/query response did not complete before stress timeout; latest terminal text: {latest_text:?}"
        );
        ensure!(
            latest_text.contains("1b5b"),
            "stress acknowledgement did not contain the terminal query response: {latest_text:?}"
        );
        let history = session.output_history_metrics();
        ensure!(
            history.bytes <= OUTPUT_HISTORY_BYTES,
            "output history exceeded byte limit"
        );
        ensure!(
            history.chunks <= OUTPUT_HISTORY_CHUNKS,
            "output history exceeded chunk limit"
        );
        Ok::<_, anyhow::Error>(())
    })();

    let history = session.output_history_metrics();
    fixture_result?;
    run.shutdown()?;

    let peak_rss_delta_kib = peak_rss_kib
        .zip(baseline_rss)
        .map(|(peak, before)| peak.saturating_sub(before));
    if let Some(delta) = peak_rss_delta_kib {
        ensure!(
            delta <= STRESS_MEMORY_LIMIT_KIB,
            "stress fixture RSS grew by {delta} KiB, over the {STRESS_MEMORY_LIMIT_KIB} KiB bound"
        );
    }

    Ok(StressReport {
        wake_requests: redraw.requests(),
        redraw_notifications: redraw.notifications(),
        snapshots,
        active_snapshots,
        scrolled_snapshots,
        output_callbacks_during_unfocused: output_callbacks_after_unfocused
            .saturating_sub(output_callbacks_before_unfocused),
        peak_rss_delta_kib,
        history_bytes: history.bytes,
        history_evicted_bytes: history.evicted_bytes,
    })
}

/// Prove that one blocked accepted paste does not stop PTY output draining.
pub fn run_blocked_input_output_fixture() -> Result<BlockedInputReport> {
    let _fixture_guard = STRESS_FIXTURE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let geometry = TerminalGeometry::DEFAULT;
    let command = SpawnCommand::new("/bin/sh").arg("-c").arg(
        r#"stty raw -echo
i=0
while [ "$i" -lt 200000 ]; do
  printf 'duplex-%06d\r\n' "$i"
  i=$((i + 1))
done
sleep 5"#,
    );
    let redraw = Arc::new(RedrawGate::default());
    let wake_count = Arc::clone(&redraw);
    let mut run = start_fixture_run(
        command,
        geometry,
        Some(Box::new(move || {
            wake_count.request();
        })),
    )?;
    let session = run.terminal().expect("stress fixture is PTY-mode");
    let fixture_result = (|| {
        let start_deadline = Instant::now() + Duration::from_secs(2);
        while session.output_history_metrics().bytes == 0 && Instant::now() < start_deadline {
            thread::sleep(Duration::from_millis(1));
        }
        ensure!(
            session.output_history_metrics().bytes > 0,
            "no fixture output"
        );

        let paste = "p".repeat(PASTE_LIMIT_BYTES);
        let mut requests = Vec::new();
        let admission_deadline = Instant::now() + Duration::from_secs(2);
        let mut saturated = false;
        while Instant::now() < admission_deadline {
            match session.send_paste(&paste) {
                Ok(request) => requests.push(request),
                Err(PasteRejection::Busy { .. }) => {
                    saturated = true;
                    break;
                }
                Err(error) => bail!("blocked-input fixture rejected paste: {error}"),
            }
            thread::sleep(Duration::from_millis(1));
        }
        ensure!(saturated, "blocked-input fixture did not saturate input");
        ensure!(
            !requests.is_empty(),
            "blocked-input fixture admitted no paste"
        );

        let before = session.output_history_metrics();
        let output_bytes_before = before.bytes + before.evicted_bytes;
        let wakes_before = redraw.requests();
        let accepted_pastes = requests.len();
        let progress_deadline = Instant::now() + Duration::from_secs(2);
        let mut pending_pastes_during_flood = requests.len();
        let mut output_bytes_after = output_bytes_before;
        let mut wakes_after = wakes_before;
        while Instant::now() < progress_deadline {
            while let Some(event) = session.poll_event() {
                if let TerminalEvent::Failed(error) = event {
                    bail!("terminal owner failed during blocked input: {error}");
                }
            }
            requests.retain_mut(|request| request.poll().is_none());
            pending_pastes_during_flood = requests.len();
            let after = session.output_history_metrics();
            output_bytes_after = after.bytes + after.evicted_bytes;
            wakes_after = redraw.requests();
            if pending_pastes_during_flood > 0
                && output_bytes_after > output_bytes_before
                && wakes_after > wakes_before
            {
                break;
            }
            ensure!(
                pending_pastes_during_flood > 0,
                "all paste requests completed before progress was observed: output bytes {output_bytes_before}->{output_bytes_after}, wakes {wakes_before}->{wakes_after}"
            );
            thread::sleep(Duration::from_millis(1));
        }
        ensure!(
            pending_pastes_during_flood > 0,
            "all paste requests completed before blocked-input observation: output bytes {output_bytes_before}->{output_bytes_after}, wakes {wakes_before}->{wakes_after}"
        );
        ensure!(
            output_bytes_after > output_bytes_before,
            "PTY output history did not advance while an accepted paste was pending: bytes {output_bytes_before}->{output_bytes_after}, pending {pending_pastes_during_flood}"
        );
        ensure!(
            wakes_after > wakes_before,
            "terminal wakes did not advance while an accepted paste was pending: wakes {wakes_before}->{wakes_after}, pending {pending_pastes_during_flood}"
        );

        Ok::<_, anyhow::Error>((
            BlockedInputReport {
                accepted_pastes,
                pending_pastes_during_flood,
                output_bytes_before,
                output_bytes_after,
                wakes_before,
                wakes_after,
            },
            requests,
        ))
    })();

    let (report, mut requests) = fixture_result?;
    run.shutdown()?;
    for request in &mut requests {
        ensure!(
            matches!(
                request.poll(),
                Some(PasteCompletion::Delivered | PasteCompletion::Failed(_))
            ),
            "paste request {} had no terminal completion after shutdown",
            request.id()
        );
    }
    Ok(report)
}

fn resident_set_kib(pid: u32) -> Option<u64> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redraw_requests_coalesce_until_a_snapshot_is_taken() {
        let gate = RedrawGate::default();

        assert!(gate.request());
        for _ in 0..10_000 {
            assert!(!gate.request());
        }
        assert_eq!(gate.requests(), 10_001);
        assert_eq!(gate.notifications(), 1);
        assert!(gate.take());
        assert!(!gate.is_pending());
        assert!(gate.request());
        assert_eq!(gate.notifications(), 2);
    }

    #[test]
    fn output_arriving_during_a_copy_remains_pending() {
        let gate = RedrawGate::default();

        assert!(gate.request());
        assert!(gate.take());
        assert!(gate.request());
        assert!(gate.is_pending());
        assert_eq!(gate.notifications(), 2);
    }

    #[test]
    fn memory_sampler_is_stable_for_this_process() {
        let before = resident_set_kib(std::process::id());
        let after = resident_set_kib(std::process::id());
        assert!(before.is_some() || after.is_some());
    }
}
