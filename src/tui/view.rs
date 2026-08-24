use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::terminal::OwnedTerminalSnapshot;

const FOOTER_HEIGHT: u16 = 1;

pub fn console_area(area: Rect) -> Rect {
    let content_height = area.height.saturating_sub(FOOTER_HEIGHT);
    Block::bordered().inner(Rect::new(area.x, area.y, area.width, content_height))
}

pub fn render(frame: &mut Frame<'_>, snapshot: &OwnedTerminalSnapshot) {
    let area = frame.area();
    let pane = Rect::new(
        area.x,
        area.y,
        area.width,
        area.height.saturating_sub(FOOTER_HEIGHT),
    );
    frame.render_widget(Block::new().borders(Borders::ALL).title(" Shell "), pane);

    let console = console_area(area);
    let source = snapshot.buffer.area();
    let width = source.width.min(console.width);
    let height = source.height.min(console.height);
    for y in 0..height {
        for x in 0..width {
            frame.buffer_mut()[(console.x + x, console.y + y)] =
                snapshot.buffer[(source.x + x, source.y + y)].clone();
        }
    }

    frame.render_widget(
        Paragraph::new("Ctrl-Q: quit · keys go to the shell")
            .style(Style::default().fg(Color::DarkGray)),
        Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
    );

    if let Some(cursor) = snapshot.cursor {
        let x = console.x.saturating_add(cursor.position.x);
        let y = console.y.saturating_add(cursor.position.y);
        if x < console.right() && y < console.bottom() {
            frame.set_cursor_position((x, y));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiny_screen_has_safe_console_area() {
        let area = console_area(Rect::new(0, 0, 0, 0));

        assert_eq!(area.width, 0);
        assert_eq!(area.height, 0);
    }
}
