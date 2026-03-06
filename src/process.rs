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

pub enum ShouldRestart {
    Yes,
    No,
}

enum Lifecycle {
    Stopped,
    Running {
        writer: pty_process::OwnedWritePty,
        pid: Option<Pid>,
        handle: JoinHandle<()>,
    },
    Stopping {
        pending_restart: bool,
    },
    Failed,
}

/// Send a signal to the process group led by `pid`.
fn signal_process_group(pid: Pid, sig: Signal) {
    let _ = signal::kill(Pid::from_raw(-pid.as_raw()), sig);
}

impl Lifecycle {
    fn state(&self) -> State {
        match self {
            Lifecycle::Stopped => State::Stopped,
            Lifecycle::Running { .. } => State::Running,
            Lifecycle::Stopping { .. } => State::Stopping,
            Lifecycle::Failed => State::Failed,
        }
    }
}

pub struct Process {
    id: usize,
    terminal: Terminal,
    lifecycle: Lifecycle,
    config: ProcConfig,
    shutdown_timeout: Duration,
    event_tx: mpsc::Sender<Event>,
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
            terminal: Terminal::new(rows, cols, scrollback),
            lifecycle: Lifecycle::Stopped,
            config,
            shutdown_timeout,
            event_tx,
            pause_tx,
            last_start_time: None,
        }
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    pub fn terminal(&self) -> &Terminal {
        &self.terminal
    }

    pub fn terminal_mut(&mut self) -> &mut Terminal {
        &mut self.terminal
    }

    pub fn state(&self) -> State {
        self.lifecycle.state()
    }

    pub fn start(&mut self) -> miette::Result<()> {
        if matches!(self.lifecycle, Lifecycle::Running { .. }) {
            return Ok(());
        }

        if self.last_start_time.is_some() {
            self.terminal.inject_banner("restarted");
        }

        let (pty, pts) = pty_process::open().map_err(|e| miette::miette!("{e}"))?;
        let (rows, cols) = self.terminal.size();
        pty.resize(Size::new(rows, cols))
            .map_err(|e| miette::miette!("{e}"))?;

        let mut cmd = pty_process::Command::new(&self.config.program)
            .args(&self.config.args)
            .env("TERM", "xterm-256color")
            .envs(self.config.env.iter().map(|(k, v)| (k, v)));
        if let Some(cwd) = &self.config.cwd {
            cmd = cmd.current_dir(cwd);
        }

        let child = cmd.spawn(pts).map_err(|e| miette::miette!("{e}"))?;
        let pid = child.id().map(|id| {
            let raw = i32::try_from(id).expect("pid overflow");
            Pid::from_raw(raw)
        });
        let (pty_reader, writer) = pty.into_split();

        self.lifecycle = Lifecycle::Running {
            writer,
            pid,
            handle: spawn_watcher(
                self.id,
                pty_reader,
                child,
                self.event_tx.clone(),
                self.pause_tx.subscribe(),
            ),
        };
        self.last_start_time = Some(Instant::now());
        self.sync_paused();

        Ok(())
    }

    pub fn stop(&mut self) {
        if !matches!(self.lifecycle, Lifecycle::Running { .. }) {
            return;
        }
        let _ = self.pause_tx.send(false);

        let Lifecycle::Running {
            writer,
            pid,
            handle,
        } = std::mem::replace(
            &mut self.lifecycle,
            Lifecycle::Stopping {
                pending_restart: false,
            },
        )
        else {
            unreachable!();
        };

        // SIGTERM the process group before dropping the PTY writer, so the
        // child doesn't see SIGHUP (from PTY close) before our SIGTERM.
        if let Some(pid) = pid {
            signal_process_group(pid, Signal::SIGTERM);
        }
        drop(writer);

        // Spawn a SIGKILL escalation timer. The watcher task sends
        // ProcessExited when the child exits regardless of signal.
        let timeout = self.shutdown_timeout;
        tokio::spawn(async move {
            if tokio::time::timeout(timeout, handle).await.is_err()
                && let Some(pid) = pid
            {
                signal_process_group(pid, Signal::SIGKILL);
            }
        });
    }

    pub fn restart(&mut self) -> ShouldRestart {
        match &self.lifecycle {
            Lifecycle::Stopped | Lifecycle::Failed => return ShouldRestart::Yes,
            Lifecycle::Running { .. } => self.stop(),
            Lifecycle::Stopping { .. } => {}
        }
        if let Lifecycle::Stopping { pending_restart } = &mut self.lifecycle {
            *pending_restart = true;
        }
        ShouldRestart::No
    }

    pub async fn write(&mut self, data: &[u8]) -> miette::Result<()> {
        if let Lifecycle::Running { writer, .. } = &mut self.lifecycle {
            writer
                .write_all(data)
                .await
                .map_err(|e| miette::miette!("{e}"))?;
        }
        Ok(())
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.terminal.resize(rows, cols);
        if let Lifecycle::Running { writer, .. } = &self.lifecycle {
            let _ = writer.resize(Size::new(rows, cols));
        }
    }

    pub fn handle_output(&mut self, data: &[u8]) {
        self.terminal.process(data);
    }

    pub fn handle_exit(&mut self, status: ProcessStatus) -> ShouldRestart {
        let (was_stopping, pending_restart) = match &self.lifecycle {
            Lifecycle::Stopped | Lifecycle::Failed => return ShouldRestart::No,
            Lifecycle::Running { .. } => (false, false),
            Lifecycle::Stopping { pending_restart } => (true, *pending_restart),
        };

        self.lifecycle = if was_stopping {
            Lifecycle::Stopped
        } else {
            match status {
                ProcessStatus::Success => Lifecycle::Stopped,
                ProcessStatus::Failed(_) | ProcessStatus::Signal => Lifecycle::Failed,
            }
        };

        if pending_restart {
            return ShouldRestart::Yes;
        }
        let too_fast = self
            .last_start_time
            .is_some_and(|t| t.elapsed() < CRASH_LOOP_THRESHOLD);

        if !was_stopping
            && self.config.autorestart
            && !too_fast
            && matches!(status, ProcessStatus::Success | ProcessStatus::Failed(_))
        {
            ShouldRestart::Yes
        } else {
            ShouldRestart::No
        }
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
        // Extract Running resources so SIGTERM is sent before writer is dropped.
        if let Lifecycle::Running { pid: Some(pid), .. } =
            std::mem::replace(&mut self.lifecycle, Lifecycle::Stopped)
        {
            signal_process_group(pid, Signal::SIGTERM);
        }
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
            program: "echo".into(),
            args: Vec::new(),
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

    /// Put process into Running state with real PTY resources.
    fn set_running(proc: &mut Process) {
        let (pty, _pts) = pty_process::open().unwrap();
        let (_reader, writer) = pty.into_split();
        proc.lifecycle = Lifecycle::Running {
            writer,
            pid: None,
            handle: tokio::spawn(std::future::pending::<()>()),
        };
    }

    fn set_stopping(proc: &mut Process, pending_restart: bool) {
        proc.lifecycle = Lifecycle::Stopping { pending_restart };
    }

    #[test]
    fn restart_on_stopped_returns_needs_start() {
        let mut proc = test_process(true);
        assert_eq!(proc.state(), State::Stopped);
        assert!(matches!(proc.restart(), ShouldRestart::Yes));
        assert_eq!(proc.state(), State::Stopped);
    }

    #[tokio::test]
    async fn restart_on_running_transitions_to_stopping() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        assert!(matches!(proc.restart(), ShouldRestart::No));
        assert_eq!(proc.state(), State::Stopping);
        assert!(matches!(
            proc.lifecycle,
            Lifecycle::Stopping {
                pending_restart: true
            }
        ));
    }

    #[test]
    fn stop_on_stopped_is_noop() {
        let mut proc = test_process(true);
        assert_eq!(proc.state(), State::Stopped);
        proc.stop();
        assert_eq!(proc.state(), State::Stopped);
    }

    #[test]
    fn handle_exit_with_pending_restart() {
        let mut proc = test_process(true);
        set_stopping(&mut proc, true);
        assert!(matches!(proc.handle_exit(ProcessStatus::Success), ShouldRestart::Yes));
        assert_eq!(proc.state(), State::Stopped);
    }

    #[tokio::test]
    async fn handle_exit_with_autorestart_on_success() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        assert!(matches!(proc.handle_exit(ProcessStatus::Success), ShouldRestart::Yes));
    }

    #[tokio::test]
    async fn handle_exit_with_autorestart_on_failure() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        assert!(matches!(proc.handle_exit(ProcessStatus::Failed(1)), ShouldRestart::Yes));
    }

    #[tokio::test]
    async fn handle_exit_with_autorestart_on_signal() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        assert!(matches!(proc.handle_exit(ProcessStatus::Signal), ShouldRestart::No));
    }

    #[tokio::test]
    async fn handle_exit_after_explicit_stop() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        proc.stop();
        assert_eq!(proc.state(), State::Stopping);
        assert!(matches!(proc.handle_exit(ProcessStatus::Success), ShouldRestart::No));
    }

    #[tokio::test]
    async fn handle_exit_without_autorestart() {
        let mut proc = test_process(false);
        set_running(&mut proc);
        assert!(matches!(proc.handle_exit(ProcessStatus::Success), ShouldRestart::No));
    }

    #[test]
    fn handle_exit_on_already_stopped() {
        let mut proc = test_process(true);
        assert!(matches!(proc.handle_exit(ProcessStatus::Success), ShouldRestart::No));
    }

    #[tokio::test]
    async fn crash_loop_suppresses_autorestart() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        proc.last_start_time = Some(Instant::now());
        assert!(matches!(proc.handle_exit(ProcessStatus::Success), ShouldRestart::No));
    }

    #[test]
    fn pending_restart_bypasses_crash_loop_check() {
        let mut proc = test_process(true);
        proc.last_start_time = Some(Instant::now());
        set_stopping(&mut proc, true);
        assert!(matches!(proc.handle_exit(ProcessStatus::Success), ShouldRestart::Yes));
    }

    #[tokio::test]
    async fn handle_exit_sets_failed_on_nonzero_exit() {
        let mut proc = test_process(false);
        set_running(&mut proc);
        proc.handle_exit(ProcessStatus::Failed(1));
        assert_eq!(proc.state(), State::Failed);
    }

    #[tokio::test]
    async fn handle_exit_sets_failed_on_signal() {
        let mut proc = test_process(false);
        set_running(&mut proc);
        proc.handle_exit(ProcessStatus::Signal);
        assert_eq!(proc.state(), State::Failed);
    }

    #[test]
    fn handle_exit_sets_stopped_after_explicit_stop() {
        let mut proc = test_process(false);
        set_stopping(&mut proc, false);
        proc.handle_exit(ProcessStatus::Failed(1));
        assert_eq!(proc.state(), State::Stopped);
    }

    #[test]
    fn restart_on_failed_returns_needs_start() {
        let mut proc = test_process(true);
        proc.lifecycle = Lifecycle::Failed;
        assert!(matches!(proc.restart(), ShouldRestart::Yes));
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
