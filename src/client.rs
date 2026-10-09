// Client side of detached mode: start, attach to, and stop the server.

use crate::protocol::{ClientMsg, Hello, ServerMsg};
use crate::server::Paths;
use crossterm::event::EventStream;
use futures_util::StreamExt;
use miette::{IntoDiagnostic, Result, bail};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::signal::unix::SignalKind;

/// Written by the server to stdout once its socket is accepting connections.
pub const READY: &str = "ready";
/// Written instead of READY when another server already owns the config.
pub const BUSY: &str = "busy";

pub enum Up {
    Started,
    AlreadyRunning,
}

pub enum AttachEnd {
    /// The server is still running.
    Detached,
    /// The server shut down normally.
    Exited,
    /// The connection dropped without the server saying goodbye.
    Lost,
}

pub async fn up(config_path: &Path, config_dir: &Path, paths: &Paths) -> Result<Up> {
    if UnixStream::connect(&paths.socket).await.is_ok() {
        return Ok(Up::AlreadyRunning);
    }
    // Validate here so config errors reach the user's terminal, not the log.
    crate::config::Config::load(config_path)?;

    // Append: if another server wins the race, this must not clobber its log.
    let log = std::fs::File::options()
        .create(true)
        .append(true)
        .open(&paths.log)
        .into_diagnostic()?;
    // Run from the config's directory so processes don't depend on where
    // whoever started the server happened to be.
    let mut child = Command::new(std::env::current_exe().into_diagnostic()?)
        .current_dir(config_dir)
        .env("PWD", config_dir)
        .arg("--config")
        .arg(config_path)
        .arg("server")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log)
        .spawn()
        .into_diagnostic()?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .into_diagnostic()?;
    if line.trim_end() != READY {
        // It has exited; don't leave a zombie for the rest of the session.
        let _ = child.wait();
    }
    match line.trim_end() {
        READY => Ok(Up::Started),
        // The winner holds the lock but may not have bound its socket yet.
        BUSY => {
            for _ in 0..40 {
                if UnixStream::connect(&paths.socket).await.is_ok() {
                    return Ok(Up::AlreadyRunning);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            bail!(
                "mtack is starting elsewhere but never became reachable; see {}",
                paths.log.display()
            );
        }
        _ => {
            bail!("server failed to start; see {}", paths.log.display());
        }
    }
}

/// Returns None if no server is running for this config.
pub async fn attach(paths: &Paths) -> Result<Option<AttachEnd>> {
    let Ok(stream) = UnixStream::connect(&paths.socket).await else {
        return Ok(None);
    };
    let (mut reader, mut writer) = stream.into_split();

    let (cols, rows) = crossterm::terminal::size().into_diagnostic()?;
    Hello::Attach { cols, rows }
        .write(&mut writer)
        .await
        .into_diagnostic()?;

    let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate()).into_diagnostic()?;
    setup_terminal()?;
    let input = tokio::spawn(async move {
        let mut events = EventStream::new();
        while let Some(Ok(event)) = events.next().await {
            let Some(msg) = ClientMsg::from_event(event) else {
                continue;
            };
            if msg.write(&mut writer).await.is_err() {
                break;
            }
        }
    });

    let end = loop {
        // Reading a frame isn't cancel-safe, but SIGTERM ends the connection.
        let msg = tokio::select! {
            msg = ServerMsg::read(&mut reader) => msg,
            _ = sigterm.recv() => break AttachEnd::Detached,
        };
        match msg {
            Ok(ServerMsg::Output(bytes)) => {
                let mut stdout = std::io::stdout().lock();
                if stdout.write_all(&bytes).is_err() || stdout.flush().is_err() {
                    break AttachEnd::Detached;
                }
            }
            Ok(ServerMsg::Detached) => break AttachEnd::Detached,
            Ok(ServerMsg::Exited) => break AttachEnd::Exited,
            Err(_) => break AttachEnd::Lost,
        }
    };
    input.abort();
    restore_terminal();
    Ok(Some(end))
}

/// Asks the server to stop its processes and waits for it to exit. Returns
/// false if it wasn't running.
pub async fn down(paths: &Paths) -> Result<bool> {
    let Ok(mut stream) = UnixStream::connect(&paths.socket).await else {
        return Ok(false);
    };
    Hello::Quit.write(&mut stream).await.into_diagnostic()?;
    // The server holds the connection until it exits.
    let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut [0]).await;
    Ok(true)
}

fn setup_terminal() -> Result<()> {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        original_hook(info);
    }));
    crossterm::terminal::enable_raw_mode().into_diagnostic()?;
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableFocusChange,
    )
    .inspect_err(|_| restore_terminal())
    .into_diagnostic()
}

fn restore_terminal() {
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableFocusChange,
        crossterm::event::DisableMouseCapture,
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show,
    );
    let _ = crossterm::terminal::disable_raw_mode();
}
