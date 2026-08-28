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
    current_dir: Option<std::path::PathBuf>,
    envs: Vec<(OsString, OsString)>,
    env_removals: Vec<OsString>,
}

impl SpawnCommand {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            envs: Vec::new(),
            env_removals: Vec::new(),
        }
    }

    pub(crate) fn program(&self) -> &OsStr {
        &self.program
    }

    pub(crate) fn args(&self) -> &[OsString] {
        &self.args
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn with_current_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }

    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }

    pub fn without_env(mut self, key: impl Into<OsString>) -> Self {
        self.env_removals.push(key.into());
        self
    }

    pub(crate) fn current_dir(&self) -> Option<&std::path::Path> {
        self.current_dir.as_deref()
    }

    pub(crate) fn envs(&self) -> &[(OsString, OsString)] {
        &self.envs
    }

    pub(crate) fn env_removals(&self) -> &[OsString] {
        &self.env_removals
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
                    let code = reported_exit_code(&status);
                    let _ = self.child.take();
                    return Ok(code);
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
        if let Some(dir) = &command.current_dir {
            builder.cwd(dir);
        }
        for key in command.env_removals {
            builder.env_remove(key);
        }
        for (key, value) in command.envs {
            builder.env(key, value);
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
        Ok(reported_exit_code(&status))
    }
}

fn reported_exit_code(status: &portable_pty::ExitStatus) -> Option<i32> {
    status
        .signal()
        .is_none()
        .then(|| i32::try_from(status.exit_code()).unwrap_or(i32::MAX))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminating_signal_has_no_success_exit_code() {
        assert_eq!(
            reported_exit_code(&portable_pty::ExitStatus::with_signal("SIGINT")),
            None
        );
        assert_eq!(
            reported_exit_code(&portable_pty::ExitStatus::with_exit_code(42)),
            Some(42)
        );
    }
}
