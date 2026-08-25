mod input_writer;
mod pty;
#[cfg(test)]
pub(crate) use run::handle_for_test;
mod run;

pub(crate) use input_writer::{
    BoundedPtyWriter, PtyWriterEvent, PtyWriterOwner, spawn_bounded_pty_writer,
};
pub(crate) use pty::{PtyIo, PtyProcess, PtyResizer, SpawnCommand};
// These re-exports form the public Run ownership seam; some are used only by
// callers outside this module tree today.
#[allow(unused_imports)]
pub use run::{
    OwnedRun, ProcessId, RunEvent, RunEventKind, RunId, RunMode, RunRuntime, RunStartRequest,
    TerminalHandle,
};
