// Horizontal tab bar.

use crate::process::State;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

const CONTAINER_BG: Color = Color::Indexed(236);
const INDICATOR: &str = "● ";

pub struct Tab<'a> {
    pub name: &'a str,
    pub state: State,
}

fn indicator_color(state: State) -> Color {
    match state {
        State::Running => Color::Green,
        State::Stopping => Color::Yellow,
        State::Stopped => Color::DarkGray,
        State::Failed => Color::Red,
    }
}

fn tab_width(tab: &Tab) -> u16 {
    // lead + INDICATOR + name + trail
    2 + INDICATOR.width() as u16 + tab.name.width() as u16
}

const ELLIPSIS: &str = "\u{2026}";
// Container bracket + ellipsis shown when tabs overflow on one side.
const OVERFLOW_WIDTH: u16 = 2;

/// Returns the half-open range `[first, last)` of tabs visible in `area_width`,
/// always including `selected`. Expands outward from the selected tab, trying
/// right then left each iteration, until no more whole tabs fit.
fn visible_range(widths: &[u16], selected: usize, area_width: u16) -> (usize, usize) {
    let total: u16 = widths.iter().sum::<u16>();
    if total <= area_width || widths.is_empty() {
        return (0, widths.len());
    }

    // Width of tabs [first, last).
    let span = |first: usize, last: usize| -> u16 { widths[first..last].iter().sum::<u16>() };

    let mut first = selected;
    let mut last = selected + 1;

    loop {
        let mut expanded = false;

        if last < widths.len() {
            let current = span(first, last);
            let extra = widths[last];
            let left_cost = if first > 0 { OVERFLOW_WIDTH } else { 0 };
            let right_cost = if last + 1 < widths.len() {
                OVERFLOW_WIDTH
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
            let extra = widths[first - 1];
            let left_cost = if first - 1 > 0 { OVERFLOW_WIDTH } else { 0 };
            let right_cost = if last < widths.len() {
                OVERFLOW_WIDTH
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
    let widths: Vec<u16> = tabs.iter().map(|t| tab_width(t)).collect();
    let (first, last) = visible_range(&widths, selected, area_width);
    let mut x: u16 = if first > 0 { OVERFLOW_WIDTH } else { 0 };
    for (i, w) in widths.iter().enumerate().take(last).skip(first) {
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
        let widths: Vec<u16> = self.tabs.iter().map(|t| tab_width(t)).collect();
        let (first, last) = visible_range(&widths, self.selected, area.width);

        let active_bg = if self.focused {
            Color::Green
        } else {
            Color::White
        };
        let container_bg = Style::default().bg(CONTAINER_BG);
        let tab_count = self.tabs.len();
        let mut spans = Vec::new();

        // Left overflow: container bracket + ellipsis (outside tab content).
        if first > 0 {
            spans.push(Span::styled("\u{e0b6}", Style::default().fg(CONTAINER_BG)));
            spans.push(Span::styled(ELLIPSIS, container_bg.fg(Color::Gray)));
        }

        // Bracket when at container edge or active pill, space otherwise.
        let edge_span = |bracket: &'static str, at_edge: bool, is_active: bool| -> Span {
            let (ch, style) = match (at_edge, is_active) {
                (true, true) => (bracket, Style::default().fg(active_bg)),
                (true, false) => (bracket, Style::default().fg(CONTAINER_BG)),
                (false, true) => (bracket, Style::default().fg(active_bg).bg(CONTAINER_BG)),
                (false, false) => (" ", container_bg),
            };
            Span::styled(ch, style)
        };

        for (i, tab) in self.tabs.iter().enumerate().take(last).skip(first) {
            let is_active = i == self.selected;

            spans.push(edge_span("\u{e0b6}", i == 0, is_active));

            if is_active {
                let base = Style::default()
                    .fg(Color::Black)
                    .bg(active_bg)
                    .add_modifier(Modifier::BOLD);
                let dot_color = indicator_color(tab.state);
                let dot_style = if dot_color == active_bg {
                    base.fg(Color::Black)
                } else {
                    base.fg(dot_color)
                };
                spans.push(Span::styled(INDICATOR, dot_style));
                spans.push(Span::styled(tab.name, base));
            } else {
                spans.push(Span::styled(
                    INDICATOR,
                    container_bg.fg(indicator_color(tab.state)),
                ));
                spans.push(Span::styled(tab.name, container_bg.fg(Color::Gray)));
            }

            spans.push(edge_span("\u{e0b4}", i == tab_count - 1, is_active));
        }

        // Right overflow: ellipsis + container bracket (outside tab content).
        if last < tab_count {
            spans.push(Span::styled(ELLIPSIS, container_bg.fg(Color::Gray)));
            spans.push(Span::styled("\u{e0b4}", Style::default().fg(CONTAINER_BG)));
        }

        let used: usize = spans.iter().map(|s| s.width()).sum();
        let remaining = (area.width as usize).saturating_sub(used + 1);
        if remaining > 0 {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                "─".repeat(remaining),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }

        Line::from(spans).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 5 tabs named "a", each width 5: lead + "● " + "a" + trail.
    const WIDTHS: [u16; 5] = [5, 5, 5, 5, 5];

    #[test]
    fn visible_range_all_fit() {
        assert_eq!(visible_range(&WIDTHS, 0, 25), (0, 5));
        assert_eq!(visible_range(&WIDTHS, 2, 40), (0, 5));
    }

    #[test]
    fn visible_range_selected_first_overflow_right() {
        // [tab0][tab1][…◗] = 5+5+2 = 12
        assert_eq!(visible_range(&WIDTHS, 0, 12), (0, 2));
    }

    #[test]
    fn visible_range_selected_last_overflow_left() {
        // [◖…][tab3][tab4] = 2+5+5 = 12
        assert_eq!(visible_range(&WIDTHS, 4, 12), (3, 5));
    }

    #[test]
    fn visible_range_selected_middle_both_ellipses() {
        // [◖…][tab2][tab3][…◗] = 2+5+5+2 = 14
        assert_eq!(visible_range(&WIDTHS, 2, 14), (2, 4));
    }

    #[test]
    fn visible_range_single_tab_wider_than_area() {
        assert_eq!(visible_range(&[20], 0, 10), (0, 1));
    }

    #[test]
    fn visible_range_expands_right_then_left() {
        // With area=12, selected=1: expands right to tab2 (reaching the end
        // recovers the right overflow column, making it fit).
        // [◖…][tab1][tab2] = 2+5+5 = 12
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
        // [tab0 (0..5)][tab1 (5..10)][tab2 (10..15)]
        assert_eq!(tab_index_at_col(&tabs, 0, 15, 0), Some(0));
        assert_eq!(tab_index_at_col(&tabs, 0, 15, 4), Some(0));
        assert_eq!(tab_index_at_col(&tabs, 0, 15, 5), Some(1));
        assert_eq!(tab_index_at_col(&tabs, 0, 15, 9), Some(1));
        assert_eq!(tab_index_at_col(&tabs, 0, 15, 10), Some(2));
        assert_eq!(tab_index_at_col(&tabs, 0, 15, 15), None); // past end
    }

    #[test]
    fn tab_index_at_col_with_left_ellipsis() {
        let tabs = make_tabs(5);
        // selected=4, area=12 → visible (3,5): [◖…(0..2)][tab3 (2..7)][tab4 (7..12)]
        assert_eq!(tab_index_at_col(&tabs, 4, 12, 0), None); // bracket
        assert_eq!(tab_index_at_col(&tabs, 4, 12, 1), None); // ellipsis
        assert_eq!(tab_index_at_col(&tabs, 4, 12, 2), Some(3));
        assert_eq!(tab_index_at_col(&tabs, 4, 12, 6), Some(3));
        assert_eq!(tab_index_at_col(&tabs, 4, 12, 7), Some(4));
        assert_eq!(tab_index_at_col(&tabs, 4, 12, 12), None); // past end
    }
}
