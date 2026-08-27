mod input_writer;
mod ladder;
mod metrics;
mod outcome;
mod pipe;
mod process_tree;
mod pty;
mod terminal_handle;
// The Run ownership seam is the product interface for Milestone 0B. Some of
// its public surface (resize, natural-exit waiting) is exercised through
// callers and tests rather than this crate's non-test code yet.
#[cfg(test)]
mod metrics_tests;
#[cfg(test)]
mod pressure_tests;
#[allow(dead_code)]
mod run;
#[cfg(test)]
mod run_tests;
#[cfg(test)]
mod sampler_fixture;
#[cfg(test)]
mod shutdown_tests;
#[cfg(test)]
pub(crate) use terminal_handle::handle_for_test;

pub(crate) use input_writer::{
    BoundedPtyWriter, PtyWriterEvent, PtyWriterOwner, spawn_bounded_pty_writer,
};
pub(crate) use pty::{PtyIo, PtyProcess, PtyResizer, SpawnCommand};
// These re-exports form the public Run ownership seam; some are used only by
// callers outside this module tree today.
#[allow(unused_imports)]
pub use metrics::RunMetrics;
#[allow(unused_imports)]
pub use outcome::{ResizeRejected, RunExitDisposition, RunOutcome, ShutdownLadder, StageResult};
#[allow(unused_imports)]
pub use pipe::{
    OUTPUT_QUEUE_BYTES, OUTPUT_QUEUE_SLOTS, OutputStream, RunOutput, RunOutputReceiver,
    RunOutputSender, output_channel,
};
#[allow(unused_imports)]
pub use run::{
    InputRejected, OsPid, OwnedRun, ProcessId, RunEvent, RunEventKind, RunId, RunMode, RunRuntime,
    RunStartRequest,
};
pub use terminal_handle::TerminalHandle;

/// Whether one Run's root child has exited but is not yet reaped. The
/// observation never reaps, so Process Group identity stays intact for the
/// Run's own bounded cleanup. Exposed for the Supervisor adapter's
/// natural-exit watchers; the type itself stays inside this module tree.
pub(crate) fn root_exit_pending(root_pid: OsPid) -> bool {
    process_tree::UnixProcessTree::root_exit_pending(root_pid.get())
}
