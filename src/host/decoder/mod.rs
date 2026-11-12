// Crossterm 0.29.0, src/event/sys/unix/parse.rs.
// Copyright (c) 2019 Timon. MIT license in LICENSE.crossterm.
// Upstream commit 36d95b26a26e64b0f8c12edfe11f410a6d56a812.

use std::io;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, KeyboardEnhancementFlags,
    MediaKeyCode, ModifierKeyCode, MouseButton, MouseEvent, MouseEventKind,
};

mod keys;
mod mouse;
use keys::*;
use mouse::*;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum InternalEvent {
    Event(Event),
    PasteStart,
    KeyboardEnhancementFlags(KeyboardEnhancementFlags),
    PrimaryDeviceAttributes,
    CursorPosition(u16, u16),
}

// Event parsing
//
// This code (& previous one) are kind of ugly. We have to think about this,
// because it's really not maintainable, no tests, etc.
//
// Every fn returns Result<Option<InputEvent>>
//
// Ok(None) -> wait for more bytes
// Err(_) -> failed to parse event, clear the buffer
// Ok(Some(event)) -> we have event, clear the buffer
//

pub(super) fn could_not_parse_event_error() -> io::Error {
    io::Error::other("Could not parse an event.")
}

pub(crate) fn parse_event(
    buffer: &[u8],
    input_available: bool,
) -> io::Result<Option<InternalEvent>> {
    if buffer.is_empty() {
        return Ok(None);
    }

    match buffer[0] {
        b'\x1B' => {
            if buffer.len() == 1 {
                if input_available {
                    // Possible Esc sequence
                    Ok(None)
                } else {
                    Ok(Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into()))))
                }
            } else {
                match buffer[1] {
                    b'O' => {
                        if buffer.len() == 2 {
                            Ok(None)
                        } else {
                            match buffer[2] {
                                b'D' => {
                                    Ok(Some(InternalEvent::Event(Event::Key(KeyCode::Left.into()))))
                                }
                                b'C' => Ok(Some(InternalEvent::Event(Event::Key(
                                    KeyCode::Right.into(),
                                )))),
                                b'A' => {
                                    Ok(Some(InternalEvent::Event(Event::Key(KeyCode::Up.into()))))
                                }
                                b'B' => {
                                    Ok(Some(InternalEvent::Event(Event::Key(KeyCode::Down.into()))))
                                }
                                b'H' => {
                                    Ok(Some(InternalEvent::Event(Event::Key(KeyCode::Home.into()))))
                                }
                                b'F' => {
                                    Ok(Some(InternalEvent::Event(Event::Key(KeyCode::End.into()))))
                                }
                                // F1-F4
                                val @ b'P'..=b'S' => Ok(Some(InternalEvent::Event(Event::Key(
                                    KeyCode::F(1 + val - b'P').into(),
                                )))),
                                _ => Err(could_not_parse_event_error()),
                            }
                        }
                    }
                    b'[' => parse_csi(buffer),
                    b'\x1B' => Ok(Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into())))),
                    _ => parse_event(&buffer[1..], input_available).map(|event_option| {
                        event_option.map(|event| {
                            if let InternalEvent::Event(Event::Key(key_event)) = event {
                                let mut alt_key_event = key_event;
                                alt_key_event.modifiers |= KeyModifiers::ALT;
                                InternalEvent::Event(Event::Key(alt_key_event))
                            } else {
                                event
                            }
                        })
                    }),
                }
            }
        }
        b'\r' => Ok(Some(InternalEvent::Event(Event::Key(
            KeyCode::Enter.into(),
        )))),
        // Issue #371: \n = 0xA, which is also the keycode for Ctrl+J. The only reason we get
        // newlines as input is because the terminal converts \r into \n for us. When we
        // enter raw mode, we disable that, so \n no longer has any meaning - it's better to
        // use Ctrl+J. Waiting to handle it here means it gets picked up later
        b'\n' if false => Ok(Some(InternalEvent::Event(Event::Key(
            KeyCode::Enter.into(),
        )))),
        b'\t' => Ok(Some(InternalEvent::Event(Event::Key(KeyCode::Tab.into())))),
        b'\x7F' => Ok(Some(InternalEvent::Event(Event::Key(
            KeyCode::Backspace.into(),
        )))),
        c @ b'\x01'..=b'\x1A' => Ok(Some(InternalEvent::Event(Event::Key(KeyEvent::new(
            KeyCode::Char((c - 0x1 + b'a') as char),
            KeyModifiers::CONTROL,
        ))))),
        c @ b'\x1C'..=b'\x1F' => Ok(Some(InternalEvent::Event(Event::Key(KeyEvent::new(
            KeyCode::Char((c - 0x1C + b'4') as char),
            KeyModifiers::CONTROL,
        ))))),
        b'\0' => Ok(Some(InternalEvent::Event(Event::Key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::CONTROL,
        ))))),
        _ => parse_utf8_char(buffer).map(|maybe_char| {
            maybe_char
                .map(KeyCode::Char)
                .map(char_code_to_event)
                .map(Event::Key)
                .map(InternalEvent::Event)
        }),
    }
}

// converts KeyCode to KeyEvent (adds shift modifier in case of uppercase characters)
fn char_code_to_event(code: KeyCode) -> KeyEvent {
    let modifiers = match code {
        KeyCode::Char(c) if c.is_uppercase() => KeyModifiers::SHIFT,
        _ => KeyModifiers::empty(),
    };
    KeyEvent::new(code, modifiers)
}

pub(crate) fn parse_csi(buffer: &[u8]) -> io::Result<Option<InternalEvent>> {
    assert!(buffer.starts_with(b"\x1B[")); // ESC [

    if buffer.len() == 2 {
        return Ok(None);
    }

    let input_event = match buffer[2] {
        b'[' => {
            if buffer.len() == 3 {
                None
            } else {
                match buffer[3] {
                    // NOTE (@imdaveho): cannot find when this occurs;
                    // having another '[' after ESC[ not a likely scenario
                    val @ b'A'..=b'E' => Some(Event::Key(KeyCode::F(1 + val - b'A').into())),
                    _ => return Err(could_not_parse_event_error()),
                }
            }
        }
        b'D' => Some(Event::Key(KeyCode::Left.into())),
        b'C' => Some(Event::Key(KeyCode::Right.into())),
        b'A' => Some(Event::Key(KeyCode::Up.into())),
        b'B' => Some(Event::Key(KeyCode::Down.into())),
        b'H' => Some(Event::Key(KeyCode::Home.into())),
        b'F' => Some(Event::Key(KeyCode::End.into())),
        b'Z' => Some(Event::Key(KeyEvent::new_with_kind(
            KeyCode::BackTab,
            KeyModifiers::SHIFT,
            KeyEventKind::Press,
        ))),
        b'M' => return parse_csi_normal_mouse(buffer),
        b'<' => return parse_csi_sgr_mouse(buffer),
        b'I' => Some(Event::FocusGained),
        b'O' => Some(Event::FocusLost),
        b';' => return parse_csi_modifier_key_code(buffer),
        // P, Q, and S for compatibility with Kitty keyboard protocol,
        // as the 1 in 'CSI 1 P' etc. must be omitted if there are no
        // modifiers pressed:
        // https://sw.kovidgoyal.net/kitty/keyboard-protocol/#legacy-functional-keys
        b'P' => Some(Event::Key(KeyCode::F(1).into())),
        b'Q' => Some(Event::Key(KeyCode::F(2).into())),
        b'S' => Some(Event::Key(KeyCode::F(4).into())),
        b'?' => match buffer[buffer.len() - 1] {
            b'u' => return parse_csi_keyboard_enhancement_flags(buffer),
            b'c' => return parse_csi_primary_device_attributes(buffer),
            _ => None,
        },
        b'0'..=b'9' => {
            // Numbered escape code.
            if buffer.len() == 3 {
                None
            } else {
                // The final byte of a CSI sequence can be in the range 64-126, so
                // let's keep reading anything else.
                let last_byte = buffer[buffer.len() - 1];
                if !(64..=126).contains(&last_byte) {
                    None
                } else {
                    if buffer.starts_with(b"\x1B[200~") {
                        return parse_csi_bracketed_paste(buffer);
                    }
                    match last_byte {
                        b'M' => return parse_csi_rxvt_mouse(buffer),
                        b'~' => return parse_csi_special_key_code(buffer),
                        b'u' => return parse_csi_u_encoded_key_code(buffer),
                        b'R' => return parse_csi_cursor_position(buffer),
                        _ => return parse_csi_modifier_key_code(buffer),
                    }
                }
            }
        }
        _ => return Err(could_not_parse_event_error()),
    };

    Ok(input_event.map(InternalEvent::Event))
}

pub(crate) fn next_parsed<T>(iter: &mut dyn Iterator<Item = &str>) -> io::Result<T>
where
    T: std::str::FromStr,
{
    iter.next()
        .ok_or_else(could_not_parse_event_error)?
        .parse::<T>()
        .map_err(|_| could_not_parse_event_error())
}

fn modifier_and_kind_parsed(iter: &mut dyn Iterator<Item = &str>) -> io::Result<(u8, u8)> {
    let mut sub_split = iter
        .next()
        .ok_or_else(could_not_parse_event_error)?
        .split(':');

    let modifier_mask = next_parsed::<u8>(&mut sub_split)?;

    if let Ok(kind_code) = next_parsed::<u8>(&mut sub_split) {
        Ok((modifier_mask, kind_code))
    } else {
        Ok((modifier_mask, 1))
    }
}

pub(crate) fn parse_csi_cursor_position(buffer: &[u8]) -> io::Result<Option<InternalEvent>> {
    // ESC [ Cy ; Cx R
    //   Cy - cursor row number (starting from 1)
    //   Cx - cursor column number (starting from 1)
    assert!(buffer.starts_with(b"\x1B[")); // ESC [
    assert!(buffer.ends_with(b"R"));

    let s = std::str::from_utf8(&buffer[2..buffer.len() - 1])
        .map_err(|_| could_not_parse_event_error())?;

    let mut split = s.split(';');

    let y = next_parsed::<u16>(&mut split)?
        .checked_sub(1)
        .ok_or_else(could_not_parse_event_error)?;
    let x = next_parsed::<u16>(&mut split)?
        .checked_sub(1)
        .ok_or_else(could_not_parse_event_error)?;

    Ok(Some(InternalEvent::CursorPosition(x, y)))
}

fn parse_csi_keyboard_enhancement_flags(buffer: &[u8]) -> io::Result<Option<InternalEvent>> {
    // ESC [ ? flags u
    assert!(buffer.starts_with(b"\x1B[?")); // ESC [ ?
    assert!(buffer.ends_with(b"u"));

    if buffer.len() < 5 {
        return Ok(None);
    }

    let bits = buffer[3];
    let mut flags = KeyboardEnhancementFlags::empty();

    if bits & 1 != 0 {
        flags |= KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;
    }
    if bits & 2 != 0 {
        flags |= KeyboardEnhancementFlags::REPORT_EVENT_TYPES;
    }
    if bits & 4 != 0 {
        flags |= KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS;
    }
    if bits & 8 != 0 {
        flags |= KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES;
    }
    // *Note*: this is not yet supported by crossterm.
    // if bits & 16 != 0 {
    //     flags |= KeyboardEnhancementFlags::REPORT_ASSOCIATED_TEXT;
    // }

    Ok(Some(InternalEvent::KeyboardEnhancementFlags(flags)))
}

fn parse_csi_primary_device_attributes(buffer: &[u8]) -> io::Result<Option<InternalEvent>> {
    // ESC [ 64 ; attr1 ; attr2 ; ... ; attrn ; c
    assert!(buffer.starts_with(b"\x1B[?"));
    assert!(buffer.ends_with(b"c"));

    // This is a stub for parsing the primary device attributes. This response is not
    // exposed in the crossterm API so we don't need to parse the individual attributes yet.
    // See <https://vt100.net/docs/vt510-rm/DA1.html>

    Ok(Some(InternalEvent::PrimaryDeviceAttributes))
}
