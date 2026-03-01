// Process output viewport (renders vt100 screen).

use crate::terminal::VisibleMatchSpan;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;
use tui_term::widget::{Cursor, PseudoTerminal};

pub struct Viewport<'a> {
    screen: &'a vt100::Screen,
    focused: bool,
    matches: &'a [VisibleMatchSpan],
}

impl<'a> Viewport<'a> {
    pub fn new(screen: &'a vt100::Screen, focused: bool, matches: &'a [VisibleMatchSpan]) -> Self {
        Self {
            screen,
            focused,
            matches,
        }
    }
}

impl Widget for Viewport<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        PseudoTerminal::new(self.screen)
            .cursor(Cursor::default().visibility(self.focused))
            .render(area, buf);

        let match_style = Style::default().fg(Color::Black).bg(Color::Yellow);

        for m in self.matches {
            for c in m.col..m.col.saturating_add(m.len) {
                let x = area.x + c;
                let y = area.y + m.row;
                if x < area.x + area.width && y < area.y + area.height {
                    buf[(x, y)].set_style(match_style);
                }
            }
        }
    }
}
