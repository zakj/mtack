// Horizontal tab bar.

use crate::process::State;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

pub struct Tab<'a> {
    pub name: &'a str,
    pub state: State,
}

fn indicator(state: State) -> &'static str {
    match state {
        State::Stopped => "○",
        State::Running | State::Stopping | State::Failed => "●",
    }
}

fn indicator_color(state: State) -> Color {
    match state {
        State::Running => Color::Green,
        State::Stopping => Color::Yellow,
        State::Stopped => Color::DarkGray,
        State::Failed => Color::Red,
    }
}

fn tab_spans(tab: &Tab) -> [String; 2] {
    [
        format!(" {} ", indicator(tab.state)),
        format!("{} ", tab.name),
    ]
}

/// Returns the tab index at the given column, or None if no tab is there.
pub fn tab_index_at_col(tabs: &[Tab], col: u16) -> Option<usize> {
    let mut x: u16 = 0;
    for (i, tab) in tabs.iter().enumerate() {
        if i > 0 {
            x += 1; // separator space
        }
        let width: u16 = tab_spans(tab)
            .iter()
            .map(|s| Span::raw(s).width() as u16)
            .sum();
        if col >= x && col < x + width {
            return Some(i);
        }
        x += width;
    }
    None
}

pub struct TabBar<'a> {
    tabs: &'a [Tab<'a>],
    selected: usize,
    focused: bool,
}

impl<'a> TabBar<'a> {
    pub fn new(tabs: &'a [Tab<'a>], selected: usize, focused: bool) -> Self {
        Self {
            tabs,
            selected,
            focused,
        }
    }
}

impl Widget for TabBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut spans = Vec::new();
        for (i, tab) in self.tabs.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" "));
            }

            let [dot_text, name_text] = tab_spans(tab);
            if i == self.selected {
                let bg = if self.focused {
                    Color::Green
                } else {
                    Color::White
                };
                let base = Style::default()
                    .fg(Color::Black)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD);
                let dot_color = indicator_color(tab.state);
                let dot_style = if dot_color == bg {
                    base.fg(Color::Black)
                } else {
                    base.fg(dot_color)
                };
                spans.push(Span::styled(dot_text, dot_style));
                spans.push(Span::styled(name_text, base));
            } else {
                let dot_style = Style::default().fg(indicator_color(tab.state));
                let name_style = Style::default().fg(Color::DarkGray);
                spans.push(Span::styled(dot_text, dot_style));
                spans.push(Span::styled(name_text, name_style));
            }
        }

        Line::from(spans).render(area, buf);
    }
}
