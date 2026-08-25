//! Structured outcomes and ladder configuration for completed Runs.

use std::time::Duration;

use crate::runtime::RunId;
use crate::runtime::metrics::RunMetrics;

/// Why a resize request was rejected. Non-fatal: the Run stays healthy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeRejected {
    /// Pipe mode has no terminal to resize.
    Unsupported,
    /// The Run is shutting down; resize requests are no longer admitted.
    Stopping,
    /// The bounded terminal command queue cannot admit the resize.
    Backpressure {
        attempted_bytes: usize,
        pending_bytes: usize,
        limit_bytes: usize,
    },
}

/// The configured semantic shutdown ladder for one Run.
///
/// interrupt → wait `graceful_timeout` → terminate → wait
/// `terminate_timeout` → kill remaining members → wait up to
/// `final_deadline` for Process Tree exit.
#[derive(Clone, Copy, Debug)]
pub struct ShutdownLadder {
    pub graceful_timeout: Duration,
    pub terminate_timeout: Duration,
    pub final_deadline: Duration,
}

impl Default for ShutdownLadder {
    fn default() -> Self {
        Self {
            graceful_timeout: Duration::from_secs(5),
            terminate_timeout: Duration::from_secs(3),
            final_deadline: Duration::from_secs(10),
        }
    }
}

/// How one completed Run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunExitDisposition {
    /// The Run finished with exit code 0 and no stop request.
    NaturalCompletion,
    /// The Run exited without a stop request and not with exit code 0.
    UnexpectedExit,
    /// A shutdown request was recorded, even if the process exited first.
    IntentionalStop,
}

/// One recorded stage of the shutdown ladder or finalization sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageResult {
    pub stage: &'static str,
    pub ok: bool,
    pub detail: Option<String>,
}

impl StageResult {
    pub(crate) fn ok(stage: &'static str) -> Self {
        Self {
            stage,
            ok: true,
            detail: None,
        }
    }

    pub(crate) fn failed(stage: &'static str, detail: String) -> Self {
        Self {
            stage,
            ok: false,
            detail: Some(detail),
        }
    }
}

/// One structured result for a completed Run. Callers never assemble
/// cleanup results from pieces; every completion path produces exactly one
/// of these.
#[derive(Clone, Debug, PartialEq)]
pub struct RunOutcome {
    pub run_id: RunId,
    pub disposition: RunExitDisposition,
    /// Whether a shutdown request was recorded for this Run.
    pub intentional_stop: bool,
    pub exit_code: Option<i32>,
    /// Every executed or skipped ladder/cleanup stage, in order.
    pub stage_results: Vec<StageResult>,
    /// True only when the owned Process Tree is confirmed empty and all
    /// worker threads joined cleanly.
    pub cleanup_confirmed: bool,
    /// Known members whose exit could not be confirmed.
    pub remaining_pids: Vec<crate::runtime::OsPid>,
    pub io_failures: Vec<String>,
    pub terminal_failure: Option<String>,
    pub worker_join_failures: Vec<String>,
    /// The last valid sample retained when the sampler stopped with the
    /// Run, if sampling was enabled and produced at least one snapshot.
    pub final_metrics: Option<RunMetrics>,
}
