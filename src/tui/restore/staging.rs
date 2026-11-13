use std::cell::RefCell;
use std::io::{self, Write};
use std::rc::Rc;

use crate::host::output::STAGING_LIMIT;

#[derive(Clone, Default)]
pub(super) struct CommandStager(Rc<RefCell<Staging>>);

#[derive(Default)]
struct Staging {
    bytes: Vec<u8>,
    state: State,
}

#[derive(Default)]
enum State {
    #[default]
    Open,
    Failed,
    Closed,
}

impl CommandStager {
    pub(super) fn take(&self) -> io::Result<Vec<u8>> {
        let mut staging = self.0.borrow_mut();
        match staging.state {
            State::Open => Ok(std::mem::take(&mut staging.bytes)),
            _ => Err(io::Error::other("terminal command staging is closed")),
        }
    }

    pub(super) fn close(&self) {
        let mut staging = self.0.borrow_mut();
        staging.state = State::Closed;
        staging.bytes.clear();
    }
}

impl Write for CommandStager {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut staging = self.0.borrow_mut();
        match staging.state {
            State::Closed => return Ok(bytes.len()),
            State::Failed => return Err(io::Error::other("terminal command staging failed")),
            State::Open => {}
        }
        if bytes.len() > STAGING_LIMIT.saturating_sub(staging.bytes.len()) {
            staging.state = State::Failed;
            staging.bytes.clear();
            return Err(io::Error::other(
                "terminal transaction exceeds the 16 MiB staging limit",
            ));
        }
        staging.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if matches!(self.0.borrow().state, State::Failed) {
            Err(io::Error::other("terminal command staging failed"))
        } else {
            Ok(())
        }
    }
}
