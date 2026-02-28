mod help;
mod hints;
mod status;
pub mod tabs;
mod viewport;

// Top-level layout composition.

use crate::input::Mode;
use crate::process::Process;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use tabs::{Tab, TabBar};

pub use help::help_height;

pub struct RenderContext<'a> {
    pub processes: &'a [Process],
    pub selected: usize,
    pub mode: Mode,
    pub show_help: bool,
    pub search_query: &'a str,
    pub last_search_query: &'a str,
    pub search_total: usize,
    pub search_current: Option<usize>,
    pub search_no_matches: bool,
    pub remaining_count: usize,
    pub visible_matches: &'a [(u16, u16, u16)],
}

pub fn render(frame: &mut Frame, ctx: &RenderContext) {
    let bottom_height = if ctx.show_help {
        help::help_height(frame.area().width)
    } else {
        1
    };

    let [tab_area, viewport_area, bottom_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(bottom_height),
    ])
    .areas(frame.area());

    // Tab bar.
    let tabs: Vec<Tab> = ctx
        .processes
        .iter()
        .map(|p| Tab {
            name: p.name(),
            state: p.state(),
        })
        .collect();
    frame.render_widget(
        TabBar::new(&tabs, ctx.selected, ctx.mode == Mode::Focused),
        tab_area,
    );

    // Viewport for selected process.
    if let Some(proc) = ctx.processes.get(ctx.selected) {
        let screen = proc.terminal().screen();
        let focused = ctx.mode == Mode::Focused;

        frame.render_widget(
            viewport::Viewport::new(screen, focused, ctx.visible_matches),
            viewport_area,
        );
    }

    // Bottom area: help panel or status bar.
    let status = status::StatusBar::new(ctx);
    if ctx.show_help {
        let [help_area, status_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(bottom_area);
        frame.render_widget(help::HelpPanel, help_area);
        frame.render_widget(status, status_area);
    } else {
        frame.render_widget(status, bottom_area);
    }
}
