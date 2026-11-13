use std::time::{Duration, Instant};

use crossterm::event::Event;

use super::colors::ColorReply;
use super::decoder::{InternalEvent, parse_event};
use super::osc_reply::OscReply;
use super::{HostFault, SessionStop};
use crate::terminal::PASTE_LIMIT_BYTES;

pub(super) const PASTE_FRAME_LIMIT: Duration = Duration::from_secs(30);
const ESCAPE_WAIT: Duration = Duration::from_millis(25);
const SEQUENCE_LIMIT: usize = 1024;
const END: &[u8] = b"\x1b[201~";

pub(super) struct Parser {
    sequence: Vec<u8>,
    state: State,
    stop: SessionStop,
    escape_since: Option<Instant>,
}

enum State {
    Normal,
    Osc(OscReply),
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
    Color(ColorReply),
    KeyboardSupported,
    DeviceAttributes,
}

impl Parser {
    pub(super) fn new(stop: SessionStop) -> Self {
        Self {
            sequence: Vec::new(),
            state: State::Normal,
            stop,
            escape_since: None,
        }
    }

    pub(super) fn tick(&mut self, now: Instant) {
        if matches!(self.state, State::Collecting { deadline, .. } | State::Discarding { deadline, .. } if now >= deadline)
        {
            self.end(HostFault::UnterminatedPaste);
        }
        if matches!(&self.state, State::Osc(frame) if frame.expired(now)) {
            self.end(HostFault::UnterminatedColorReply);
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
        if self
            .escape_since
            .is_some_and(|since| now >= since + ESCAPE_WAIT)
        {
            self.release_keys(now, &mut out);
        }
        for (index, &byte) in bytes.iter().enumerate() {
            match &mut self.state {
                State::Ended => break,
                State::Osc(frame) => {
                    if let Some(replies) = frame.advance(byte) {
                        out.extend(replies.into_iter().map(Decoded::Color));
                        self.state = State::Normal;
                    }
                }
                State::Normal => {
                    self.escape_since = None;
                    if self.sequence.len() == SEQUENCE_LIMIT {
                        self.end(HostFault::SequenceTooLong);
                        break;
                    }
                    self.sequence.push(byte);
                    if self.sequence.starts_with(b"\x1b]") {
                        let prefix = &self.sequence[2..];
                        let commands: &[&[u8]] = &[b"4;", b"10;", b"11;"];
                        if commands.contains(&prefix) {
                            let mut frame = OscReply::new(now);
                            for &byte in prefix {
                                frame.advance(byte);
                            }
                            self.sequence.clear();
                            self.state = State::Osc(frame);
                        } else if commands.iter().any(|command| command.starts_with(prefix)) {
                            self.escape_since = Some(now);
                        } else {
                            self.release_keys(now, &mut out);
                        }
                        continue;
                    }
                    if self.sequence == b"\x1b" {
                        self.escape_since = Some(now);
                        continue;
                    }
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

    fn release_keys(&mut self, now: Instant, out: &mut Vec<Decoded>) {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let bytes = std::mem::take(&mut self.sequence);
        self.escape_since = None;
        if bytes == b"\x1b" {
            out.push(Decoded::Event(Event::Key(KeyCode::Esc.into())));
        } else {
            out.push(Decoded::Event(Event::Key(KeyEvent::new(
                KeyCode::Char(']'),
                KeyModifiers::ALT,
            ))));
            out.extend(self.advance(&bytes[2..], false, now));
        }
    }
}

#[cfg(test)]
mod tests;
