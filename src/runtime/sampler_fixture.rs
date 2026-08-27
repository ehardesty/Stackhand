//! The sampler fixture for issue #33: a real spawned Process is sampled
//! with a short interval, so the sampler's event stream proves metrics keep
//! the live Run identity within a fixed bound.

use std::time::{Duration, Instant};

use std::os::unix::process::CommandExt;

use crate::runtime::{ProcessId, RunEventKind, RunId, metrics::MetricsSampler};

const SAMPLE_INTERVAL: Duration = Duration::from_millis(100);
/// `ps` settles after the fork; the bound stays generous but fixed.
const BOUND: Duration = Duration::from_secs(15);

#[cfg(test)]
#[test]
fn a_live_sampler_reports_run_scoped_metrics() {
    let (events, receiver) = std::sync::mpsc::channel();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // A real child keeps the sampler's enumeration rooted: a busy loop
    // guarantees non-zero resident memory once `ps` sees it. The child
    // is its own Process Group root, exactly like a pipe Run.
    let mut command = std::process::Command::new("/bin/sh");
    command.args(["-c", "i=0; while :; do i=$((i+1)); done"]);
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let mut child = command.spawn().expect("the fixture child spawns");
    let root_pid = child.id();

    let sampler = MetricsSampler::spawn(
        root_pid,
        ProcessId::new(9),
        RunId::new(901),
        SAMPLE_INTERVAL,
        stop.clone(),
        events,
    );

    // A bounded wait for the sampler's first real sample.
    let deadline = Instant::now() + BOUND;
    let mut saw_sample = false;
    while !saw_sample && Instant::now() < deadline {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => {
                saw_sample = matches!(event.kind, RunEventKind::Metrics { .. });
            }
            // Quiet slice: keep polling until the bound or the sampler
            // drops the channel (Disconnected ends the wait).
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Release);
    let (last, joined) = sampler.stop_and_join();
    assert!(joined, "the sampler thread joins on stop");
    assert!(
        saw_sample || last.is_some(),
        "a live Run must report a sample within the bound"
    );
    let sample = last.expect("the sampler keeps its last sample");
    assert_eq!(sample.run_id, RunId::new(901));
    assert!(
        sample.rss_kib > 0,
        "a live loop reports resident memory, got {sample:?}"
    );

    child.kill().expect("the fixture child is killed");
    child.wait().expect("the fixture child is reaped");
}
