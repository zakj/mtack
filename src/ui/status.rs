// Status bar with keybinding hints.

use super::RenderContext;
use super::hints;
use crate::input::Mode;
use crate::process::State;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

pub struct StatusBar<'a> {
    ctx: &'a RenderContext<'a>,
    scrolled_back: bool,
    alternate_screen: bool,
    process_state: State,
}

impl<'a> StatusBar<'a> {
    pub fn new(ctx: &'a RenderContext<'a>) -> Self {
        let proc = ctx.processes.get(ctx.selected);
        let scrolled_back = proc.is_some_and(|p| p.terminal().is_scrolled_back());
        let alternate_screen = proc.is_some_and(|p| p.terminal().is_alternate_screen());
        let process_state = proc.map_or(State::Stopped, |p| p.state());
        Self {
            ctx,
            scrolled_back,
            alternate_screen,
            process_state,
        }
    }
}

impl Widget for StatusBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let max_width = area.width as usize;
        let mut spans = Vec::new();
        let mut right_spans: Vec<Span<'static>> = Vec::new();

        match self.ctx.mode {
            Mode::ConfirmQuit => {
                spans.push(Span::styled(
                    "Quit with running processes? ",
                    Style::default().fg(Color::Yellow),
                ));
                push_hint(&mut spans, "y/enter", "quit");
                push_hint(&mut spans, "any", "cancel");
            }
            Mode::Search => {
                let search = self.ctx.search;
                spans.push(Span::styled("/", Style::default().fg(Color::Yellow)));
                if search.query.is_empty() && !search.last_query.is_empty() {
                    let placeholder = &search.last_query;
                    let first = &placeholder[..placeholder.ceil_char_boundary(1)];
                    let rest = &placeholder[first.len()..];
                    spans.push(Span::styled(
                        first.to_string(),
                        Style::default().fg(Color::DarkGray).bg(Color::Yellow),
                    ));
                    if !rest.is_empty() {
                        spans.push(Span::styled(
                            rest.to_string(),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                } else {
                    spans.push(Span::styled(
                        search.query.to_string(),
                        Style::default().fg(Color::Yellow),
                    ));
                    spans.push(Span::styled("▎", Style::default().fg(Color::Yellow)));
                }
            }
            Mode::Normal => {
                let search = self.ctx.search;
                let searching = !search.query.is_empty();
                // Badges/search state are always shown (small, high-priority).
                if let Some(idx) = search.current() {
                    spans.push(Span::styled(
                        format!(" {}/{} ", idx + 1, search.match_count()),
                        Style::default().fg(Color::Black).bg(Color::Yellow),
                    ));
                    spans.push(Span::raw(" "));
                } else if search.no_matches() {
                    spans.push(Span::styled(
                        " 0/0 ",
                        Style::default().fg(Color::Black).bg(Color::Red),
                    ));
                    spans.push(Span::raw(" "));
                } else if searching && self.alternate_screen {
                    push_badge(&mut spans, "FIND", Color::Yellow);
                }
                if self.scrolled_back {
                    push_badge(&mut spans, "SCROLL", Color::Yellow);
                }
                let mut width: usize = spans.iter().map(|s| s.width()).sum();
                let full = (searching || self.scrolled_back)
                    && !try_push_hint(&mut spans, &mut width, max_width, "esc", "clear");
                if !full {
                    for hint in hints::normal_hints() {
                        if hint.right {
                            continue;
                        }
                        if let Some((bar_keys, show_when)) = hint.bar
                            && show_when.visible(self.process_state, self.scrolled_back)
                            && !try_push_hint(
                                &mut spans, &mut width, max_width, bar_keys, hint.desc,
                            )
                        {
                            break;
                        }
                    }
                }
                for hint in hints::normal_hints() {
                    if hint.right
                        && let Some((bar_keys, show_when)) = hint.bar
                        && show_when.visible(self.process_state, self.scrolled_back)
                    {
                        right_spans.extend(build_hint(true, bar_keys, hint.desc));
                    }
                }
            }
            Mode::Focused => {
                push_badge(&mut spans, "FOCUS", Color::Green);
                let mut width: usize = spans.iter().map(|s| s.width()).sum();
                for hint in hints::focused_hints() {
                    if let Some((bar_keys, _)) = hint.bar
                        && !try_push_hint(&mut spans, &mut width, max_width, bar_keys, hint.desc)
                    {
                        break;
                    }
                }
            }
            Mode::Quitting => {
                spans.push(Span::styled(
                    format!(
                        "Shutting down\u{2026} ({} remaining)",
                        self.ctx.remaining_count
                    ),
                    Style::default().fg(Color::Yellow),
                ));
            }
        }

        let used: usize = spans.iter().map(|s| s.width()).sum();
        let right_width: usize = right_spans.iter().map(|s| s.width()).sum();
        let gap = if used > 0 { 2 } else { 0 };
        let available = max_width.saturating_sub(used + gap);
        if available > 0 {
            if gap > 0 {
                spans.push(Span::raw("  "));
            }
            if right_width > 0 && available > right_width {
                let fill = available - right_width;
                spans.push(Span::styled(
                    "─".repeat(fill),
                    Style::default().add_modifier(Modifier::DIM),
                ));
                spans.extend(right_spans);
            } else {
                spans.push(Span::styled(
                    "─".repeat(available),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
        }

        Line::from(spans).render(area, buf);
    }
}

fn push_badge(spans: &mut Vec<Span<'static>>, label: &str, bg: Color) {
    if !spans.is_empty() {
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(
        format!(" {label} "),
        Style::default().fg(Color::Black).bg(bg),
    ));
}

fn build_hint(has_prefix: bool, key: &str, desc: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if has_prefix {
        spans.push(Span::raw("  "));
    }
    hints::styled_keys(key, &mut spans);
    spans.push(Span::raw(format!(" {desc}")));
    spans
}

fn push_hint(spans: &mut Vec<Span<'static>>, key: &str, desc: &str) {
    spans.extend(build_hint(!spans.is_empty(), key, desc));
}

/// Push a hint only if it fits within `max_width`. Updates `width` and returns
/// `true` on success, `false` if it didn't fit (callers should stop adding hints).
fn try_push_hint(
    spans: &mut Vec<Span<'static>>,
    width: &mut usize,
    max_width: usize,
    key: &str,
    desc: &str,
) -> bool {
    let prefix = if *width > 0 { 2 } else { 0 };
    let w = prefix + key.len() + 1 + desc.len();
    if *width + w > max_width {
        return false;
    }
    spans.extend(build_hint(*width > 0, key, desc));
    *width += w;
    true
}
