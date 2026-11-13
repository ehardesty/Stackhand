use std::cell::RefCell;
use std::rc::Rc;

use libghostty_vt::color::{Palette, PaletteIndex, Rgb};
use libghostty_vt::render::RenderState;
use libghostty_vt::terminal::Terminal;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

use super::HostColorProfile;
use crate::terminal::{new_terminal_with_colors, render};

fn profile() -> HostColorProfile {
    let mut profile = HostColorProfile {
        foreground: Some(Rgb {
            r: 18,
            g: 52,
            b: 86,
        }),
        background: Some(Rgb {
            r: 33,
            g: 67,
            b: 101,
        }),
        ..Default::default()
    };
    for (index, color) in profile.indexed.iter_mut().enumerate() {
        *color = Some(Rgb {
            r: index as u8,
            g: 27,
            b: 73,
        });
    }
    profile
}

fn buffer(terminal: &Terminal<'_, '_>) -> Buffer {
    let area = Rect::new(0, 0, 24, 3);
    let mut buffer = Buffer::empty(area);
    render::render(
        terminal,
        &mut RenderState::new().unwrap(),
        &mut buffer,
        true,
        area,
    )
    .unwrap();
    buffer
}

#[test]
fn inherited_colors_render_exactly_with_bold_underline_inverse_and_erase() {
    let mut terminal = new_terminal_with_colors(24, 3, 1024, &profile()).unwrap();
    terminal.vt_write(b"d\x1b[31mr\x1b[1mb\x1b[0;38;5;200mi\x1b[38;2;7;8;9mt\x1b[4;58;5;15mu\x1b[58;2;111;112;113mv\x1b[0;7mn\x1b[0;1;38;5;1me\x1b[0m\r\n\x1b[48;5;201m\x1b[K\x1b[0m");
    let rendered = buffer(&terminal);
    assert_eq!(rendered[(0, 0)].fg, Color::Rgb(18, 52, 86));
    assert_eq!(rendered[(0, 0)].bg, Color::Reset);
    assert_eq!(rendered[(1, 0)].fg, Color::Rgb(1, 27, 73));
    assert_eq!(rendered[(2, 0)].fg, Color::Rgb(1, 27, 73));
    assert!(rendered[(2, 0)].modifier.contains(Modifier::BOLD));
    assert_eq!(rendered[(3, 0)].fg, Color::Rgb(200, 27, 73));
    assert_eq!(rendered[(4, 0)].fg, Color::Rgb(7, 8, 9));
    assert_eq!(rendered[(5, 0)].underline_color, Color::Rgb(15, 27, 73));
    assert_eq!(rendered[(6, 0)].underline_color, Color::Rgb(111, 112, 113));
    assert!(rendered[(7, 0)].modifier.contains(Modifier::REVERSED));
    assert_eq!(rendered[(8, 0)].fg, Color::Rgb(1, 27, 73));
    assert!(rendered[(8, 0)].modifier.contains(Modifier::BOLD));
    assert_eq!(rendered[(23, 1)].bg, Color::Rgb(201, 27, 73));
    terminal.vt_write(b"\x1b[48;2;3;4;5m\x1b[3;1H\x1b[K\x1b[0m");
    assert_eq!(buffer(&terminal)[(23, 2)].bg, Color::Rgb(3, 4, 5));
    let selection = terminal.select_all().unwrap().unwrap();
    terminal.set_selection(Some(&selection)).unwrap();
    let selected = buffer(&terminal);
    assert!(selected[(0, 0)].modifier.contains(Modifier::REVERSED));
    assert!(!selected[(7, 0)].modifier.contains(Modifier::REVERSED));
}

#[test]
fn builtin_rgb_override_stays_visible_and_reset_restores_inherited_colors_per_pane() {
    let profile = profile();
    let mut first = new_terminal_with_colors(24, 3, 1024, &profile).unwrap();
    let mut second = new_terminal_with_colors(24, 3, 1024, &profile).unwrap();
    let replies = Rc::new(RefCell::new(Vec::new()));
    first
        .on_pty_write({
            let replies = Rc::clone(&replies);
            move |_, bytes| replies.borrow_mut().extend_from_slice(bytes)
        })
        .unwrap();
    first.vt_write(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]4;1;?;255;?\x07");
    let answers = String::from_utf8(replies.borrow().clone()).unwrap();
    assert!(answers.contains("10;rgb:1212/3434/5656"), "{answers:?}");
    assert!(answers.contains("11;rgb:2121/4343/6565"), "{answers:?}");
    assert!(answers.contains("4;1;rgb:0101/1b1b/4949"), "{answers:?}");
    assert!(answers.contains("4;255;rgb:ffff/1b1b/4949"), "{answers:?}");

    let original_red = Palette::default().get(PaletteIndex(1));
    first.vt_write(
        format!(
            "\x1b]4;1;#{:02x}{:02x}{:02x}\x07",
            original_red.r, original_red.g, original_red.b
        )
        .as_bytes(),
    );
    first.vt_write(b"\x1b]10;#aabbcc\x07\x1b]11;#ddeecc\x07\x1b[31mr\x1b[0md");
    second.vt_write(b"\x1b[31mr\x1b[0md");
    let changed = buffer(&first);
    assert_eq!(
        changed[(0, 0)].fg,
        Color::Rgb(original_red.r, original_red.g, original_red.b)
    );
    assert_eq!(changed[(1, 0)].fg, Color::Rgb(170, 187, 204));
    assert_eq!(changed[(1, 0)].bg, Color::Rgb(221, 238, 204));
    let unchanged = buffer(&second);
    assert_eq!(unchanged[(0, 0)].fg, Color::Rgb(1, 27, 73));
    assert_eq!(unchanged[(1, 0)].fg, Color::Rgb(18, 52, 86));
    assert_eq!(unchanged[(1, 0)].bg, Color::Reset);
    first.vt_write(b"\x1b]104;1\x07\x1b]110\x07\x1b]111\x07\x1b[2J\x1b[H\x1b[31mr\x1b[0md");
    let reset = buffer(&first);
    assert_eq!(reset[(0, 0)].fg, Color::Rgb(1, 27, 73));
    assert_eq!(reset[(1, 0)].fg, Color::Rgb(18, 52, 86));
    assert_eq!(reset[(1, 0)].bg, Color::Reset);
    replies.borrow_mut().clear();
    first.vt_write(b"\x1b]4;1;?\x07\x1b]10;?\x07\x1b]11;?\x07");
    let answers = String::from_utf8(replies.borrow().clone()).unwrap();
    assert!(answers.contains("4;1;rgb:0101/1b1b/4949"), "{answers:?}");
    assert!(answers.contains("10;rgb:1212/3434/5656"), "{answers:?}");
    assert!(answers.contains("11;rgb:2121/4343/6565"), "{answers:?}");
}
