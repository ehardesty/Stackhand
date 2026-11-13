use std::time::{Duration, Instant};

use crossterm::event::Event;

use super::decoder::{InternalEvent, parse_event};
use super::{HostFault, SessionStop};
use crate::terminal::PASTE_LIMIT_BYTES;

pub(super) const PASTE_FRAME_LIMIT: Duration = Duration::from_secs(30);
const SEQUENCE_LIMIT: usize = 1024;
const END: &[u8] = b"\x1b[201~";

pub(super) struct Parser {
    sequence: Vec<u8>,
    state: State,
    stop: SessionStop,
}

enum State {
    Normal,
    Collecting {
        bytes: Vec<u8>,
        matched: usize,
        deadline: Instant,
    },
    Discarding {
        matched: usize,
        deadline: Instant,
    },
    Ended,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decoded {
    Event(Event),
    PasteRejected,
    KeyboardSupported,
    DeviceAttributes,
}

impl Parser {
    pub(super) fn new(stop: SessionStop) -> Self {
        Self {
            sequence: Vec::new(),
            state: State::Normal,
            stop,
        }
    }

    pub(super) fn tick(&mut self, now: Instant) {
        if matches!(self.state, State::Collecting { deadline, .. } | State::Discarding { deadline, .. } if now >= deadline)
        {
            self.end(HostFault::UnterminatedPaste);
        }
    }

    fn end(&mut self, fault: HostFault) {
        self.state = State::Ended;
        self.sequence.clear();
        self.stop.fail(fault);
    }

    pub(super) fn advance(&mut self, bytes: &[u8], more: bool, now: Instant) -> Vec<Decoded> {
        self.tick(now);
        let mut out = Vec::new();
        for (index, &byte) in bytes.iter().enumerate() {
            match &mut self.state {
                State::Ended => break,
                State::Normal => {
                    if self.sequence.len() == SEQUENCE_LIMIT {
                        self.end(HostFault::SequenceTooLong);
                        break;
                    }
                    self.sequence.push(byte);
                    match parse_event(&self.sequence, index + 1 < bytes.len() || more) {
                        Ok(Some(event)) => {
                            self.sequence.clear();
                            match event {
                                InternalEvent::PasteStart => {
                                    self.state = State::Collecting {
                                        bytes: Vec::with_capacity(PASTE_LIMIT_BYTES),
                                        matched: 0,
                                        deadline: now + PASTE_FRAME_LIMIT,
                                    }
                                }
                                InternalEvent::Event(event) => out.push(Decoded::Event(event)),
                                InternalEvent::KeyboardEnhancementFlags(_) => {
                                    out.push(Decoded::KeyboardSupported)
                                }
                                InternalEvent::PrimaryDeviceAttributes => {
                                    out.push(Decoded::DeviceAttributes)
                                }
                                InternalEvent::CursorPosition(_, _) => {}
                            }
                        }
                        Ok(None) => {}
                        Err(_) => self.sequence.clear(),
                    }
                }
                State::Collecting {
                    bytes,
                    matched,
                    deadline,
                } => {
                    if byte == END[*matched] {
                        *matched += 1;
                        if *matched == END.len() {
                            let text = String::from_utf8_lossy(bytes).into_owned();
                            out.push(if text.len() <= PASTE_LIMIT_BYTES {
                                Decoded::Event(Event::Paste(text))
                            } else {
                                Decoded::PasteRejected
                            });
                            self.state = State::Normal;
                        }
                    } else {
                        let prefix = *matched;
                        *matched = usize::from(byte == END[0]);
                        let extra = prefix + usize::from(*matched == 0);
                        if bytes.len() + extra > PASTE_LIMIT_BYTES {
                            self.state = State::Discarding {
                                matched: *matched,
                                deadline: *deadline,
                            };
                            out.push(Decoded::PasteRejected);
                        } else {
                            bytes.extend_from_slice(&END[..prefix]);
                            if *matched == 0 {
                                bytes.push(byte);
                            }
                        }
                    }
                }
                State::Discarding { matched, .. } => {
                    if byte == END[*matched] {
                        *matched += 1;
                        if *matched == END.len() {
                            self.state = State::Normal;
                        }
                    } else {
                        *matched = usize::from(byte == END[0]);
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests;
