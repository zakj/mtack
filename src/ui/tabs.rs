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

fn tab_span_width(spans: &[String; 2]) -> u16 {
    spans.iter().map(|s| Span::raw(s).width() as u16).sum()
}

const ELLIPSIS: &str = "\u{2026}";
const ELLIPSIS_WIDTH: u16 = 1;

/// Returns the half-open range `[first, last)` of tabs visible in `area_width`,
/// always including `selected`. Expands outward from the selected tab, trying
/// right then left each iteration, until no more whole tabs fit.
fn visible_range(widths: &[u16], selected: usize, area_width: u16) -> (usize, usize) {
    let total: u16 = widths.iter().sum::<u16>() + widths.len().saturating_sub(1) as u16;
    if total <= area_width || widths.is_empty() {
        return (0, widths.len());
    }

    // Width of tabs [first, last) including internal separators.
    let span = |first: usize, last: usize| -> u16 {
        widths[first..last].iter().sum::<u16>() + last.saturating_sub(first + 1) as u16
    };

    let mut first = selected;
    let mut last = selected + 1;

    loop {
        let mut expanded = false;

        if last < widths.len() {
            let current = span(first, last);
            let extra = 1 + widths[last];
            let left_cost = if first > 0 { ELLIPSIS_WIDTH } else { 0 };
            let right_cost = if last + 1 < widths.len() {
                ELLIPSIS_WIDTH
            } else {
                0
            };
            if current + extra + left_cost + right_cost <= area_width {
                last += 1;
                expanded = true;
            }
        }

        if first > 0 {
            let current = span(first, last);
            let extra = widths[first - 1] + 1;
            let left_cost = if first - 1 > 0 { ELLIPSIS_WIDTH } else { 0 };
            let right_cost = if last < widths.len() {
                ELLIPSIS_WIDTH
            } else {
                0
            };
            if current + extra + left_cost + right_cost <= area_width {
                first -= 1;
                expanded = true;
            }
        }

        if !expanded {
            break;
        }
    }

    (first, last)
}

/// Returns the tab index at the given column, or None if no tab is there.
pub fn tab_index_at_col(tabs: &[Tab], selected: usize, area_width: u16, col: u16) -> Option<usize> {
    let widths: Vec<u16> = tabs.iter().map(|t| tab_span_width(&tab_spans(t))).collect();
    let (first, last) = visible_range(&widths, selected, area_width);
    let mut x: u16 = if first > 0 { ELLIPSIS_WIDTH } else { 0 };
    for (i, w) in widths.iter().enumerate().take(last).skip(first) {
        if i > first {
            x += 1;
        }
        if col >= x && col < x + w {
            return Some(i);
        }
        x += w;
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
        let tab_data: Vec<_> = self
            .tabs
            .iter()
            .map(|t| {
                let s = tab_spans(t);
                let w = tab_span_width(&s);
                (s, w)
            })
            .collect();
        let widths: Vec<u16> = tab_data.iter().map(|(_, w)| *w).collect();
        let (first, last) = visible_range(&widths, self.selected, area.width);

        let mut spans = Vec::new();

        if first > 0 {
            spans.push(Span::styled(ELLIPSIS, Style::default().fg(Color::DarkGray)));
        }

        for (i, (tab, ([dot_text, name_text], _))) in self
            .tabs
            .iter()
            .zip(tab_data.iter())
            .enumerate()
            .take(last)
            .skip(first)
        {
            if i > first {
                spans.push(Span::raw(" "));
            }

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

        if last < self.tabs.len() {
            spans.push(Span::styled(ELLIPSIS, Style::default().fg(Color::DarkGray)));
        }

        Line::from(spans).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 5 tabs, each width 5 (e.g. " ● a "), with 1-col separators.
    // Total = 5*5 + 4 = 29.
    const WIDTHS: [u16; 5] = [5, 5, 5, 5, 5];

    #[test]
    fn visible_range_all_fit() {
        assert_eq!(visible_range(&WIDTHS, 0, 29), (0, 5));
        assert_eq!(visible_range(&WIDTHS, 2, 40), (0, 5));
    }

    #[test]
    fn visible_range_selected_first_overflow_right() {
        // [tab0][sep][tab1][…] = 5+1+5+1 = 12
        assert_eq!(visible_range(&WIDTHS, 0, 13), (0, 2));
    }

    #[test]
    fn visible_range_selected_last_overflow_left() {
        // […][tab3][sep][tab4] = 1+5+1+5 = 12
        assert_eq!(visible_range(&WIDTHS, 4, 13), (3, 5));
    }

    #[test]
    fn visible_range_selected_middle_both_ellipses() {
        // […][tab2][sep][tab3][…] = 1+5+1+5+1 = 13
        assert_eq!(visible_range(&WIDTHS, 2, 13), (2, 4));
    }

    #[test]
    fn visible_range_single_tab_wider_than_area() {
        assert_eq!(visible_range(&[20], 0, 10), (0, 1));
    }

    #[test]
    fn visible_range_expands_right_then_left() {
        // With area=12, selected=1: expands right to tab2 (reaching the end
        // recovers the right ellipsis column, making it fit).
        // […][tab1][sep][tab2] = 1+5+1+5 = 12
        let widths = [5, 5, 5];
        assert_eq!(visible_range(&widths, 1, 12), (1, 3));
    }

    fn make_tabs(n: usize) -> Vec<Tab<'static>> {
        (0..n)
            .map(|_| Tab {
                name: "a",
                state: State::Running,
            })
            .collect()
    }

    #[test]
    fn tab_index_at_col_no_scroll() {
        let tabs = make_tabs(3);
        // [tab0 (0..5)][sep (5)][tab1 (6..11)][sep (11)][tab2 (12..17)]
        assert_eq!(tab_index_at_col(&tabs, 0, 17, 0), Some(0));
        assert_eq!(tab_index_at_col(&tabs, 0, 17, 4), Some(0));
        assert_eq!(tab_index_at_col(&tabs, 0, 17, 5), None); // separator
        assert_eq!(tab_index_at_col(&tabs, 0, 17, 6), Some(1));
        assert_eq!(tab_index_at_col(&tabs, 0, 17, 12), Some(2));
        assert_eq!(tab_index_at_col(&tabs, 0, 17, 17), None); // past end
    }

    #[test]
    fn tab_index_at_col_with_left_ellipsis() {
        let tabs = make_tabs(5);
        // selected=4, area=13 → visible (3,5): [… (0)][tab3 (1..6)][sep (6)][tab4 (7..12)]
        assert_eq!(tab_index_at_col(&tabs, 4, 13, 0), None); // ellipsis
        assert_eq!(tab_index_at_col(&tabs, 4, 13, 1), Some(3));
        assert_eq!(tab_index_at_col(&tabs, 4, 13, 7), Some(4));
        assert_eq!(tab_index_at_col(&tabs, 4, 13, 12), None); // past end
    }
}
