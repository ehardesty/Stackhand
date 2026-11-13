use std::fs::File;
use std::io::{self, Read};
use std::time::{Duration, Instant};

use crossterm::event::Event;

use super::framing::{Decoded, Parser};
use super::{HostFault, SessionStop, terminal_size, tty};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HostAction {
    Input(Event),
    PasteRejected,
}

pub(crate) struct HostReader {
    file: File,
    parser: Parser,
    stop: SessionStop,
    last_size: (u16, u16),
    colors: super::colors::HostColorQuery,
    pub(crate) keyboard_supported: bool,
    pub(crate) device_attributes: bool,
}

impl HostReader {
    pub(crate) fn new(
        file: File,
        stop: SessionStop,
        colors: super::colors::HostColorQuery,
    ) -> io::Result<Self> {
        let last_size = terminal_size(&file)?;
        Ok(Self {
            file,
            colors,
            parser: Parser::new(stop.clone()),
            stop,
            last_size,
            keyboard_supported: false,
            device_attributes: false,
        })
    }

    pub(crate) fn check(&mut self) -> io::Result<()> {
        self.parser.tick(Instant::now());
        match self.stop.error() {
            Some(error) => Err(io::Error::other(error)),
            None => Ok(()),
        }
    }

    pub(crate) fn discard_batch(&mut self, timeout: Duration) -> io::Result<bool> {
        tty::poll(&self.file, libc::POLLIN, timeout)?;
        let mut buffer = [0; 1024];
        match self.file.read(&mut buffer) {
            Ok(count) => Ok(count != 0),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn next_batch(&mut self, timeout: Duration) -> io::Result<Vec<HostAction>> {
        self.check()?;
        tty::poll(
            &self.file,
            libc::POLLIN,
            timeout.min(Duration::from_millis(10)),
        )?;
        self.check()?;
        let mut buffer = [0; 1024];
        let mut out = Vec::new();
        let count = match self.file.read(&mut buffer) {
            Ok(0) => {
                self.stop.fail(HostFault::InputClosed);
                return Err(io::Error::other("host terminal input closed"));
            }
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                0
            }
            Err(error) => {
                self.stop.fail(HostFault::InputFailed);
                return Err(error);
            }
        };
        for event in self
            .parser
            .advance(&buffer[..count], count == buffer.len(), Instant::now())
        {
            match event {
                Decoded::Event(event) => out.push(HostAction::Input(event)),
                Decoded::Color(reply) => self.colors.record(reply, Instant::now()),
                Decoded::PasteRejected => out.push(HostAction::PasteRejected),
                Decoded::KeyboardSupported => self.keyboard_supported = true,
                Decoded::DeviceAttributes => self.device_attributes = true,
            }
        }
        self.check()?;
        let size = terminal_size(&self.file)?;
        if size != self.last_size {
            self.last_size = size;
            out.push(HostAction::Input(Event::Resize(size.0, size.1)));
        }
        Ok(out)
    }
}
