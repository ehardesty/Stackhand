use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};

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
}

pub struct SpawnedPty {
    pub process: PtyProcess,
    pub io: PtyIo,
}

pub struct PtyProcess {
    child: Option<Box<dyn Child + Send + Sync>>,
}

impl PtyProcess {
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

    pub fn shutdown(&mut self) -> Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };

        if child.try_wait()?.is_none() {
            child.kill().context("could not stop the shell")?;
        }
        child.wait().context("could not wait for the shell")?;
        Ok(())
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
