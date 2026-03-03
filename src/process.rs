// Process lifecycle: spawn, stop, restart, PTY management.

use crate::config::ProcConfig;
use crate::event::{Event, ProcessStatus};
use crate::terminal::Terminal;
use bytes::BytesMut;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use pty_process::Size;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Stopped,
    Running,
    Stopping,
    Failed,
}

const CRASH_LOOP_THRESHOLD: Duration = Duration::from_secs(1);

pub struct Process {
    id: usize,
    name: String,
    terminal: Terminal,
    state: State,
    config: ProcConfig,
    shutdown_timeout: Duration,
    event_tx: mpsc::Sender<Event>,
    pty_writer: Option<pty_process::OwnedWritePty>,
    child_pid: Option<u32>,
    watcher_handle: Option<JoinHandle<()>>,
    pending_restart: bool,
    pause_tx: watch::Sender<bool>,
    last_start_time: Option<Instant>,
}

impl Process {
    pub fn new(
        id: usize,
        config: ProcConfig,
        rows: u16,
        cols: u16,
        scrollback: usize,
        shutdown_timeout: Duration,
        event_tx: mpsc::Sender<Event>,
    ) -> Self {
        let (pause_tx, _) = watch::channel(false);
        Self {
            id,
            name: config.name.clone(),
            terminal: Terminal::new(rows, cols, scrollback),
            state: State::Stopped,
            config,
            shutdown_timeout,
            event_tx,
            pty_writer: None,
            child_pid: None,
            watcher_handle: None,
            pending_restart: false,
            pause_tx,
            last_start_time: None,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn terminal(&self) -> &Terminal {
        &self.terminal
    }

    pub fn terminal_mut(&mut self) -> &mut Terminal {
        &mut self.terminal
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn start(&mut self) -> miette::Result<()> {
        if self.state == State::Running {
            return Ok(());
        }

        if self.last_start_time.is_some() {
            self.terminal.inject_banner("restarted");
        }

        let (pty, pts) = pty_process::open().map_err(|e| miette::miette!("{e}"))?;
        let (rows, cols) = self.terminal.size();
        pty.resize(Size::new(rows, cols))
            .map_err(|e| miette::miette!("{e}"))?;

        let mut cmd = pty_process::Command::new(&self.config.cmd[0])
            .args(&self.config.cmd[1..])
            .env("TERM", "xterm-256color")
            .envs(self.config.env.iter().map(|(k, v)| (k, v)));
        if let Some(cwd) = &self.config.cwd {
            cmd = cmd.current_dir(cwd);
        }

        let child = cmd.spawn(pts).map_err(|e| miette::miette!("{e}"))?;
        self.child_pid = child.id();
        let (pty_reader, pty_writer) = pty.into_split();

        self.pty_writer = Some(pty_writer);
        self.state = State::Running;
        self.last_start_time = Some(Instant::now());
        self.watcher_handle = Some(spawn_watcher(
            self.id,
            pty_reader,
            child,
            self.event_tx.clone(),
            self.pause_tx.subscribe(),
        ));
        self.sync_paused();

        Ok(())
    }

    pub fn stop(&mut self) {
        if self.state != State::Running {
            return;
        }
        let _ = self.pause_tx.send(false);
        let pid = self.child_pid.take();
        self.state = State::Stopping;

        // SIGTERM the process group before dropping the PTY writer, so the
        // child doesn't see SIGHUP (from PTY close) before our SIGTERM.
        if let Some(pid) = pid {
            let raw = i32::try_from(pid).expect("pid overflow");
            let _ = signal::kill(Pid::from_raw(-raw), Signal::SIGTERM);
        }
        self.pty_writer.take();

        // Spawn a SIGKILL escalation timer. The watcher task sends
        // ProcessExited when the child exits regardless of signal.
        if let Some(handle) = self.watcher_handle.take() {
            let timeout = self.shutdown_timeout;
            tokio::spawn(async move {
                if tokio::time::timeout(timeout, handle).await.is_err()
                    && let Some(pid) = pid
                {
                    let raw = i32::try_from(pid).expect("pid overflow");
                    let _ = signal::kill(Pid::from_raw(-raw), Signal::SIGKILL);
                }
            });
        }
    }

    /// Returns true if the process needs to be started (was already stopped).
    pub fn restart(&mut self) -> bool {
        if matches!(self.state, State::Stopped | State::Failed) {
            return true;
        }
        self.pending_restart = true;
        self.stop();
        false
    }

    pub async fn write(&mut self, data: &[u8]) -> miette::Result<()> {
        if let Some(writer) = &mut self.pty_writer {
            writer
                .write_all(data)
                .await
                .map_err(|e| miette::miette!("{e}"))?;
        }
        Ok(())
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.terminal.resize(rows, cols);
        if let Some(writer) = &self.pty_writer {
            let _ = writer.resize(Size::new(rows, cols));
        }
    }

    pub fn handle_output(&mut self, data: &[u8]) {
        self.terminal.process(data);
    }

    /// Returns true if the process should be restarted.
    pub fn handle_exit(&mut self, status: ProcessStatus) -> bool {
        if matches!(self.state, State::Stopped | State::Failed) {
            return false;
        }
        let was_stopping = self.state == State::Stopping;
        self.pty_writer.take();
        self.child_pid.take();
        self.watcher_handle.take();
        self.state = if was_stopping {
            State::Stopped
        } else {
            match status {
                ProcessStatus::Success => State::Stopped,
                ProcessStatus::Failed(_) | ProcessStatus::Signal => State::Failed,
            }
        };

        if self.pending_restart {
            self.pending_restart = false;
            return true;
        }
        let too_fast = self
            .last_start_time
            .is_some_and(|t| t.elapsed() < CRASH_LOOP_THRESHOLD);

        !was_stopping
            && self.config.autorestart
            && !too_fast
            && matches!(status, ProcessStatus::Success | ProcessStatus::Failed(_))
    }

    pub fn autostart(&self) -> bool {
        self.config.autostart
    }

    pub fn unfocus_key(&self) -> &crate::config::UnfocusKey {
        &self.config.unfocus_key
    }

    /// Sync the PTY reader's pause state with the terminal's scroll position.
    pub fn sync_paused(&self) {
        let _ = self.pause_tx.send(self.terminal.is_scrolled_back());
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.pause_tx.send(false);
        if let Some(pid) = self.child_pid.take() {
            let raw = i32::try_from(pid).expect("pid overflow");
            let _ = signal::kill(Pid::from_raw(-raw), Signal::SIGTERM);
        }
        self.pty_writer.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UnfocusKey;

    fn test_config(autorestart: bool) -> ProcConfig {
        ProcConfig {
            name: "test".into(),
            autostart: true,
            autorestart,
            cmd: vec!["echo".into()],
            cwd: None,
            env: Vec::new(),
            scrollback: None,
            unfocus_key: UnfocusKey::Esc,
        }
    }

    fn test_process(autorestart: bool) -> Process {
        let (tx, _rx) = mpsc::channel(8);
        Process::new(
            0,
            test_config(autorestart),
            24,
            80,
            100,
            Duration::from_secs(5),
            tx,
        )
    }

    /// Put process into Running state without spawning a real PTY.
    fn set_running(proc: &mut Process) {
        proc.state = State::Running;
    }

    #[test]
    fn restart_on_stopped_returns_needs_start() {
        let mut proc = test_process(true);
        assert_eq!(proc.state(), State::Stopped);
        assert!(proc.restart());
        assert_eq!(proc.state(), State::Stopped);
    }

    #[test]
    fn restart_on_running_transitions_to_stopping() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        assert!(!proc.restart());
        assert_eq!(proc.state(), State::Stopping);
        assert!(proc.pending_restart);
    }

    #[test]
    fn stop_on_stopped_is_noop() {
        let mut proc = test_process(true);
        assert_eq!(proc.state(), State::Stopped);
        proc.stop();
        assert_eq!(proc.state(), State::Stopped);
    }

    #[test]
    fn handle_exit_with_pending_restart_returns_true() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        proc.pending_restart = true;
        proc.state = State::Stopping;
        assert!(proc.handle_exit(ProcessStatus::Success));
        assert_eq!(proc.state(), State::Stopped);
        assert!(!proc.pending_restart);
    }

    #[test]
    fn handle_exit_with_autorestart_on_success() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        assert!(proc.handle_exit(ProcessStatus::Success));
    }

    #[test]
    fn handle_exit_with_autorestart_on_failure() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        assert!(proc.handle_exit(ProcessStatus::Failed(1)));
    }

    #[test]
    fn handle_exit_with_autorestart_on_signal_returns_false() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        assert!(!proc.handle_exit(ProcessStatus::Signal));
    }

    #[test]
    fn handle_exit_after_explicit_stop_returns_false() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        proc.stop();
        assert_eq!(proc.state(), State::Stopping);
        assert!(!proc.handle_exit(ProcessStatus::Success));
    }

    #[test]
    fn handle_exit_without_autorestart() {
        let mut proc = test_process(false);
        set_running(&mut proc);
        assert!(!proc.handle_exit(ProcessStatus::Success));
    }

    #[test]
    fn handle_exit_on_already_stopped() {
        let mut proc = test_process(true);
        assert!(!proc.handle_exit(ProcessStatus::Success));
    }

    #[test]
    fn crash_loop_suppresses_autorestart() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        proc.last_start_time = Some(Instant::now());
        assert!(!proc.handle_exit(ProcessStatus::Success));
    }

    #[test]
    fn pending_restart_bypasses_crash_loop_check() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        proc.last_start_time = Some(Instant::now());
        proc.pending_restart = true;
        proc.state = State::Stopping;
        assert!(proc.handle_exit(ProcessStatus::Success));
    }

    #[test]
    fn handle_exit_sets_failed_on_nonzero_exit() {
        let mut proc = test_process(false);
        set_running(&mut proc);
        proc.handle_exit(ProcessStatus::Failed(1));
        assert_eq!(proc.state(), State::Failed);
    }

    #[test]
    fn handle_exit_sets_failed_on_signal() {
        let mut proc = test_process(false);
        set_running(&mut proc);
        proc.handle_exit(ProcessStatus::Signal);
        assert_eq!(proc.state(), State::Failed);
    }

    #[test]
    fn handle_exit_sets_stopped_after_explicit_stop() {
        let mut proc = test_process(false);
        set_running(&mut proc);
        proc.state = State::Stopping;
        proc.handle_exit(ProcessStatus::Failed(1));
        assert_eq!(proc.state(), State::Stopped);
    }

    #[test]
    fn restart_on_failed_returns_needs_start() {
        let mut proc = test_process(true);
        proc.state = State::Failed;
        assert!(proc.restart());
    }
}

/// Reads PTY output, waits for child exit, and sends events.
fn spawn_watcher(
    id: usize,
    mut reader: pty_process::OwnedReadPty,
    mut child: tokio::process::Child,
    event_tx: mpsc::Sender<Event>,
    mut pause_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Read PTY output until EOF. BytesMut amortizes allocation across reads.
        let mut buf = BytesMut::with_capacity(32 * 1024);
        loop {
            // Backpressure: stop reading when the process is scrolled back.
            while *pause_rx.borrow_and_update() {
                if pause_rx.changed().await.is_err() {
                    return;
                }
            }
            match reader.read_buf(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let data = buf.split_to(n).freeze();
                    if event_tx.send(Event::PtyOutput { id, data }).await.is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }

        // Wait for the child to exit and report status.
        let status = match child.wait().await {
            Ok(exit) => {
                if exit.success() {
                    ProcessStatus::Success
                } else if let Some(code) = exit.code() {
                    ProcessStatus::Failed(code)
                } else {
                    ProcessStatus::Signal
                }
            }
            Err(_) => ProcessStatus::Signal,
        };
        let _ = event_tx.send(Event::ProcessExited { id, status }).await;
    })
}
