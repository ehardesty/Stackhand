use libghostty_vt::render::{CellIterator, CursorVisualStyle, RenderState, RowIterator};
use libghostty_vt::style::{self, Underline};
use libghostty_vt::terminal::Terminal;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};

use super::session::{CursorShape, OwnedCursorState};

pub fn render<'alloc, 'cb>(
    terminal: &Terminal<'alloc, 'cb>,
    state: &mut RenderState<'alloc>,
    buffer: &mut Buffer,
    focused: bool,
    area: Rect,
) -> Result<Option<OwnedCursorState>, libghostty_vt::Error> {
    let snapshot = state.update(terminal)?;
    let colors = snapshot.colors()?;
    let cursor = cursor_state(&snapshot, focused, area);
    let default_fg = rgb(colors.foreground);
    let default_bg = rgb(colors.background);
    let mut rows = RowIterator::new()?;
    let mut cells = CellIterator::new()?;
    let mut row_iteration = rows.update(&snapshot)?;
    let mut y = 0_u16;
    while let Some(row) = row_iteration.next() {
        if y >= area.height {
            break;
        }
        let mut cell_iteration = cells.update(row)?;
        let mut x = 0_u16;
        while let Some(cell) = cell_iteration.next() {
            if x >= area.width {
                break;
            }
            let mut symbol = String::new();
            cell.graphemes_utf8(&mut symbol)?;
            if symbol.is_empty() {
                symbol.push(' ');
            }
            let mut cell_style = cell
                .style()
                .map(|value| to_ratatui_style(&value, &colors.palette))?;
            cell_style = cell_style
                .fg(cell.fg_color()?.map(rgb).unwrap_or(default_fg))
                .bg(cell.bg_color()?.map(rgb).unwrap_or(default_bg));
            if cell.is_selected()? {
                cell_style = if cell_style.add_modifier.contains(Modifier::REVERSED) {
                    cell_style.remove_modifier(Modifier::REVERSED)
                } else {
                    cell_style.add_modifier(Modifier::REVERSED)
                };
            }
            buffer[(area.x + x, area.y + y)]
                .set_symbol(&symbol)
                .set_style(cell_style);
            x += 1;
        }
        y += 1;
    }
    Ok(cursor)
}

fn cursor_state(
    snapshot: &libghostty_vt::render::Snapshot<'_, '_>,
    focused: bool,
    area: Rect,
) -> Option<OwnedCursorState> {
    let position = snapshot.cursor_viewport().ok().flatten()?;
    if !focused
        || !snapshot.cursor_visible().unwrap_or(false)
        || position.at_wide_tail
        || position.x >= area.width
        || position.y >= area.height
    {
        return None;
    }
    let shape = match snapshot.cursor_visual_style().ok()? {
        CursorVisualStyle::Bar => CursorShape::Bar,
        CursorVisualStyle::Underline => CursorShape::Underline,
        _ => CursorShape::Block,
    };
    Some(OwnedCursorState {
        position: Position::new(position.x, position.y),
        shape,
        blinking: snapshot.cursor_blinking().unwrap_or(false),
    })
}

fn rgb(color: style::RgbColor) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

fn resolve(color: &style::StyleColor, palette: &[style::RgbColor; 256]) -> Option<Color> {
    match color {
        style::StyleColor::None => None,
        style::StyleColor::Rgb(value) => Some(rgb(*value)),
        style::StyleColor::Palette(index) => Some(rgb(palette[index.0 as usize])),
    }
}

fn to_ratatui_style(value: &style::Style, palette: &[style::RgbColor; 256]) -> Style {
    let mut result = Style::default();
    if let Some(color) = resolve(&value.fg_color, palette) {
        result = result.fg(color);
    }
    if let Some(color) = resolve(&value.bg_color, palette) {
        result = result.bg(color);
    }
    let mut modifiers = Modifier::empty();
    if value.bold {
        modifiers |= Modifier::BOLD;
    }
    if value.italic {
        modifiers |= Modifier::ITALIC;
    }
    if value.faint {
        modifiers |= Modifier::DIM;
    }
    if value.blink {
        modifiers |= Modifier::SLOW_BLINK;
    }
    if value.inverse {
        modifiers |= Modifier::REVERSED;
    }
    if value.invisible {
        modifiers |= Modifier::HIDDEN;
    }
    if value.strikethrough {
        modifiers |= Modifier::CROSSED_OUT;
    }
    if value.underline != Underline::None {
        modifiers |= Modifier::UNDERLINED;
    }
    result.add_modifier(modifiers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use libghostty_vt::terminal::Options;

    #[test]
    fn active_selection_is_visible_in_the_owned_buffer() {
        let mut terminal = Terminal::new(Options {
            cols: 8,
            rows: 2,
            max_scrollback: 1024,
        })
        .unwrap();
        terminal.vt_write(b"selected");
        let selection = terminal.select_all().unwrap().unwrap();
        terminal.set_selection(Some(&selection)).unwrap();
        let area = Rect::new(0, 0, 8, 2);
        let mut buffer = Buffer::empty(area);
        let mut state = RenderState::new().unwrap();

        render(&terminal, &mut state, &mut buffer, true, area).unwrap();

        assert!(buffer[(0, 0)].modifier.contains(Modifier::REVERSED));
    }
}
