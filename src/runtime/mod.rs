mod input_writer;
mod metrics;
mod pipe;
mod process_tree;
mod pty;
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
mod shutdown_tests;
#[cfg(test)]
pub(crate) use run::handle_for_test;

pub(crate) use input_writer::{
    BoundedPtyWriter, PtyWriterEvent, PtyWriterOwner, spawn_bounded_pty_writer,
};
pub(crate) use pty::{PtyIo, PtyProcess, PtyResizer, SpawnCommand};
// These re-exports form the public Run ownership seam; some are used only by
// callers outside this module tree today.
#[allow(unused_imports)]
pub use metrics::RunMetrics;
#[allow(unused_imports)]
pub use run::{
    OwnedRun, ProcessId, ResizeRejected, RunEvent, RunEventKind, RunExitDisposition, RunId,
    RunMode, RunOutcome, RunRuntime, RunStartRequest, ShutdownLadder, StageResult, TerminalHandle,
};
