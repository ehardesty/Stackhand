use super::*;
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

fn key(code: KeyCode, modifiers: KeyModifiers) -> Decoded {
    Decoded::Event(Event::Key(KeyEvent::new(code, modifiers)))
}

#[test]
fn exact_limit_paste_survives_fragmented_markers_and_failed_near_matches() {
    let body = format!(
        "{}\x1b[20x\x1b\x1b[201x\r\x03é",
        "a".repeat(PASTE_LIMIT_BYTES - 20)
    );
    let body = format!("{}{}", body, "z".repeat(PASTE_LIMIT_BYTES - body.len()));
    let bytes = [b"\x1b[200~".as_slice(), body.as_bytes(), END, b"x"].concat();
    for chunk in [1, 2, 3, 5, 7, 1024] {
        let mut parser = Parser::new(SessionStop::default());
        let mut out = Vec::new();
        let now = Instant::now();
        for part in bytes.chunks(chunk) {
            out.extend(parser.advance(part, true, now));
        }
        assert_eq!(
            out,
            vec![
                Decoded::Event(Event::Paste(body.clone())),
                key(KeyCode::Char('x'), KeyModifiers::NONE)
            ]
        );
    }
}

#[test]
fn oversize_is_atomic_and_suffix_stays_quarantined_through_all_marker_splits() {
    for split in 0..=END.len() {
        let mut parser = Parser::new(SessionStop::default());
        let now = Instant::now();
        let mut out = parser.advance(b"\x1b[200~", true, now);
        out.extend(parser.advance(&vec![b'p'; PASTE_LIMIT_BYTES], true, now));
        out.extend(parser.advance(b"\nexecute-me\r\x03\x1b[201x\x1b[200~", true, now));
        for _ in 0..128 {
            out.extend(parser.advance(&[b'q'; 1024], true, now));
        }
        assert!(matches!(parser.state, State::Discarding { .. }));
        out.extend(parser.advance(&END[..split], true, now));
        out.extend(parser.advance(&[&END[split..], b"x"].concat(), false, now));
        assert_eq!(
            out,
            vec![
                Decoded::PasteRejected,
                key(KeyCode::Char('x'), KeyModifiers::NONE)
            ]
        );
    }
}

#[test]
fn absolute_deadline_ends_collecting_and_discarding_permanently() {
    for oversized in [false, true] {
        let stop = SessionStop::default();
        let mut parser = Parser::new(stop.clone());
        let now = Instant::now();
        parser.advance(b"\x1b[200~", true, now);
        if oversized {
            parser.advance(&vec![b'p'; PASTE_LIMIT_BYTES + 1], true, now);
        }
        assert!(!stop.requested());
        parser.advance(
            b"still arriving",
            true,
            now + PASTE_FRAME_LIMIT - Duration::from_millis(1),
        );
        parser.tick(now + PASTE_FRAME_LIMIT);
        assert!(stop.error().is_some());
        assert!(
            parser
                .advance(b"execute\r\x03\x1b[201~x", false, now + PASTE_FRAME_LIMIT)
                .is_empty()
        );
        parser.tick(now + PASTE_FRAME_LIMIT * 2);
        assert!(matches!(parser.state, State::Ended));
    }
}

#[test]
fn expanded_invalid_utf8_and_unbounded_nonpaste_sequences_are_rejected() {
    let mut parser = Parser::new(SessionStop::default());
    let now = Instant::now();
    let mut bytes = b"\x1b[200~".to_vec();
    bytes.extend(vec![0xff; PASTE_LIMIT_BYTES / 2]);
    bytes.extend(END);
    assert_eq!(
        parser.advance(&bytes, false, now),
        vec![Decoded::PasteRejected]
    );
    let stop = SessionStop::default();
    let mut parser = Parser::new(stop.clone());
    let bytes = [b"\x1b[?".as_slice(), &vec![b'1'; SEQUENCE_LIMIT * 2]].concat();
    assert!(parser.advance(&bytes, true, now).is_empty());
    assert!(stop.error().is_some());
    assert!(parser.advance(b"uhello", false, now).is_empty());
}

#[test]
fn representative_crossterm_protocol_vectors_preserve_events() {
    let vectors: Vec<(&[u8], Decoded)> = vec![
        (b"\x03", key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        (b"\x1bx", key(KeyCode::Char('x'), KeyModifiers::ALT)),
        ("é".as_bytes(), key(KeyCode::Char('é'), KeyModifiers::NONE)),
        (b"\x1b[1;5A", key(KeyCode::Up, KeyModifiers::CONTROL)),
        (b"\x1bOP", key(KeyCode::F(1), KeyModifiers::NONE)),
        (
            b"\x1b[97;3:2u",
            Decoded::Event(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::ALT,
                KeyEventKind::Repeat,
            ))),
        ),
        (
            b"\x1b[97;1:3u",
            Decoded::Event(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ))),
        ),
        (
            b"\x1b[97:65;2u",
            key(KeyCode::Char('A'), KeyModifiers::NONE),
        ),
        (b"\x1b[I", Decoded::Event(Event::FocusGained)),
        (b"\x1b[O", Decoded::Event(Event::FocusLost)),
        (
            b"\x1b[<0;4;5M",
            Decoded::Event(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 3,
                row: 4,
                modifiers: KeyModifiers::NONE,
            })),
        ),
        (
            b"\x1b[<64;4;5M",
            Decoded::Event(Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 3,
                row: 4,
                modifiers: KeyModifiers::NONE,
            })),
        ),
        (b"\x1b[?3u", Decoded::KeyboardSupported),
        (b"\x1b[?1;2c", Decoded::DeviceAttributes),
    ];
    for (bytes, expected) in vectors {
        let mut parser = Parser::new(SessionStop::default());
        assert_eq!(
            parser.advance(bytes, false, Instant::now()),
            vec![expected],
            "{bytes:?}"
        );
    }
    for bytes in [
        b"\x1b[0;0R".as_slice(),
        b"\x1b[<0;0;0M",
        b"\x1b[M\x20\x00\x00",
    ] {
        let mut parser = Parser::new(SessionStop::default());
        assert!(parser.advance(bytes, false, Instant::now()).is_empty());
    }
}

#[test]
fn fragmented_color_replies_are_private_and_paste_keeps_literal_osc() {
    let now = Instant::now();
    let bytes = b"x\x1b]4;1;rgb:12/34/56;255;#abcdef\x1b\\\x1b]10;rgb:ffff/8080/0000\x07\x1b]11;#102030\x1b\\\x1b[?3u\x1b[?1;2c\x1b[200~\x1b]10;#ffffff\x07\x03\x1b[201~\x03y";
    for chunk in [1, 2, 3, 5, 1024] {
        let mut parser = Parser::new(SessionStop::default());
        let out = bytes
            .chunks(chunk)
            .flat_map(|part| parser.advance(part, false, now))
            .collect::<Vec<_>>();
        use super::super::colors::ColorReply::*;
        use libghostty_vt::style::RgbColor;
        assert_eq!(
            out,
            vec![
                key(KeyCode::Char('x'), KeyModifiers::NONE),
                Decoded::Color(Indexed(
                    1,
                    RgbColor {
                        r: 0x12,
                        g: 0x34,
                        b: 0x56
                    }
                )),
                Decoded::Color(Indexed(
                    255,
                    RgbColor {
                        r: 0xab,
                        g: 0xcd,
                        b: 0xef
                    }
                )),
                Decoded::Color(Foreground(RgbColor {
                    r: 255,
                    g: 128,
                    b: 0
                })),
                Decoded::Color(Background(RgbColor {
                    r: 16,
                    g: 32,
                    b: 48
                })),
                Decoded::KeyboardSupported,
                Decoded::DeviceAttributes,
                Decoded::Event(Event::Paste("\x1b]10;#ffffff\x07\x03".to_string())),
                key(KeyCode::Char('c'), KeyModifiers::CONTROL),
                key(KeyCode::Char('y'), KeyModifiers::NONE),
            ],
            "chunk {chunk}"
        );
    }
}

#[test]
fn late_malformed_and_oversized_osc_never_replay_as_keys() {
    let now = Instant::now();
    let stop = SessionStop::default();
    let mut parser = Parser::new(stop.clone());
    let mut out = parser.advance(
        b"\x1b]10;invalid\x07\x1b]4;999;rgb:ff/ff/ff\x1b\\\x1b]11;",
        false,
        now,
    );
    out.extend(parser.advance(&vec![b'x'; 32 * 1024], false, now));
    out.extend(parser.advance(b"execute\r\x03\x1b\\a", false, now + Duration::from_secs(5)));
    assert_eq!(out, vec![key(KeyCode::Char('a'), KeyModifiers::NONE)]);
    assert!(!stop.requested());
    parser.advance(b"\x1b]4;0;", false, now);
    parser.tick(now + super::super::osc_reply::REPLY_FRAME_LIMIT);
    assert!(stop.framing_fault());
    assert!(
        parser
            .advance(b"run\r\x07z", false, now + Duration::from_secs(31))
            .is_empty()
    );
}

#[test]
fn standalone_escape_is_delivered_after_disambiguation() {
    let now = Instant::now();
    let mut parser = Parser::new(SessionStop::default());
    assert!(parser.advance(b"\x1b", false, now).is_empty());
    assert_eq!(
        parser.advance(b"", false, now + ESCAPE_WAIT),
        vec![key(KeyCode::Esc, KeyModifiers::NONE)]
    );
}

#[test]
fn legacy_alt_bracket_keeps_following_keys_and_ctrl_c_out_of_reply_quarantine() {
    let now = Instant::now();
    for chunk in [1, 2, 3, 20] {
        let stop = SessionStop::default();
        let mut parser = Parser::new(stop.clone());
        let out = b"\x1b]x\x03\x1b]1z"
            .chunks(chunk)
            .flat_map(|part| parser.advance(part, false, now))
            .collect::<Vec<_>>();
        assert_eq!(
            out,
            vec![
                key(KeyCode::Char(']'), KeyModifiers::ALT),
                key(KeyCode::Char('x'), KeyModifiers::NONE),
                key(KeyCode::Char('c'), KeyModifiers::CONTROL),
                key(KeyCode::Char(']'), KeyModifiers::ALT),
                key(KeyCode::Char('1'), KeyModifiers::NONE),
                key(KeyCode::Char('z'), KeyModifiers::NONE),
            ]
        );
        parser.tick(now + Duration::from_secs(31));
        assert!(!stop.requested());
    }
    let mut parser = Parser::new(SessionStop::default());
    assert!(parser.advance(b"\x1b]", false, now).is_empty());
    assert_eq!(
        parser.advance(b"", false, now + ESCAPE_WAIT),
        vec![key(KeyCode::Char(']'), KeyModifiers::ALT)]
    );
    assert_eq!(
        parser.advance(b"\x03", false, now + ESCAPE_WAIT),
        vec![key(KeyCode::Char('c'), KeyModifiers::CONTROL)]
    );
}
