use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use libghostty_vt::key;
use libghostty_vt::terminal::{Mode, Terminal};

pub fn encode_key(
    encoder: &mut key::Encoder,
    event: KeyEvent,
    terminal: &Terminal,
) -> Result<Vec<u8>, libghostty_vt::Error> {
    encoder.set_options_from_terminal(terminal);
    let mut input = key::Event::new()?;
    input
        .set_action(match event.kind {
            KeyEventKind::Press => key::Action::Press,
            KeyEventKind::Repeat => key::Action::Repeat,
            KeyEventKind::Release => key::Action::Release,
        })
        .set_key(key_code(event.code))
        .set_mods(key_modifiers(event.modifiers));
    if let KeyCode::Char(character) = event.code {
        input.set_utf8(Some(character.to_string()));
        input.set_unshifted_codepoint(if event.modifiers.contains(KeyModifiers::SHIFT) {
            character.to_ascii_lowercase()
        } else {
            character
        });
    }
    let mut bytes = Vec::new();
    encoder.encode_to_vec(&input, &mut bytes)?;
    Ok(bytes)
}

pub fn encode_focus(gained: bool) -> &'static [u8] {
    if gained { b"\x1b[I" } else { b"\x1b[O" }
}

pub fn encode_paste(data: &[u8], terminal: &Terminal) -> Vec<u8> {
    if terminal.mode(Mode::BRACKETED_PASTE).unwrap_or(false) {
        [b"\x1b[200~".as_slice(), data, b"\x1b[201~".as_slice()].concat()
    } else {
        data.to_vec()
    }
}

fn key_modifiers(modifiers: KeyModifiers) -> key::Mods {
    let mut result = key::Mods::empty();
    if modifiers.contains(KeyModifiers::SHIFT) {
        result |= key::Mods::SHIFT;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        result |= key::Mods::CTRL;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        result |= key::Mods::ALT;
    }
    if modifiers.contains(KeyModifiers::SUPER) {
        result |= key::Mods::SUPER;
    }
    result
}

fn key_code(code: KeyCode) -> key::Key {
    match code {
        KeyCode::Backspace => key::Key::Backspace,
        KeyCode::Enter => key::Key::Enter,
        KeyCode::Left => key::Key::ArrowLeft,
        KeyCode::Right => key::Key::ArrowRight,
        KeyCode::Up => key::Key::ArrowUp,
        KeyCode::Down => key::Key::ArrowDown,
        KeyCode::Home => key::Key::Home,
        KeyCode::End => key::Key::End,
        KeyCode::PageUp => key::Key::PageUp,
        KeyCode::PageDown => key::Key::PageDown,
        KeyCode::Tab | KeyCode::BackTab => key::Key::Tab,
        KeyCode::Delete => key::Key::Delete,
        KeyCode::Insert => key::Key::Insert,
        KeyCode::Esc => key::Key::Escape,
        KeyCode::CapsLock => key::Key::CapsLock,
        KeyCode::ScrollLock => key::Key::ScrollLock,
        KeyCode::NumLock => key::Key::NumLock,
        KeyCode::PrintScreen => key::Key::PrintScreen,
        KeyCode::Pause => key::Key::Pause,
        KeyCode::Menu => key::Key::ContextMenu,
        KeyCode::KeypadBegin => key::Key::NumpadBegin,
        KeyCode::F(number) => function_key(number),
        KeyCode::Char(character) => character_key(character),
        _ => key::Key::Unidentified,
    }
}

fn function_key(number: u8) -> key::Key {
    use key::Key;
    match number {
        1 => Key::F1,
        2 => Key::F2,
        3 => Key::F3,
        4 => Key::F4,
        5 => Key::F5,
        6 => Key::F6,
        7 => Key::F7,
        8 => Key::F8,
        9 => Key::F9,
        10 => Key::F10,
        11 => Key::F11,
        12 => Key::F12,
        13 => Key::F13,
        14 => Key::F14,
        15 => Key::F15,
        16 => Key::F16,
        17 => Key::F17,
        18 => Key::F18,
        19 => Key::F19,
        20 => Key::F20,
        21 => Key::F21,
        22 => Key::F22,
        23 => Key::F23,
        24 => Key::F24,
        25 => Key::F25,
        _ => Key::Unidentified,
    }
}

fn character_key(character: char) -> key::Key {
    use key::Key;
    match character.to_ascii_uppercase() {
        'A' => Key::A,
        'B' => Key::B,
        'C' => Key::C,
        'D' => Key::D,
        'E' => Key::E,
        'F' => Key::F,
        'G' => Key::G,
        'H' => Key::H,
        'I' => Key::I,
        'J' => Key::J,
        'K' => Key::K,
        'L' => Key::L,
        'M' => Key::M,
        'N' => Key::N,
        'O' => Key::O,
        'P' => Key::P,
        'Q' => Key::Q,
        'R' => Key::R,
        'S' => Key::S,
        'T' => Key::T,
        'U' => Key::U,
        'V' => Key::V,
        'W' => Key::W,
        'X' => Key::X,
        'Y' => Key::Y,
        'Z' => Key::Z,
        '0' => Key::Digit0,
        '1' => Key::Digit1,
        '2' => Key::Digit2,
        '3' => Key::Digit3,
        '4' => Key::Digit4,
        '5' => Key::Digit5,
        '6' => Key::Digit6,
        '7' => Key::Digit7,
        '8' => Key::Digit8,
        '9' => Key::Digit9,
        ' ' => Key::Space,
        '`' => Key::Backquote,
        '\\' => Key::Backslash,
        '[' => Key::BracketLeft,
        ']' => Key::BracketRight,
        ',' => Key::Comma,
        '=' => Key::Equal,
        '-' => Key::Minus,
        '.' => Key::Period,
        '\'' => Key::Quote,
        ';' => Key::Semicolon,
        '/' => Key::Slash,
        _ => Key::Unidentified,
    }
}
