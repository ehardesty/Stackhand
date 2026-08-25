use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use crate::geometry::TerminalGeometry;
use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

pub type PtyResizer = Box<
    dyn FnMut(u16, u16) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> + Send,
>;

pub struct PtyIo {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub resizer: PtyResizer,
}

pub struct SpawnCommand {
    program: OsString,
    args: Vec<OsString>,
}

impl SpawnCommand {
    pub fn shell() -> Self {
        let program = std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"));
        Self {
            program,
            args: Vec::new(),
        }
    }

    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub(crate) fn program(&self) -> &OsStr {
        &self.program
    }

    pub(crate) fn args(&self) -> &[OsString] {
        &self.args
    }
}

pub struct SpawnedPty {
    pub process: PtyProcess,
    pub io: PtyIo,
}

pub struct PtyProcess {
    child: Option<Box<dyn Child + Send + Sync>>,
}

impl PtyProcess {
    /// Whether the root process has exited, with its exit code when the
    /// platform reports one. Does not consume the child handle. Used by the
    /// Run owner's natural-exit wait.
    #[allow(dead_code)]
    pub(crate) fn try_wait(&mut self) -> Result<Option<i32>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let status = child
            .try_wait()
            .context("could not poll the process exit state")?;
        Ok(status.map(|status| i32::try_from(status.exit_code()).unwrap_or(i32::MAX)))
    }

    /// Reap only after the caller has observed root exit without reaping and
    /// Process Tree work is complete. The poll is bounded and never calls the
    /// blocking child wait when the deadline has passed.
    pub(crate) fn reap_bounded(&mut self, deadline: Instant) -> Result<Option<i32>> {
        loop {
            let Some(child) = self.child.as_mut() else {
                return Ok(None);
            };
            match child.try_wait()? {
                Some(status) => {
                    let code = i32::try_from(status.exit_code()).unwrap_or(i32::MAX);
                    let _ = self.child.take();
                    return Ok(Some(code));
                }
                None if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                None => {
                    return Err(anyhow::anyhow!(
                        "root process was not reaped before the final deadline"
                    ));
                }
            }
        }
    }

    pub fn spawn(command: SpawnCommand, geometry: TerminalGeometry) -> Result<SpawnedPty> {
        let pair = native_pty_system()
            .openpty(pty_size(geometry))
            .context("could not open the PTY")?;

        let mut builder = CommandBuilder::new(&command.program);
        for arg in command.args {
            builder.arg(arg);
        }
        builder.env("TERM", "xterm-256color");
        builder.env("COLORTERM", "truecolor");

        let child = pair
            .slave
            .spawn_command(builder)
            .with_context(|| format!("could not start {}", display_program(&command.program)))?;
        let process = Self { child: Some(child) };
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .context("could not open the PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("could not open the PTY writer")?;
        let master = pair.master;

        let io = PtyIo {
            reader,
            writer,
            resizer: Box::new(move |cols, rows| resize_master(master.as_ref(), cols, rows)),
        };

        Ok(SpawnedPty { process, io })
    }

    /// The root operating-system PID when the platform reports one.
    pub fn process_id(&self) -> Option<u32> {
        self.child.as_ref().and_then(|child| child.process_id())
    }

    /// Non-blocking cleanup for failed shutdown paths. Reaps the root only
    /// when it has already exited and reports when it cannot be confirmed.
    pub(crate) fn abandon_nonblocking(&mut self) -> Vec<String> {
        let Some(child) = self.child.as_mut() else {
            return Vec::new();
        };
        match child.try_wait() {
            Ok(Some(_)) => Vec::new(),
            Ok(None) => vec!["root process left unreaped".to_string()],
            Err(error) => vec![format!("root process state unobservable: {error}")],
        }
    }

    /// Detach the root without probing or reaping it. This is required when
    /// Process Tree containment is not confirmed before a final deadline.
    pub(crate) fn abandon_without_reap(&mut self) -> Vec<String> {
        if self.child.take().is_some() {
            vec![
                "root process detached without reaping because containment was unconfirmed"
                    .to_string(),
            ]
        } else {
            Vec::new()
        }
    }

    pub fn shutdown(&mut self) -> Result<Option<i32>> {
        let Some(mut child) = self.child.take() else {
            return Ok(None);
        };

        if child.try_wait()?.is_none() {
            child.kill().context("could not stop the shell")?;
        }
        let status = child.wait().context("could not wait for the shell")?;
        Ok(Some(i32::try_from(status.exit_code()).unwrap_or(i32::MAX)))
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn resize_master(
    master: &dyn MasterPty,
    cols: u16,
    rows: u16,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(geometry) = TerminalGeometry::new(cols, rows) else {
        return Err("PTY geometry must be non-zero".into());
    };
    master.resize(pty_size(geometry)).map_err(Into::into)
}

fn pty_size(geometry: TerminalGeometry) -> PtySize {
    PtySize {
        rows: geometry.rows(),
        cols: geometry.cols(),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn display_program(program: &OsStr) -> String {
    program.to_string_lossy().into_owned()
}
