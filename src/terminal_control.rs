//! Streaming plain-text projection of terminal output. Control payloads are discarded
//! without buffering; cursor movement is interpreted only by the Terminal view.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ParserState {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    StringSequence,
    StringEscape,
}

/// A finite-state terminal-control filter. It stores no control payload.
#[derive(Default)]
pub(crate) struct ControlParser {
    state: ParserState,
    skip_lf_after_cr: bool,
    utf8_continuations: u8,
}

impl ControlParser {
    /// Consume one raw byte and return one visible matching byte when present.
    pub(crate) fn consume(&mut self, byte: u8) -> Option<u8> {
        if self.state == ParserState::Ground && self.utf8_continuations > 0 {
            if (0x80..=0xbf).contains(&byte) {
                self.utf8_continuations -= 1;
                return Some(byte);
            }
            self.utf8_continuations = 0;
        }

        if self.state == ParserState::Ground && self.skip_lf_after_cr {
            self.skip_lf_after_cr = false;
            if byte == b'\n' {
                return None;
            }
        }

        match self.state {
            ParserState::Ground => self.consume_ground(byte),
            ParserState::Escape => {
                self.state = match byte {
                    b'[' => ParserState::Csi,
                    b']' | b'P' | b'X' | b'^' | b'_' => ParserState::StringSequence,
                    0x20..=0x2f => ParserState::EscapeIntermediate,
                    _ => ParserState::Ground,
                };
                None
            }
            ParserState::EscapeIntermediate => {
                if byte == 0x1b {
                    self.state = ParserState::Escape;
                } else if (0x30..=0x7e).contains(&byte) {
                    self.state = ParserState::Ground;
                }
                None
            }
            ParserState::Csi => {
                if byte == 0x1b {
                    self.state = ParserState::Escape;
                } else if byte == 0x9c || (0x40..=0x7e).contains(&byte) {
                    self.state = ParserState::Ground;
                }
                None
            }
            ParserState::StringSequence => {
                if byte == 0x07 {
                    self.state = ParserState::Ground;
                } else if byte == 0x1b {
                    self.state = ParserState::StringEscape;
                } else if byte == 0x9c {
                    self.state = ParserState::Ground;
                }
                None
            }
            ParserState::StringEscape => {
                self.state = if byte == b'\\' || byte == 0x9c {
                    ParserState::Ground
                } else if byte == 0x1b {
                    ParserState::StringEscape
                } else {
                    ParserState::StringSequence
                };
                None
            }
        }
    }

    fn consume_ground(&mut self, byte: u8) -> Option<u8> {
        match byte {
            0x1b => {
                self.state = ParserState::Escape;
                None
            }
            0x9b => {
                self.state = ParserState::Csi;
                None
            }
            0x90 | 0x98 | 0x9d | 0x9e | 0x9f => {
                self.state = ParserState::StringSequence;
                None
            }
            0x9c => None,
            b'\r' => {
                self.skip_lf_after_cr = true;
                Some(b'\n')
            }
            b'\n' | b'\t' => Some(byte),
            0x20..=0x7e => Some(byte),
            0xc2..=0xf4 => {
                self.utf8_continuations = if byte <= 0xdf {
                    1
                } else if byte <= 0xef {
                    2
                } else {
                    3
                };
                Some(byte)
            }
            byte if byte >= 0x80 => Some(byte),
            _ => None,
        }
    }
}
