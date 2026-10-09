// Process lifecycle: spawn, stop, restart, PTY management.

use crate::config::ProcConfig;
use crate::event::{Event, ProcessStatus};
use crate::terminal::Terminal;
use bytes::{Buf, Bytes, BytesMut};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use pty_process::Size;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::error::TrySendError;
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

/// Pending input per process, in messages. A paste arrives one key per
/// message, so this is sized for large pastes into a process that's busy.
/// Input beyond it is dropped: a process that isn't reading must not stall
/// the App.
const INPUT_QUEUE_SIZE: usize = 64 * 1024;

enum Lifecycle {
    Stopped,
    Running {
        /// Dropping these ends the writer task.
        input: mpsc::Sender<Bytes>,
        size: watch::Sender<Size>,
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
    dropping_input: bool,
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
            dropping_input: false,
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

    /// A spawn failure leaves the process Failed with the error shown in its
    /// output, so one bad command can't take down the others.
    pub fn start(&mut self) {
        if matches!(self.lifecycle, Lifecycle::Running { .. }) {
            return;
        }

        if self.last_start_time.is_some() {
            self.terminal.inject_banner("restarted");
        }

        match self.spawn() {
            Ok(running) => {
                self.lifecycle = running;
                self.dropping_input = false;
                self.last_start_time = Some(Instant::now());
                self.sync_paused();
            }
            Err(e) => {
                self.terminal
                    .inject_banner(&format!("failed to start {}: {e}", self.config.program));
                self.lifecycle = Lifecycle::Failed;
            }
        }
    }

    fn spawn(&self) -> miette::Result<Lifecycle> {
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
        let (input, input_rx) = mpsc::channel(INPUT_QUEUE_SIZE);
        let (size, size_rx) = watch::channel(Size::new(rows, cols));
        tokio::spawn(write_input(writer, input_rx, size_rx));

        Ok(Lifecycle::Running {
            input,
            size,
            pid,
            handle: spawn_watcher(
                self.id,
                pty_reader,
                child,
                self.event_tx.clone(),
                self.pause_tx.subscribe(),
            ),
        })
    }

    pub fn stop(&mut self) {
        if !matches!(self.lifecycle, Lifecycle::Running { .. }) {
            return;
        }
        let _ = self.pause_tx.send(false);

        let Lifecycle::Running {
            input,
            size,
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

        if let Some(pid) = pid {
            signal_process_group(pid, Signal::SIGTERM);
        }
        drop((input, size));

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

    /// Queues input without waiting. Input is dropped if the process has
    /// fallen too far behind reading, or its PTY has closed (its exit event
    /// follows).
    pub fn write(&mut self, data: &[u8]) {
        let Lifecycle::Running { input, .. } = &self.lifecycle else {
            return;
        };
        // After an overflow, drop everything until the queue drains, so a
        // paste loses one clean tail rather than holes that splice lines.
        if self.dropping_input && input.capacity() < input.max_capacity() {
            return;
        }
        let full = matches!(
            input.try_send(Bytes::copy_from_slice(data)),
            Err(TrySendError::Full(_))
        );
        if full && !self.dropping_input {
            self.terminal
                .inject_banner("input dropped: process isn't reading fast enough");
        }
        self.dropping_input = full;
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.terminal.resize(rows, cols);
        if let Lifecycle::Running { size, .. } = &self.lifecycle {
            size.send_replace(Size::new(rows, cols));
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
        proc.lifecycle = Lifecycle::Running {
            input: mpsc::channel(1).0,
            size: watch::channel(Size::new(24, 80)).0,
            pid: None,
            handle: tokio::spawn(std::future::pending::<()>()),
        };
    }

    fn set_stopping(proc: &mut Process, pending_restart: bool) {
        proc.lifecycle = Lifecycle::Stopping { pending_restart };
    }

    #[tokio::test]
    async fn written_input_reaches_the_process() {
        let (tx, mut rx) = mpsc::channel(64);
        let mut config = test_config(false);
        config.program = "cat".into();
        let mut proc = Process::new(0, config, 24, 80, 100, Duration::from_secs(5), tx);
        proc.start();
        proc.write(b"hello from mtack\n");

        let mut output = Vec::new();
        let found = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(Event::PtyOutput { data, .. }) = rx.recv().await {
                output.extend_from_slice(&data);
                // Once echoed by the terminal, once printed by cat.
                if output
                    .windows(16)
                    .filter(|w| w == b"hello from mtack")
                    .count()
                    == 2
                {
                    return;
                }
            }
        })
        .await;
        assert!(found.is_ok(), "{}", String::from_utf8_lossy(&output));
    }

    fn shell_process(script: &str) -> (Process, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::channel(1024);
        let mut config = test_config(false);
        config.program = "sh".into();
        config.args = vec!["-c".into(), script.into()];
        let mut proc = Process::new(0, config, 24, 80, 100, Duration::from_secs(5), tx);
        proc.start();
        (proc, rx)
    }

    // Whole lines throughout: a terminal discards an overlong partial line
    // instead of queueing it.

    #[tokio::test]
    async fn resize_applies_while_input_is_stuck() {
        let (mut proc, mut rx) = shell_process("stty -echo; sleep 0.5; stty size; sleep 30");
        let line = [b"x".repeat(39), b"\n".to_vec()].concat();
        for _ in 0..4096 {
            proc.write(&line);
        }
        // Let the writer fill the terminal's input buffer and get stuck.
        tokio::time::sleep(Duration::from_millis(100)).await;
        proc.resize(33, 77);

        let mut output = Vec::new();
        let found = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(Event::PtyOutput { data, .. }) = rx.recv().await {
                output.extend_from_slice(&data);
                if output.windows(5).any(|w| w == b"33 77") {
                    return;
                }
            }
        })
        .await;
        assert!(found.is_ok(), "{}", String::from_utf8_lossy(&output));
    }

    #[tokio::test]
    async fn overflowing_input_loses_only_a_clean_tail() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let (mut proc, _rx) = shell_process(&format!(
            "stty -echo; while IFS= read -r l; do printf '%s\\n' \"$l\"; done > {}",
            out.display()
        ));
        tokio::time::sleep(Duration::from_millis(300)).await;

        // A paste arrives one key at a time. Overflow the queue before the
        // writer can run, then keep going with the writer draining between
        // keys, as it does in the App.
        let lines = (INPUT_QUEUE_SIZE + 20_000) / 7;
        let sent: Vec<u8> = (0..lines)
            .flat_map(|n| format!("{n:06}\n").into_bytes())
            .collect();
        for (i, byte) in sent.iter().enumerate() {
            proc.write(&[*byte]);
            if i > INPUT_QUEUE_SIZE && i % 256 == 0 {
                tokio::task::yield_now().await;
            }
        }

        let mut received = Vec::new();
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let now = std::fs::read(&out).unwrap_or_default();
            if !now.is_empty() && now.len() == received.len() {
                break;
            }
            received = now;
        }
        assert!(received.len() < sent.len(), "nothing overflowed");
        assert!(sent.starts_with(&received), "input arrived with holes");
        let screen = proc.terminal().screen().contents();
        assert!(screen.contains("input dropped"), "{screen}");
    }

    #[tokio::test]
    async fn start_with_missing_program_marks_failed() {
        let mut proc = test_process(false);
        proc.config.program = "mtack-test-no-such-program".into();
        proc.start();
        assert_eq!(proc.state(), State::Failed);
        assert!(
            proc.terminal()
                .screen()
                .contents()
                .contains("failed to start")
        );
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
        assert!(matches!(
            proc.handle_exit(ProcessStatus::Success),
            ShouldRestart::Yes
        ));
        assert_eq!(proc.state(), State::Stopped);
    }

    #[tokio::test]
    async fn handle_exit_with_autorestart_on_success() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        assert!(matches!(
            proc.handle_exit(ProcessStatus::Success),
            ShouldRestart::Yes
        ));
    }

    #[tokio::test]
    async fn handle_exit_with_autorestart_on_failure() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        assert!(matches!(
            proc.handle_exit(ProcessStatus::Failed(1)),
            ShouldRestart::Yes
        ));
    }

    #[tokio::test]
    async fn handle_exit_with_autorestart_on_signal() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        assert!(matches!(
            proc.handle_exit(ProcessStatus::Signal),
            ShouldRestart::No
        ));
    }

    #[tokio::test]
    async fn handle_exit_after_explicit_stop() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        proc.stop();
        assert_eq!(proc.state(), State::Stopping);
        assert!(matches!(
            proc.handle_exit(ProcessStatus::Success),
            ShouldRestart::No
        ));
    }

    #[tokio::test]
    async fn handle_exit_without_autorestart() {
        let mut proc = test_process(false);
        set_running(&mut proc);
        assert!(matches!(
            proc.handle_exit(ProcessStatus::Success),
            ShouldRestart::No
        ));
    }

    #[test]
    fn handle_exit_on_already_stopped() {
        let mut proc = test_process(true);
        assert!(matches!(
            proc.handle_exit(ProcessStatus::Success),
            ShouldRestart::No
        ));
    }

    #[tokio::test]
    async fn crash_loop_suppresses_autorestart() {
        let mut proc = test_process(true);
        set_running(&mut proc);
        proc.last_start_time = Some(Instant::now());
        assert!(matches!(
            proc.handle_exit(ProcessStatus::Success),
            ShouldRestart::No
        ));
    }

    #[test]
    fn pending_restart_bypasses_crash_loop_check() {
        let mut proc = test_process(true);
        proc.last_start_time = Some(Instant::now());
        set_stopping(&mut proc, true);
        assert!(matches!(
            proc.handle_exit(ProcessStatus::Success),
            ShouldRestart::Yes
        ));
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

/// Feeds queued input to the PTY. Resizes go on a separate channel so they
/// apply even while a write is stuck behind a process that isn't reading.
async fn write_input(
    mut pty: pty_process::OwnedWritePty,
    mut input: mpsc::Receiver<Bytes>,
    mut size: watch::Receiver<Size>,
) {
    let mut pending = Bytes::new();
    loop {
        // `write` is cancel-safe: if another branch wins, nothing was written.
        tokio::select! {
            changed = size.changed() => {
                if changed.is_err() {
                    return;
                }
                let _ = pty.resize(*size.borrow_and_update());
            }
            data = input.recv(), if pending.is_empty() => match data {
                Some(data) => pending = data,
                None => return,
            },
            written = pty.write(&pending), if !pending.is_empty() => match written {
                Ok(n) if n > 0 => pending.advance(n),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                // The child has likely closed its terminal and its exit event
                // will follow; drop this input but keep serving until stopped.
                _ => pending.clear(),
            },
        }
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
