use std::time::{Duration, Instant};

use super::colors::{ColorReply, parse_reply};

const REPLY_LIMIT: usize = 16 * 1024;
pub(super) const REPLY_FRAME_LIMIT: Duration = Duration::from_secs(30);

pub(super) struct OscReply {
    bytes: Vec<u8>,
    escaped: bool,
    state: FrameState,
    deadline: Instant,
}

enum FrameState {
    Collecting,
    Discarding,
}

impl OscReply {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            bytes: Vec::new(),
            escaped: false,
            state: FrameState::Collecting,
            deadline: now + REPLY_FRAME_LIMIT,
        }
    }

    pub(super) fn expired(&self, now: Instant) -> bool {
        now >= self.deadline
    }

    pub(super) fn advance(&mut self, byte: u8) -> Option<Vec<ColorReply>> {
        if byte == 7 || (self.escaped && byte == b'\\') {
            return Some(match self.state {
                FrameState::Collecting => parse_reply(&self.bytes),
                FrameState::Discarding => Vec::new(),
            });
        }
        if self.escaped || self.bytes.len() == REPLY_LIMIT || !byte.is_ascii() {
            self.state = FrameState::Discarding;
            self.bytes.clear();
        }
        self.escaped = byte == 0x1b;
        if matches!(self.state, FrameState::Collecting) && !self.escaped {
            self.bytes.push(byte);
        }
        None
    }
}
