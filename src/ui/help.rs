// Help panel showing all keybindings in columns by category.

use super::hints::{self, Category, KeyHint};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

struct HelpColumns {
    columns: [(&'static str, Vec<&'static KeyHint>); 3],
}

impl HelpColumns {
    fn new() -> Self {
        let normal = hints::normal_hints();
        let focused = hints::focused_hints();

        let nav = normal
            .iter()
            .filter(|h| h.category == Category::Navigation)
            .collect();
        let actions = normal
            .iter()
            .filter(|h| h.category == Category::Actions)
            .chain(focused.iter())
            .collect();
        let app = normal
            .iter()
            .filter(|h| h.category == Category::App)
            .collect();

        Self {
            columns: [("Navigation", nav), ("Actions", actions), ("App", app)],
        }
    }

    fn max_rows(&self) -> usize {
        self.columns
            .iter()
            .map(|(_, hints)| hints.len())
            .max()
            .unwrap_or(0)
    }

    fn column_widths(&self) -> [u16; 3] {
        std::array::from_fn(|i| {
            let (header, hints) = &self.columns[i];
            let header_w = (2 + header.len()) as u16;
            let max_hint = hints.iter().map(|h| h.help_width()).max().unwrap_or(0);
            header_w.max(max_hint)
        })
    }

    fn total_width(&self) -> u16 {
        let w = self.column_widths();
        w[0] + GAP + w[1] + GAP + w[2]
    }
}

const GAP: u16 = 2;

pub struct HelpPanel;

impl Widget for HelpPanel {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }

        // Top border.
        let border_style = Style::default().fg(Color::DarkGray);
        for x in area.x..area.x + area.width {
            buf[(x, area.y)]
                .set_symbol(symbols::line::HORIZONTAL)
                .set_style(border_style);
        }

        // Content area (below border, above blank spacer line).
        let content_top = area.y + 1;
        let content_height = area.height.saturating_sub(2); // border + spacer
        if content_height == 0 {
            return;
        }
        let content_area = Rect::new(area.x, content_top, area.width, content_height);

        let groups = HelpColumns::new();

        if groups.total_width() > area.width {
            let msg = "resize for help";
            let x = area.x + area.width.saturating_sub(msg.len() as u16) / 2;
            Line::from(Span::styled(msg, Style::default().fg(Color::DarkGray)))
                .render(Rect::new(x, content_top, msg.len() as u16, 1), buf);
            return;
        }

        let col_widths = groups.column_widths();
        let [left, _, mid, _, right] = Layout::horizontal([
            Constraint::Length(col_widths[0]),
            Constraint::Length(GAP),
            Constraint::Length(col_widths[1]),
            Constraint::Length(GAP),
            Constraint::Length(col_widths[2]),
        ])
        .flex(Flex::Start)
        .areas(content_area);

        for (col_area, (header, hints)) in [left, mid, right].into_iter().zip(groups.columns) {
            render_column(col_area, buf, header, &hints);
        }
    }
}

fn render_column(area: Rect, buf: &mut Buffer, header: &str, hints: &[&KeyHint]) {
    if area.height == 0 {
        return;
    }

    let header_line = Line::from(Span::styled(
        format!("  {header}"),
        Style::default().fg(Color::Yellow),
    ));
    header_line.render(area, buf);

    for (i, hint) in hints.iter().enumerate() {
        let y = area.y + 1 + i as u16;
        if y >= area.y + area.height {
            break;
        }
        let row = Rect::new(area.x, y, area.width, 1);
        render_hint_row(row, buf, hint);
    }
}

fn render_hint_row(area: Rect, buf: &mut Buffer, hint: &KeyHint) {
    let mut spans = vec![Span::raw("  ")];
    hints::styled_keys(hint.keys, &mut spans);
    spans.push(Span::raw(format!(" {}", hint.desc)));

    Line::from(spans).render(area, buf);
}

/// Height: border + header + max column rows + spacer + status bar.
pub fn help_height(width: u16) -> u16 {
    let groups = HelpColumns::new();
    if groups.total_width() > width {
        4 // border + message + spacer + status bar
    } else {
        (1 + 1 + groups.max_rows() + 1 + 1) as u16
    }
}
