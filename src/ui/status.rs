// Status bar with keybinding hints.

use super::RenderContext;
use super::hints::{self, ShowWhen};
use crate::input::Mode;
use crate::process::State;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
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
                spans.push(Span::styled("/", Style::default().fg(Color::Yellow)));
                if self.ctx.search_query.is_empty() && !self.ctx.last_search_query.is_empty() {
                    let placeholder = self.ctx.last_search_query.to_string();
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
                        self.ctx.search_query.to_string(),
                        Style::default().fg(Color::Yellow),
                    ));
                    spans.push(Span::styled("▎", Style::default().fg(Color::Yellow)));
                }
            }
            Mode::Normal => {
                let searching = !self.ctx.search_query.is_empty();
                // Badges/search state are always shown (small, high-priority).
                if let Some(idx) = self.ctx.search_current {
                    spans.push(Span::styled(
                        format!(" {}/{} ", idx + 1, self.ctx.search_total),
                        Style::default().fg(Color::Black).bg(Color::Yellow),
                    ));
                    spans.push(Span::raw(" "));
                } else if self.ctx.search_no_matches {
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
                let running = self.process_state == State::Running;
                if !full {
                    for hint in hints::normal_hints() {
                        let visible = match hint.bar_when {
                            ShowWhen::Always => true,
                            ShowWhen::WhenRunning => running,
                            ShowWhen::WhenStopped => {
                                !running && self.process_state != State::Stopping
                            }
                            ShowWhen::WhenScrolled => self.scrolled_back,
                        };
                        if visible
                            && let Some(bar_keys) = hint.bar
                            && !try_push_hint(
                                &mut spans, &mut width, max_width, bar_keys, hint.desc,
                            )
                        {
                            break;
                        }
                    }
                }
            }
            Mode::Focused => {
                push_badge(&mut spans, "FOCUS", Color::Green);
                let mut width: usize = spans.iter().map(|s| s.width()).sum();
                for hint in hints::focused_hints() {
                    if let Some(bar_keys) = hint.bar
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
