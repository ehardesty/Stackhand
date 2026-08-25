//! The semantic shutdown ladder: interrupt → wait → terminate → wait →
//! kill → wait, executed against the owned Process Tree while its identity
//! is intact.

use std::time::{Duration, Instant};

use crate::runtime::OsPid;
use crate::runtime::outcome::StageResult;
use crate::runtime::process_tree::{SemanticSignal, SignalError, UnixProcessTree};

/// Poll cadence for observing Process Tree state.
pub(crate) const RUN_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(15);
/// Upper bound for Process Tree enumeration polls while waiting for
/// containment confirmation; keeps `ps` pressure low under parallel load.
const SETTLED_POLL_CEILING: Duration = Duration::from_millis(75);

/// Collected results of one shutdown-ladder execution.
pub(crate) struct LadderTrace {
    pub stages: Vec<StageResult>,
    pub remaining_pids: Vec<OsPid>,
    /// Set when a signal-stage error makes further Process Group signaling
    /// unsafe. Finalization still runs; only further signals are skipped.
    pub signals_stopped: bool,
}

impl LadderTrace {
    fn new() -> Self {
        Self {
            stages: Vec::new(),
            remaining_pids: Vec::new(),
            signals_stopped: false,
        }
    }

    /// The trace recorded when the Run never had an observable Process Tree
    /// identity. Signals are skipped; finalization still proceeds.
    pub(crate) fn without_identity() -> Self {
        Self {
            stages: vec![StageResult::failed(
                "identity",
                "no observable Process Tree identity".to_string(),
            )],
            remaining_pids: Vec::new(),
            signals_stopped: true,
        }
    }

    /// Trace used when a prior semantic signal failed and the Run must skip
    /// every later signal attempt.
    pub(crate) fn signal_failure(detail: String) -> Self {
        Self {
            stages: vec![StageResult::failed("signal", detail)],
            remaining_pids: Vec::new(),
            signals_stopped: true,
        }
    }

    pub(crate) fn record_remaining(&mut self, tree: &UnixProcessTree) {
        let root_exited = UnixProcessTree::root_exit_pending(tree.root_pid());
        if let Ok(mut members) = tree.remaining_members() {
            if root_exited {
                members.remove(&tree.root_pid());
            } else {
                // A live or unobservable root must remain in the diagnostic
                // set even if enumeration raced and did not list it.
                members.insert(tree.root_pid());
            }
            self.remaining_pids = members.into_iter().map(OsPid::new).collect();
        } else if !root_exited {
            self.remaining_pids = vec![OsPid::new(tree.root_pid())];
        }
    }
}

fn stage_result(name: &'static str, sent: bool) -> StageResult {
    if sent {
        StageResult::ok(name)
    } else {
        StageResult {
            stage: name,
            ok: true,
            detail: Some("already settled; no signal needed".to_string()),
        }
    }
}

fn record_retransmit_failure(trace: &mut LadderTrace, stage: &'static str, error: SignalError) {
    if let Some(result) = trace
        .stages
        .iter_mut()
        .rev()
        .find(|result| result.stage == stage)
    {
        result.ok = false;
        result.detail = Some(format!("signal retransmit failed: {}", error.detail()));
    } else {
        trace.stages.push(StageResult::failed(
            stage,
            format!("signal retransmit failed: {}", error.detail()),
        ));
    }
}

/// Execute interrupt → wait → terminate → wait → kill → wait against
/// the owned Process Tree while its identity is intact. The unreaped
/// root keeps the group in existence and its PID reserved, so group
/// signals and direct unreaped-root signals are both safe here. No
/// signal of any kind follows a reap.
pub(crate) fn run(
    tree: &UnixProcessTree,
    ladder: crate::runtime::outcome::ShutdownLadder,
) -> LadderTrace {
    let mut trace = LadderTrace::new();

    // Stage: interrupt.
    match send_stage(tree, SemanticSignal::Interrupt) {
        Ok(sent) => trace.stages.push(stage_result("interrupt", sent)),
        Err(error) => {
            trace
                .stages
                .push(StageResult::failed("interrupt", error.detail()));
            trace.signals_stopped = true;
            trace.record_remaining(tree);
            return trace;
        }
    }
    if let Err(error) =
        wait_settled_retransmitting(tree, SemanticSignal::Interrupt, ladder.graceful_timeout)
    {
        record_retransmit_failure(&mut trace, "interrupt", error);
        trace.signals_stopped = true;
        trace.record_remaining(tree);
        return trace;
    }

    // Stage: terminate remaining members.
    match send_stage(tree, SemanticSignal::Terminate) {
        Ok(sent) => trace.stages.push(stage_result("terminate", sent)),
        Err(error) => {
            trace
                .stages
                .push(StageResult::failed("terminate", error.detail()));
            trace.signals_stopped = true;
            trace.record_remaining(tree);
            return trace;
        }
    }
    if let Err(error) =
        wait_settled_retransmitting(tree, SemanticSignal::Terminate, ladder.terminate_timeout)
    {
        record_retransmit_failure(&mut trace, "terminate", error);
        trace.signals_stopped = true;
        trace.record_remaining(tree);
        return trace;
    }

    // Stage: kill whatever remains.
    let kill_sent = send_stage(tree, SemanticSignal::Kill);
    if !finish_kill_stage(
        &mut trace,
        kill_sent,
        || {
            wait_settled_retransmitting(tree, SemanticSignal::Kill, ladder.final_deadline)
                .map(|_| ())
        },
        |trace| trace.record_remaining(tree),
    ) {
        return trace;
    }
    trace.record_remaining(tree);
    trace
}

/// Finish the kill stage. An initial signal failure is terminal: record the
/// remaining members and do not run retransmission against the numeric PGID.
fn finish_kill_stage<Wait, Record>(
    trace: &mut LadderTrace,
    initial: std::result::Result<bool, SignalError>,
    wait: Wait,
    record_remaining: Record,
) -> bool
where
    Wait: FnOnce() -> std::result::Result<(), SignalError>,
    Record: FnOnce(&mut LadderTrace),
{
    match initial {
        Ok(sent) => {
            trace.stages.push(stage_result("kill", sent));
            if let Err(error) = wait() {
                record_retransmit_failure(trace, "kill", error);
                trace.signals_stopped = true;
            }
            true
        }
        Err(error) => {
            trace
                .stages
                .push(StageResult::failed("kill", error.detail()));
            trace.signals_stopped = true;
            record_remaining(trace);
            false
        }
    }
}

/// Deliver one ladder stage. Kill applies only to members that remain:
/// dead processes leave the group, and a settled tree needs no signal.
/// Returns whether a signal was actually sent.
fn send_stage(
    tree: &UnixProcessTree,
    semantic: SemanticSignal,
) -> std::result::Result<bool, SignalError> {
    // Cheap probe first; enumeration runs only when needed.
    if tree_settled(tree) {
        return Ok(false);
    }
    let target_group = !tree_is_empty(tree);
    let result = if target_group {
        tree.signal(semantic)
    } else {
        tree.signal_root_unreaped(semantic)
    };
    match result {
        Ok(()) => Ok(true),
        // An exit race is harmless and means there is nothing left.
        Err(SignalError::NotFound) => Ok(false),
        // Ownership/permission failures fail closed: no further signals
        // against this numeric PGID. Finalization still proceeds.
        Err(error) => Err(error),
    }
}

fn wait_settled_retransmitting(
    tree: &UnixProcessTree,
    stage: SemanticSignal,
    budget: Duration,
) -> std::result::Result<bool, SignalError> {
    const RETRANSMIT_INTERVAL: Duration = Duration::from_millis(250);
    let started = Instant::now();
    let mut next_send = started + RETRANSMIT_INTERVAL;
    loop {
        if tree_settled(tree) {
            return Ok(true);
        }
        if Instant::now() >= started + budget {
            return Ok(tree_settled(tree));
        }
        if Instant::now() >= next_send {
            let result = if tree_is_empty(tree) {
                tree.signal_root_unreaped(stage)
            } else {
                tree.signal(stage)
            };
            match result {
                Ok(()) | Err(SignalError::NotFound) => {}
                Err(error) => return Err(error),
            }
            next_send = Instant::now() + RETRANSMIT_INTERVAL;
        }
        std::thread::sleep(RUN_EXIT_POLL_INTERVAL.min(SETTLED_POLL_CEILING));
    }
}

fn tree_is_empty(tree: &UnixProcessTree) -> bool {
    tree.remaining_members_excluding_root()
        .map(|members| members.is_empty())
        .unwrap_or(false)
}

/// Whether the Run's Process Tree work is done: the root has exited and
/// no other member remains. Both halves are required — an empty member
/// list alone cannot distinguish "all clear" from "children not yet
/// spawned by a live root".
fn tree_settled(tree: &UnixProcessTree) -> bool {
    UnixProcessTree::root_exit_pending(tree.root_pid()) && tree_is_empty(tree)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_kill_failure_records_members_without_retransmission() {
        let mut trace = LadderTrace::new();
        let mut waited = false;
        let mut recorded = false;
        let continued = finish_kill_stage(
            &mut trace,
            Err(SignalError::Ownership("ownership changed".to_string())),
            || {
                waited = true;
                Ok(())
            },
            |trace| {
                recorded = true;
                trace.remaining_pids.push(OsPid::new(7));
            },
        );

        assert!(!continued);
        assert!(recorded);
        assert!(!waited);
        assert!(trace.signals_stopped);
        assert_eq!(trace.remaining_pids, vec![OsPid::new(7)]);
        assert_eq!(trace.stages.len(), 1);
        assert_eq!(trace.stages[0].stage, "kill");
        assert!(!trace.stages[0].ok);
    }
}
