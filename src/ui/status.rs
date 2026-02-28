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
                if searching || self.scrolled_back {
                    push_hint(&mut spans, "esc", "clear");
                }
                let running = self.process_state == State::Running;
                for hint in hints::normal_hints() {
                    let visible = match hint.bar_when {
                        ShowWhen::Always => true,
                        ShowWhen::WhenRunning => running,
                        ShowWhen::WhenStopped => !running && self.process_state != State::Stopping,
                        ShowWhen::WhenScrolled => self.scrolled_back,
                    };
                    if visible && let Some(bar_keys) = hint.bar {
                        push_hint(&mut spans, bar_keys, hint.desc);
                    }
                }
            }
            Mode::Focused => {
                push_badge(&mut spans, "FOCUS", Color::Green);
                for hint in hints::focused_hints() {
                    if let Some(bar_keys) = hint.bar {
                        push_hint(&mut spans, bar_keys, hint.desc);
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

fn push_hint(spans: &mut Vec<Span<'static>>, key: &str, desc: &str) {
    if !spans.is_empty() {
        spans.push(Span::raw("  "));
    }
    hints::styled_keys(key, spans);
    spans.push(Span::raw(format!(" {desc}")));
}
