// Top-level App state, event loop, frame rendering.

use crate::config::Config;
use crate::event::Event;
use crate::input::{self, Action, Mode, ScrollAmount};
use crate::process::{Process, ShouldRestart, State};
use crate::protocol::ClientMsg;
use crate::server::{self, Client, Drawn, ServerEvent};
use crate::terminal::MatchSpan;
use crossterm::event::{Event as CtEvent, MouseButton, MouseEventKind};
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Interval, MissedTickBehavior};

enum SearchDirection {
    Forward,
    Backward,
}

#[derive(Default)]
pub struct SearchState {
    pub query: String,
    pub last_query: String,
    matches: Vec<MatchSpan>,
    current: Option<usize>,
}

impl SearchState {
    pub fn no_matches(&self) -> bool {
        !self.query.is_empty() && self.matches.is_empty()
    }

    pub fn current(&self) -> Option<usize> {
        self.current
    }

    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn clear(&mut self) {
        self.query.clear();
        self.matches.clear();
        self.current = None;
    }
}

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
    dirty: bool,
    search: SearchState,
    focused: bool,
    client: Option<Client>,
    /// Goodbyes to detached clients that may still be in flight.
    goodbyes: Vec<JoinHandle<()>>,
}

impl App {
    pub fn new(config: &Config) -> Self {
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_SIZE);

        // Use a placeholder size; will be resized when a client attaches.
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
                    config.shutdown_timeout,
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
            dirty: true,
            search: SearchState::default(),
            focused: true,
            client: None,
            goodbyes: Vec::new(),
        }
    }

    pub async fn run(&mut self, listener: UnixListener) -> miette::Result<()> {
        self.resize_processes();

        // Auto-start processes.
        for proc in &mut self.processes {
            if proc.autostart() {
                proc.start();
            }
        }

        let (server_tx, mut server_rx) = mpsc::channel(EVENT_CHANNEL_SIZE);
        server::listen(listener, server_tx);
        let mut render_interval = new_render_interval(RENDER_INTERVAL_FOCUSED);

        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|e| miette::miette!("{e}"))?;
        let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .map_err(|e| miette::miette!("{e}"))?;

        loop {
            tokio::select! {
                _ = render_interval.tick(), if self.dirty && self.client.is_some() => {
                    self.draw();
                }
                Some(event) = server_rx.recv() => {
                    self.handle_server_event(event);
                }
                msg = next_client_msg(&mut self.client) => match msg {
                    Some(msg) => self.handle_client_msg(msg),
                    None => self.drop_client(),
                },
                Some(event) = self.event_rx.recv() => {
                    let needs_render = match &event {
                        Event::PtyOutput { id, .. } => *id == self.selected,
                        Event::ProcessExited { .. } => true,
                    };
                    self.handle_app_event(event);
                    self.dirty |= needs_render;
                }
                _ = sigterm.recv() => {
                    self.begin_quit();
                }
                _ = sighup.recv() => {
                    self.begin_quit();
                }
            }
            if self.should_quit || (self.mode == Mode::Quitting && self.all_stopped()) {
                break;
            }
            let period = if self.focused {
                RENDER_INTERVAL_FOCUSED
            } else {
                RENDER_INTERVAL_UNFOCUSED
            };
            if render_interval.period() != period {
                render_interval = new_render_interval(period);
            }
        }
        let mut goodbyes = std::mem::take(&mut self.goodbyes);
        goodbyes.extend(self.client.take().map(Client::exit));
        // Clients that attached as we quit; later ones are told by their
        // connection task once sending to us fails.
        server_rx.close();
        while let Ok(event) = server_rx.try_recv() {
            if let ServerEvent::Attach { client, .. } = event {
                goodbyes.push(client.exit());
            }
        }
        server::flush_goodbyes(goodbyes).await;
        Ok(())
    }

    fn draw(&mut self) {
        let Some(client) = &mut self.client else {
            return;
        };
        // Inside the closure, so a busy client costs no search.
        let drawn = client.draw(|frame| {
            let visible_matches = if !self.search.query.is_empty() && self.mode != Mode::Search {
                self.processes[self.selected]
                    .terminal_mut()
                    .find_visible_matches(&self.search.query)
                    .to_vec()
            } else {
                Vec::new()
            };
            let ctx = crate::ui::RenderContext {
                processes: &self.processes,
                selected: self.selected,
                mode: self.mode,
                show_help: self.show_help,
                search: &self.search,
                remaining_count: self
                    .processes
                    .iter()
                    .filter(|p| matches!(p.state(), State::Running | State::Stopping))
                    .count(),
                visible_matches: &visible_matches,
            };
            crate::ui::render(frame, &ctx);
        });
        match drawn {
            Drawn::Sent => self.dirty = false,
            Drawn::Busy => {}
            Drawn::Disconnected => self.drop_client(),
        }
    }

    fn handle_server_event(&mut self, event: ServerEvent) {
        match event {
            ServerEvent::Attach { client, cols, rows } => {
                self.detach();
                self.set_size(cols, rows);
                self.client = Some(client);
                self.dirty = true;
            }
            ServerEvent::Quit => self.begin_quit(),
        }
    }

    fn handle_client_msg(&mut self, msg: ClientMsg) {
        match msg {
            ClientMsg::Event(event) => {
                self.handle_crossterm_event(event);
                self.processes[self.selected].sync_paused();
            }
            ClientMsg::Resize { cols, rows } => {
                if let Some(client) = &mut self.client {
                    client.resize(cols, rows);
                }
                self.set_size(cols, rows);
            }
        }
        self.dirty = true;
    }

    fn detach(&mut self) {
        if let Some(client) = self.client.take() {
            self.goodbyes.retain(|goodbye| !goodbye.is_finished());
            self.goodbyes.push(client.detach());
        }
        self.reset_view();
    }

    fn drop_client(&mut self) {
        self.client = None;
        self.reset_view();
    }

    /// Drops per-client view state so the next client starts clean. Scrolling
    /// to the bottom also unpauses any process that was paused by scrollback.
    fn reset_view(&mut self) {
        if self.mode != Mode::Quitting {
            self.mode = Mode::Normal;
        }
        self.show_help = false;
        self.search.clear();
        self.focused = true;
        for proc in &mut self.processes {
            proc.terminal_mut().scroll_to_bottom();
            proc.sync_paused();
        }
    }

    fn set_size(&mut self, cols: u16, rows: u16) {
        self.terminal_cols = cols;
        self.terminal_rows = rows;
        self.resize_processes();
    }

    fn handle_crossterm_event(&mut self, event: CtEvent) {
        match event {
            CtEvent::Key(key) => {
                let unfocus_key = self.processes[self.selected].unfocus_key();
                if let Some(action) = input::resolve(key, self.mode, unfocus_key) {
                    self.handle_action(action);
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
                        self.search.clear();
                        if self.mode == Mode::Focused {
                            self.mode = Mode::Normal;
                        }
                    }
                }
                _ => {}
            },
            CtEvent::FocusGained => self.focused = true,
            CtEvent::FocusLost => self.focused = false,
            _ => {}
        }
    }

    fn handle_action(&mut self, action: Action) {
        match action {
            Action::SelectTab(idx) => {
                if idx < self.processes.len() {
                    self.selected = idx;
                    self.search.clear();
                }
            }
            Action::NextTab => {
                if !self.processes.is_empty() {
                    self.selected = (self.selected + 1) % self.processes.len();
                    self.search.clear();
                }
            }
            Action::PrevTab => {
                if !self.processes.is_empty() {
                    self.selected =
                        (self.selected + self.processes.len() - 1) % self.processes.len();
                    self.search.clear();
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
                self.processes[self.selected].start();
            }
            Action::StopProcess => {
                self.processes[self.selected].stop();
            }
            Action::RestartProcess => {
                if matches!(self.processes[self.selected].restart(), ShouldRestart::Yes) {
                    self.processes[self.selected].start();
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
                self.search.clear();
            }
            Action::SearchInput(c) => {
                self.search.query.push(c);
            }
            Action::SearchBackspace => {
                self.search.query.pop();
            }
            Action::SearchFillPlaceholder => {
                if self.search.query.is_empty() && !self.search.last_query.is_empty() {
                    self.search.query.clone_from(&self.search.last_query);
                }
            }
            Action::SearchAccept => {
                // Enter with empty query and a placeholder: use the placeholder.
                if self.search.query.is_empty() && !self.search.last_query.is_empty() {
                    self.search.query.clone_from(&self.search.last_query);
                }
                if !self.processes[self.selected]
                    .terminal()
                    .is_alternate_screen()
                {
                    let (matches, total_rows) = self.processes[self.selected]
                        .terminal_mut()
                        .find_all_matches(&self.search.query);
                    self.search.matches = matches.to_vec();
                    if !self.search.matches.is_empty() {
                        // Jump to last match at or before current viewport.
                        let scrollback = self.processes[self.selected].terminal().scrollback();
                        let (rows, _) = self.processes[self.selected].terminal().size();
                        let visible_top = total_rows.saturating_sub(rows as usize + scrollback);
                        let visible_bottom = visible_top + rows as usize;

                        let idx = self
                            .search
                            .matches
                            .iter()
                            .rposition(|m| m.row <= visible_bottom)
                            .unwrap_or(self.search.matches.len() - 1);
                        self.search.current = Some(idx);
                        self.processes[self.selected]
                            .terminal_mut()
                            .scroll_to_row(self.search.matches[idx].row);
                    }
                }
                if !self.search.query.is_empty() {
                    self.search.last_query.clone_from(&self.search.query);
                }
                self.mode = Mode::Normal;
            }
            Action::SearchCancel => {
                self.search.clear();
                self.processes[self.selected]
                    .terminal_mut()
                    .scroll_to_bottom();
                self.mode = Mode::Normal;
            }
            Action::SearchNext => {
                self.search_navigate(SearchDirection::Forward);
            }
            Action::SearchPrev => {
                self.search_navigate(SearchDirection::Backward);
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
                if self.mode == Mode::Quitting {
                    self.should_quit = true;
                } else {
                    self.begin_quit();
                }
            }
            Action::Detach => {
                self.detach();
            }
            Action::CancelQuit => {
                self.mode = Mode::Normal;
            }
            Action::ForwardKey(key) => {
                let bytes = key_event_to_bytes(key);
                self.processes[self.selected].write(&bytes);
            }
        }
    }

    fn handle_app_event(&mut self, event: Event) {
        match event {
            Event::PtyOutput { id, data } => {
                if let Some(proc) = self.processes.get_mut(id) {
                    proc.handle_output(&data);
                }
            }
            Event::ProcessExited { id, status } => {
                if let Some(proc) = self.processes.get_mut(id)
                    && matches!(proc.handle_exit(status), ShouldRestart::Yes)
                {
                    proc.start();
                }
            }
        }
    }

    // Navigate to the next (or previous) match relative to the current viewport
    // center, reusing matches from the initial search accept. Positions may drift
    // slightly if streaming output shifts the scrollback buffer; the user can
    // re-search to refresh.
    fn search_navigate(&mut self, direction: SearchDirection) {
        if self.search.matches.is_empty()
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
        let idx = match direction {
            SearchDirection::Forward => self
                .search
                .matches
                .iter()
                .position(|m| m.row > visible_center),
            SearchDirection::Backward => self
                .search
                .matches
                .iter()
                .rposition(|m| m.row < visible_center),
        };
        let Some(idx) = idx else { return };
        self.search.current = Some(idx);
        self.processes[self.selected]
            .terminal_mut()
            .scroll_to_row(self.search.matches[idx].row);
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
        self.dirty = true;
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
    let Ok(tkey) = terminput_crossterm::to_terminput_key(key) else {
        return Vec::new();
    };
    let event = terminput::Event::Key(tkey);
    let mut buf = [0u8; 64];
    match event.encode(&mut buf, terminput::Encoding::Xterm) {
        Ok(n) => buf[..n].to_vec(),
        Err(_) => Vec::new(),
    }
}

fn new_render_interval(period: Duration) -> Interval {
    let mut interval = tokio::time::interval(period);
    // Ticks missed while nothing was dirty or attached shouldn't replay as a
    // burst of back-to-back renders.
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    interval
}

/// Resolves to None when the attached client disconnects; never resolves while
/// no client is attached.
async fn next_client_msg(client: &mut Option<Client>) -> Option<ClientMsg> {
    match client {
        Some(client) => client.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Hello, ServerMsg};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::path::{Path, PathBuf};
    use tokio::net::UnixStream;
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

    const CONFIG: &str = r#"proc "alpha" { cmd "true"; autostart #false; }"#;
    const COLS: u16 = 100;
    const ROWS: u16 = 30;
    const HELP_TEXT: &str = "go to tab";
    /// Prints wide lines that each repeat one count, so every frame rewrites
    /// most of a big screen and fills any socket buffer fast.
    const SPEW_CONFIG: &str = r##"proc "spew" { cmd "awk" #"BEGIN { for (i = 1; ; i++) { l = ""; while (length(l) < 400) l = l i " "; print l; fflush() } }"#; }"##;
    const BIG_COLS: u16 = 500;
    const BIG_ROWS: u16 = 100;

    struct TestClient {
        reader: OwnedReadHalf,
        writer: OwnedWriteHalf,
        screen: vt100::Parser,
    }

    impl TestClient {
        async fn attach(socket: &Path) -> Self {
            Self::attach_sized(socket, COLS, ROWS).await
        }

        async fn attach_sized(socket: &Path, cols: u16, rows: u16) -> Self {
            let (reader, mut writer) = UnixStream::connect(socket).await.unwrap().into_split();
            Hello::Attach { cols, rows }
                .write(&mut writer)
                .await
                .unwrap();
            Self {
                reader,
                writer,
                screen: vt100::Parser::new(rows, cols, 0),
            }
        }

        async fn key(&mut self, c: char) {
            let event = CtEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            ClientMsg::Event(event)
                .write(&mut self.writer)
                .await
                .unwrap();
        }

        fn contents(&self) -> String {
            self.screen.screen().contents()
        }

        /// Applies the next message if it's a frame; returns it otherwise.
        async fn apply(&mut self) -> Option<ServerMsg> {
            match ServerMsg::read(&mut self.reader).await.unwrap() {
                ServerMsg::Output(bytes) => {
                    self.screen.process(&bytes);
                    None
                }
                other => Some(other),
            }
        }

        async fn next_frame(&mut self) {
            if let Some(other) = self.apply().await {
                panic!("expected output, got {other:?}");
            }
        }

        /// Applies rendered frames until the screen satisfies `done`.
        async fn wait_for(&mut self, done: impl Fn(&str) -> bool) {
            while !done(&self.contents()) {
                self.next_frame().await;
            }
        }

        /// Applies `n` frames, checking the screen after each.
        async fn check_frames(&mut self, n: usize, check: impl Fn(&str) -> bool) {
            for _ in 0..n {
                self.next_frame().await;
                let contents = self.contents();
                assert!(check(&contents), "{contents}");
            }
        }

        /// Applies rendered frames until the server sends anything else.
        async fn next_non_output(&mut self) -> ServerMsg {
            loop {
                if let Some(other) = self.apply().await {
                    return other;
                }
            }
        }
    }

    async fn send_quit(socket: &Path) {
        let mut stream = UnixStream::connect(socket).await.unwrap();
        Hello::Quit.write(&mut stream).await.unwrap();
    }

    async fn with_app<F: Future<Output = ()>>(test: impl FnOnce(PathBuf) -> F) {
        with_app_config(CONFIG, test).await;
    }

    /// Runs an App on a temporary socket alongside `test`, which must end by
    /// asking the App to quit. What `test` returns is dropped only after the
    /// App has returned.
    async fn with_app_config<T, F: Future<Output = T>>(
        config: &str,
        test: impl FnOnce(PathBuf) -> F,
    ) -> T {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("test.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let mut app = App::new(&crate::config::parse(config).unwrap());
        let (result, kept) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(app.run(listener), test(socket))
        })
        .await
        .expect("test timed out");
        result.unwrap();
        kept
    }

    #[tokio::test]
    async fn attached_client_sees_tabs_and_detach_hint() {
        with_app(|socket| async move {
            let mut client = TestClient::attach(&socket).await;
            client
                .wait_for(|s| s.contains("alpha") && s.contains("detach"))
                .await;
            send_quit(&socket).await;
        })
        .await;
    }

    #[tokio::test]
    async fn second_attach_detaches_the_first() {
        with_app(|socket| async move {
            let mut first = TestClient::attach(&socket).await;
            first.wait_for(|s| s.contains("alpha")).await;
            let mut second = TestClient::attach(&socket).await;
            assert_eq!(first.next_non_output().await, ServerMsg::Detached);
            second.wait_for(|s| s.contains("alpha")).await;
            send_quit(&socket).await;
        })
        .await;
    }

    #[tokio::test]
    async fn detach_key_ends_session_and_next_client_starts_clean() {
        with_app(|socket| async move {
            let mut first = TestClient::attach(&socket).await;
            first.wait_for(|s| s.contains("alpha")).await;
            first.key('?').await;
            first.wait_for(|s| s.contains(HELP_TEXT)).await;
            first.key('d').await;
            assert_eq!(first.next_non_output().await, ServerMsg::Detached);

            let mut second = TestClient::attach(&socket).await;
            second.wait_for(|s| s.contains("alpha")).await;
            assert!(!second.contents().contains(HELP_TEXT));
            send_quit(&socket).await;
        })
        .await;
    }

    #[tokio::test]
    async fn quit_says_goodbye_to_a_client_attaching_as_it_happens() {
        with_app(|socket| async move {
            let mut first = TestClient::attach(&socket).await;
            first.wait_for(|s| s.contains("alpha")).await;
            send_quit(&socket).await;
            let mut second = TestClient::attach(&socket).await;
            assert_eq!(first.next_non_output().await, ServerMsg::Exited);
            assert_eq!(second.next_non_output().await, ServerMsg::Exited);
        })
        .await;
    }

    #[tokio::test]
    async fn quit_says_goodbye_to_the_attached_client() {
        with_app(|socket| async move {
            let mut client = TestClient::attach(&socket).await;
            client.wait_for(|s| s.contains("alpha")).await;
            send_quit(&socket).await;
            assert_eq!(client.next_non_output().await, ServerMsg::Exited);
        })
        .await;
    }

    /// Whether SPEW_CONFIG's lines on screen are whole and consecutive. Frames
    /// are diffs, so one that went missing would leave stale cells behind.
    fn spew_is_whole(screen: &str) -> bool {
        let counts: Vec<Vec<u64>> = screen
            .lines()
            .filter_map(|line| line.split_whitespace().map(|t| t.parse().ok()).collect())
            .filter(|counts: &Vec<u64>| !counts.is_empty())
            .collect();
        // The newest line may have arrived only partly.
        let Some((_, whole)) = counts.split_last() else {
            return false;
        };
        whole.len() > 50
            && whole.iter().all(|line| line.iter().all(|&n| n == line[0]))
            && whole.windows(2).all(|w| w[1][0] == w[0][0] + 1)
    }

    #[tokio::test]
    async fn client_that_stops_reading_cannot_stall_the_server() {
        with_app_config(SPEW_CONFIG, |socket| async move {
            let stuck = TestClient::attach_sized(&socket, BIG_COLS, BIG_ROWS).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            send_quit(&socket).await;
            // Kept open until the App returns, so a hangup can't unstick it.
            stuck
        })
        .await;
    }

    #[tokio::test]
    async fn client_that_falls_behind_catches_up_then_gets_its_goodbye() {
        with_app_config(SPEW_CONFIG, |socket| async move {
            let mut first = TestClient::attach_sized(&socket, BIG_COLS, BIG_ROWS).await;
            first.wait_for(spew_is_whole).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            // Well past the backlog, into frames rendered after the stall.
            first.check_frames(20, spew_is_whole).await;
            // Fall behind again so the goodbye queues behind a frame.
            tokio::time::sleep(Duration::from_millis(200)).await;

            let mut second = TestClient::attach(&socket).await;
            second.wait_for(|s| s.contains("spew")).await;
            assert_eq!(first.next_non_output().await, ServerMsg::Detached);
            assert!(spew_is_whole(&first.contents()));
            send_quit(&socket).await;
        })
        .await;
    }
}
