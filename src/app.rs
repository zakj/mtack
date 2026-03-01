// Top-level App state, event loop, frame rendering.

use crate::config::Config;
use crate::event::Event;
use crate::input::{self, Action, Mode, ScrollAmount};
use crate::process::Process;
use crate::process::State;
use crossterm::event::{Event as CtEvent, EventStream, MouseButton, MouseEventKind};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use std::time::Duration;
use tokio::sync::mpsc;

const EVENT_CHANNEL_SIZE: usize = 256;
const MOUSE_SCROLL_LINES: usize = 3;
const RENDER_INTERVAL_FOCUSED: Duration = Duration::from_millis(16);
const RENDER_INTERVAL_UNFOCUSED: Duration = Duration::from_millis(100);

pub struct App {
    processes: Vec<Process>,
    selected: usize,
    mode: Mode,
    show_help: bool,
    terminal_cols: u16,
    terminal_rows: u16,
    event_rx: mpsc::Receiver<Event>,
    should_quit: bool,
    search_query: String,
    last_search_query: String,
    search_matches: Vec<(usize, usize, usize)>,
    search_current: Option<usize>,
    search_no_matches: bool,
    focused: bool,
}

impl App {
    pub fn new(config: &Config) -> Self {
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_SIZE);

        // Use a placeholder size; will be resized on first frame.
        let processes = config
            .procs
            .iter()
            .enumerate()
            .map(|(id, proc_config)| {
                Process::new(
                    id,
                    proc_config.clone(),
                    24,
                    80,
                    proc_config.scrollback(config.scrollback),
                    Duration::from_secs(config.shutdown_timeout),
                    event_tx.clone(),
                )
            })
            .collect();

        Self {
            processes,
            selected: 0,
            mode: Mode::Normal,
            show_help: false,
            terminal_cols: 80,
            terminal_rows: 24,
            event_rx,
            should_quit: false,
            search_query: String::new(),
            last_search_query: String::new(),
            search_matches: Vec::new(),
            search_current: None,
            search_no_matches: false,
            focused: true,
        }
    }

    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> miette::Result<()> {
        // Set initial size from actual terminal dimensions.
        let size = terminal.size().map_err(|e| miette::miette!("{e}"))?;
        self.terminal_cols = size.width;
        self.terminal_rows = size.height;
        self.resize_processes();

        // Auto-start processes.
        for i in 0..self.processes.len() {
            if self.processes[i].autostart() {
                self.processes[i].start()?;
            }
        }

        let mut crossterm_events = EventStream::new();
        let mut render_interval = tokio::time::interval(RENDER_INTERVAL_FOCUSED);
        let mut dirty = true;

        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|e| miette::miette!("{e}"))?;
        let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .map_err(|e| miette::miette!("{e}"))?;

        loop {
            tokio::select! {
                _ = render_interval.tick(), if dirty => {
                    let visible_matches: Vec<(u16, u16, u16)> =
                        if !self.search_query.is_empty() && self.mode != Mode::Search {
                            self.processes[self.selected]
                                .terminal_mut()
                                .find_visible_matches(&self.search_query)
                                .iter()
                                .map(|&(row, col, len)| (row, col as u16, len as u16))
                                .collect()
                        } else {
                            Vec::new()
                        };

                    terminal
                        .draw(|frame| {
                            let ctx = crate::ui::RenderContext {
                                processes: &self.processes,
                                selected: self.selected,
                                mode: self.mode,
                                show_help: self.show_help,
                                search_query: &self.search_query,
                                last_search_query: &self.last_search_query,
                                search_total: self.search_matches.len(),
                                search_current: self.search_current,
                                search_no_matches: self.search_no_matches,
                                remaining_count: self
                                    .processes
                                    .iter()
                                    .filter(|p| {
                                        matches!(p.state(), State::Running | State::Stopping)
                                    })
                                    .count(),
                                visible_matches: &visible_matches,
                            };
                            crate::ui::render(frame, &ctx);
                        })
                        .map_err(|e| miette::miette!("{e}"))?;
                    dirty = false;
                    if self.should_quit {
                        break;
                    }
                    if self.mode == Mode::Quitting && self.all_stopped() {
                        break;
                    }
                }
                Some(ct_event) = crossterm_events.next() => {
                    if let Ok(event) = ct_event {
                        let was_focused = self.focused;
                        self.handle_crossterm_event(event).await?;
                        if self.focused != was_focused {
                            let period = if self.focused {
                                RENDER_INTERVAL_FOCUSED
                            } else {
                                RENDER_INTERVAL_UNFOCUSED
                            };
                            render_interval = tokio::time::interval(period);
                        }
                        self.processes[self.selected].sync_paused();
                        dirty = true;
                    }
                }
                Some(event) = self.event_rx.recv() => {
                    let needs_render = match &event {
                        Event::PtyOutput { id, .. } => *id == self.selected,
                        Event::ProcessExited { .. } => true,
                    };
                    self.handle_app_event(event).await?;
                    dirty |= needs_render;
                }
                _ = sigterm.recv() => {
                    self.begin_quit();
                    dirty = true;
                }
                _ = sighup.recv() => {
                    self.begin_quit();
                    dirty = true;
                }
            }
        }

        Ok(())
    }

    async fn handle_crossterm_event(&mut self, event: CtEvent) -> miette::Result<()> {
        match event {
            CtEvent::Key(key) => {
                let unfocus_key = self.processes[self.selected].unfocus_key();
                if let Some(action) = input::resolve(key, self.mode, unfocus_key) {
                    self.handle_action(action).await?;
                }
            }
            CtEvent::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.processes[self.selected]
                        .terminal_mut()
                        .scroll_up(MOUSE_SCROLL_LINES);
                }
                MouseEventKind::ScrollDown => {
                    self.processes[self.selected]
                        .terminal_mut()
                        .scroll_down(MOUSE_SCROLL_LINES);
                }
                MouseEventKind::Down(MouseButton::Left) if mouse.row == 0 => {
                    let tabs: Vec<_> = self
                        .processes
                        .iter()
                        .map(|p| crate::ui::tabs::Tab {
                            name: p.name(),
                            state: p.state(),
                        })
                        .collect();
                    if let Some(idx) = crate::ui::tabs::tab_index_at_col(
                        &tabs,
                        self.selected,
                        self.terminal_cols,
                        mouse.column,
                    ) {
                        self.selected = idx;
                        self.clear_search();
                        if self.mode == Mode::Focused {
                            self.mode = Mode::Normal;
                        }
                    }
                }
                _ => {}
            },
            CtEvent::Resize(cols, rows) => {
                self.terminal_cols = cols;
                self.terminal_rows = rows;
                self.resize_processes();
            }
            CtEvent::FocusGained => self.focused = true,
            CtEvent::FocusLost => self.focused = false,
            _ => {}
        }
        Ok(())
    }

    async fn handle_action(&mut self, action: Action) -> miette::Result<()> {
        match action {
            Action::SelectTab(idx) => {
                if idx < self.processes.len() {
                    self.selected = idx;
                    self.clear_search();
                }
            }
            Action::NextTab => {
                if !self.processes.is_empty() {
                    self.selected = (self.selected + 1) % self.processes.len();
                    self.clear_search();
                }
            }
            Action::PrevTab => {
                if !self.processes.is_empty() {
                    self.selected =
                        (self.selected + self.processes.len() - 1) % self.processes.len();
                    self.clear_search();
                }
            }
            Action::Focus => {
                if self.processes[self.selected].state() == State::Running {
                    self.mode = Mode::Focused;
                }
            }
            Action::Unfocus => {
                self.mode = Mode::Normal;
            }
            Action::StartProcess => {
                self.processes[self.selected].start()?;
            }
            Action::StopProcess => {
                self.processes[self.selected].stop();
            }
            Action::RestartProcess => {
                if self.processes[self.selected].restart() {
                    self.processes[self.selected].start()?;
                }
            }
            Action::ScrollUp(amount) => {
                let lines = self.scroll_lines(amount);
                self.processes[self.selected]
                    .terminal_mut()
                    .scroll_up(lines);
            }
            Action::ScrollDown(amount) => {
                let lines = self.scroll_lines(amount);
                self.processes[self.selected]
                    .terminal_mut()
                    .scroll_down(lines);
            }
            Action::EnterSearch => {
                self.mode = Mode::Search;
                self.search_query.clear();
                self.search_matches.clear();
                self.search_current = None;
            }
            Action::SearchInput(c) => {
                self.search_query.push(c);
            }
            Action::SearchBackspace => {
                self.search_query.pop();
            }
            Action::SearchFillPlaceholder => {
                if self.search_query.is_empty() && !self.last_search_query.is_empty() {
                    self.search_query.clone_from(&self.last_search_query);
                }
            }
            Action::SearchAccept => {
                // Enter with empty query and a placeholder: use the placeholder.
                if self.search_query.is_empty() && !self.last_search_query.is_empty() {
                    self.search_query.clone_from(&self.last_search_query);
                }
                if !self.processes[self.selected]
                    .terminal()
                    .is_alternate_screen()
                {
                    let (matches, total_rows) = self.processes[self.selected]
                        .terminal_mut()
                        .find_all_matches(&self.search_query);
                    self.search_matches = matches.to_vec();
                    self.search_no_matches = self.search_matches.is_empty();
                    if !self.search_matches.is_empty() {
                        // Jump to last match at or before current viewport.
                        let scrollback = self.processes[self.selected].terminal().scrollback();
                        let (rows, _) = self.processes[self.selected].terminal().size();
                        let visible_top = total_rows.saturating_sub(rows as usize + scrollback);
                        let visible_bottom = visible_top + rows as usize;

                        let idx = self
                            .search_matches
                            .iter()
                            .rposition(|&(row, _, _)| row <= visible_bottom)
                            .unwrap_or(self.search_matches.len() - 1);
                        self.search_current = Some(idx);
                        self.processes[self.selected]
                            .terminal_mut()
                            .scroll_to_row(self.search_matches[idx].0);
                    }
                }
                if !self.search_query.is_empty() {
                    self.last_search_query.clone_from(&self.search_query);
                }
                self.mode = Mode::Normal;
            }
            Action::SearchCancel => {
                self.clear_search();
                self.processes[self.selected]
                    .terminal_mut()
                    .scroll_to_bottom();
                self.mode = Mode::Normal;
            }
            Action::SearchNext => {
                self.search_navigate(true);
            }
            Action::SearchPrev => {
                self.search_navigate(false);
            }
            Action::ToggleHelp => {
                self.show_help = !self.show_help;
                self.resize_processes();
            }
            Action::ScrollToTop => {
                self.processes[self.selected].terminal_mut().scroll_to_top();
            }
            Action::ScrollToBottom => {
                self.processes[self.selected]
                    .terminal_mut()
                    .scroll_to_bottom();
            }
            Action::Quit => {
                if self.has_running_processes() && self.mode != Mode::ConfirmQuit {
                    self.mode = Mode::ConfirmQuit;
                } else {
                    self.begin_quit();
                }
            }
            Action::ForceQuit => {
                self.begin_quit();
            }
            Action::CancelQuit => {
                self.mode = Mode::Normal;
            }
            Action::ForwardKey(key) => {
                let bytes = key_event_to_bytes(key);
                self.processes[self.selected].write(&bytes).await?;
            }
        }
        Ok(())
    }

    async fn handle_app_event(&mut self, event: Event) -> miette::Result<()> {
        match event {
            Event::PtyOutput { id, data } => {
                if let Some(proc) = self.processes.get_mut(id) {
                    proc.handle_output(&data);
                }
            }
            Event::ProcessExited { id, status } => {
                if let Some(proc) = self.processes.get_mut(id)
                    && proc.handle_exit(status)
                {
                    proc.start()?;
                }
            }
        }
        Ok(())
    }

    fn clear_search(&mut self) {
        self.search_query.clear();
        self.search_matches.clear();
        self.search_current = None;
        self.search_no_matches = false;
    }

    // Navigate to the next (or previous) match relative to the current viewport
    // center, reusing matches from the initial search accept. Positions may drift
    // slightly if streaming output shifts the scrollback buffer; the user can
    // re-search to refresh.
    fn search_navigate(&mut self, forward: bool) {
        if self.search_matches.is_empty()
            || self.processes[self.selected]
                .terminal()
                .is_alternate_screen()
        {
            return;
        }

        let total_rows = self.processes[self.selected].terminal_mut().total_rows();
        let scrollback = self.processes[self.selected].terminal().scrollback();
        let (rows, _) = self.processes[self.selected].terminal().size();
        let visible_center = total_rows.saturating_sub(scrollback + rows as usize / 2);

        // Don't wrap around — stop at the first/last match.
        let idx = if forward {
            self.search_matches
                .iter()
                .position(|&(row, _, _)| row > visible_center)
        } else {
            self.search_matches
                .iter()
                .rposition(|&(row, _, _)| row < visible_center)
        };
        let Some(idx) = idx else { return };
        self.search_current = Some(idx);
        self.processes[self.selected]
            .terminal_mut()
            .scroll_to_row(self.search_matches[idx].0);
    }

    fn resize_processes(&mut self) {
        // Tab bar (1) + bottom area (help_height or 1 for status bar).
        let bottom = if self.show_help {
            crate::ui::help_height(self.terminal_cols)
        } else {
            1
        };
        let viewport_rows = self.terminal_rows.saturating_sub(1 + bottom);
        for proc in &mut self.processes {
            proc.resize(viewport_rows, self.terminal_cols);
        }
    }

    fn begin_quit(&mut self) {
        if self.all_stopped() {
            self.should_quit = true;
            return;
        }
        self.mode = Mode::Quitting;
        self.show_help = false;
        self.resize_processes();
        for proc in &mut self.processes {
            proc.stop();
        }
    }

    fn has_running_processes(&self) -> bool {
        self.processes
            .iter()
            .any(|p| matches!(p.state(), State::Running | State::Stopping))
    }

    fn all_stopped(&self) -> bool {
        self.processes
            .iter()
            .all(|p| matches!(p.state(), State::Stopped | State::Failed))
    }

    fn scroll_lines(&self, amount: ScrollAmount) -> usize {
        let (rows, _) = self.processes[self.selected].terminal().size();
        let rows = rows as usize;
        match amount {
            ScrollAmount::Line => 1,
            ScrollAmount::FullPage => rows,
        }
    }
}

fn key_event_to_bytes(key: crossterm::event::KeyEvent) -> Vec<u8> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let mut bytes = Vec::new();
    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let byte = match c.to_ascii_lowercase() {
                    c @ 'a'..='z' => Some(c as u8 - b'a' + 1),
                    '@' | ' ' => Some(0x00),
                    '[' => Some(0x1b),
                    '\\' => Some(0x1c),
                    ']' => Some(0x1d),
                    '^' => Some(0x1e),
                    '_' => Some(0x1f),
                    _ => None,
                };
                if let Some(b) = byte {
                    bytes.push(b);
                }
            } else {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        KeyCode::Enter => bytes.push(b'\r'),
        KeyCode::Backspace => bytes.push(0x7f),
        KeyCode::Tab => bytes.push(b'\t'),
        KeyCode::Esc => bytes.push(0x1b),
        KeyCode::Up => bytes.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => bytes.extend_from_slice(b"\x1b[B"),
        KeyCode::Right => bytes.extend_from_slice(b"\x1b[C"),
        KeyCode::Left => bytes.extend_from_slice(b"\x1b[D"),
        KeyCode::Home => bytes.extend_from_slice(b"\x1b[H"),
        KeyCode::End => bytes.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => bytes.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => bytes.extend_from_slice(b"\x1b[6~"),
        KeyCode::Delete => bytes.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => bytes.extend_from_slice(b"\x1b[2~"),
        KeyCode::F(n) => {
            let seq = match n {
                1 => "\x1bOP",
                2 => "\x1bOQ",
                3 => "\x1bOR",
                4 => "\x1bOS",
                5 => "\x1b[15~",
                6 => "\x1b[17~",
                7 => "\x1b[18~",
                8 => "\x1b[19~",
                9 => "\x1b[20~",
                10 => "\x1b[21~",
                11 => "\x1b[23~",
                12 => "\x1b[24~",
                _ => return bytes,
            };
            bytes.extend_from_slice(seq.as_bytes());
        }
        _ => {}
    }
    bytes
}
