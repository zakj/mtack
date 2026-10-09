// Background server: owns the socket, turns client connections into events
// for App, and renders frames for the attached client.

use crate::protocol::{ClientMsg, Hello, ServerMsg};
use miette::{IntoDiagnostic, Result, WrapErr, bail};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use std::ffi::OsString;
use std::fs::{File, TryLockError};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::{SendError, TrySendError};
use tokio::task::JoinHandle;

const CLIENT_CHANNEL_SIZE: usize = 64;
/// Room for one frame and a goodbye. A frame is only queued when none is
/// waiting, so a goodbye always fits.
const OUTPUT_QUEUE_SIZE: usize = 2;
/// How long shutdown waits for goodbyes to reach clients.
const GOODBYE_TIMEOUT: Duration = Duration::from_secs(1);

pub enum ServerEvent {
    Attach {
        client: Client,
        cols: u16,
        rows: u16,
    },
    Quit,
}

/// Files for the server that runs a given config file.
pub struct Paths {
    pub config: PathBuf,
    pub socket: PathBuf,
    pub log: PathBuf,
    pub lock: PathBuf,
}

/// A per-user 0700 directory: anyone who can connect to a server's socket can
/// type into every process's terminal. Fixed rather than under $TMPDIR, which
/// differs between login sessions, ssh, and sudo; the per-config lock only
/// works if every shell agrees on where it lives.
fn runtime_dir() -> Result<PathBuf> {
    let uid = nix::unistd::getuid().as_raw();
    let base =
        std::env::var_os("MTACK_TMPDIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    let dir = base.join(format!("mtack-{uid}"));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to create {}", dir.display()))?;
    let meta = std::fs::symlink_metadata(&dir).into_diagnostic()?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        bail!(
            "{} must be a directory owned by you with mode 0700",
            dir.display()
        );
    }
    Ok(dir)
}

/// Config files of servers that are accepting connections.
pub fn running_configs() -> Result<Vec<PathBuf>> {
    Ok(list_running(&runtime_dir()?))
}

fn list_running(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut configs: Vec<PathBuf> = entries
        .filter_map(|entry| {
            let socket = entry.ok()?.path();
            if socket.extension()? != "sock" {
                return None;
            }
            std::os::unix::net::UnixStream::connect(&socket).ok()?;
            let config = std::fs::read(socket.with_extension("lock")).ok()?;
            Some(PathBuf::from(OsString::from_vec(config)))
        })
        .collect();
    configs.sort();
    configs
}

impl Paths {
    pub fn for_config(config_path: &Path) -> Result<Self> {
        let dir = runtime_dir()?;

        // Client and server are the same binary, so the hash only has to be
        // stable within one build.
        let mut hasher = DefaultHasher::new();
        config_path.hash(&mut hasher);
        let name = format!("{:016x}", hasher.finish());
        Ok(Self {
            config: config_path.to_path_buf(),
            socket: dir.join(format!("{name}.sock")),
            log: dir.join(format!("{name}.log")),
            lock: dir.join(format!("{name}.lock")),
        })
    }
}

pub enum Bound {
    Listening(UnixListener, SocketGuard),
    AlreadyRunning,
}

/// Holds the server's lock; on drop, removes the socket before releasing it,
/// so a server that is starting up can't lose its new socket to us.
pub struct SocketGuard {
    socket: PathBuf,
    _lock: File,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Claims the config's lock, then binds its socket. The lock, not the socket
/// file, decides which server owns a config: two servers starting at once
/// would otherwise each unlink the other's socket. The OS drops the lock if
/// the server dies, so a leftover socket file is safe to replace.
pub fn bind(paths: &Paths) -> Result<Bound> {
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&paths.lock)
        .into_diagnostic()?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(Bound::AlreadyRunning),
        Err(TryLockError::Error(e)) => return Err(e).into_diagnostic(),
    }
    // Written before the socket exists, so anything listing running servers
    // can always say which config one belongs to.
    lock.set_len(0).into_diagnostic()?;
    (&lock)
        .write_all(paths.config.as_os_str().as_bytes())
        .into_diagnostic()?;
    // If this fails, bind reports it.
    let _ = std::fs::remove_file(&paths.socket);
    let listener = UnixListener::bind(&paths.socket)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to bind {}", paths.socket.display()))?;
    let guard = SocketGuard {
        socket: paths.socket.clone(),
        _lock: lock,
    };
    Ok(Bound::Listening(listener, guard))
}

/// Accepts connections forever, forwarding what they send to `tx`.
pub fn listen(listener: UnixListener, tx: mpsc::Sender<ServerEvent>) {
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(handle_connection(stream, tx.clone()));
        }
    });
}

async fn handle_connection(stream: UnixStream, tx: mpsc::Sender<ServerEvent>) {
    let (mut reader, writer) = stream.into_split();
    match Hello::read(&mut reader).await {
        Ok(Hello::Attach { cols, rows }) => {
            let (msg_tx, msg_rx) = mpsc::channel(CLIENT_CHANNEL_SIZE);
            let client = Client::new(writer, msg_rx, cols, rows);
            if let Err(SendError(ServerEvent::Attach { client, .. })) =
                tx.send(ServerEvent::Attach { client, cols, rows }).await
            {
                // The App has already quit.
                flush_goodbyes(vec![client.exit()]).await;
                return;
            }
            // Dropping msg_tx when the client hangs up tells App it's gone;
            // a failed send means App has already replaced this client.
            while let Ok(msg) = ClientMsg::read(&mut reader).await {
                let Some(msg) = msg else { continue };
                if msg_tx.send(msg).await.is_err() {
                    return;
                }
            }
        }
        Ok(Hello::Quit) => {
            let _ = tx.send(ServerEvent::Quit).await;
            // Hold the connection open so `mtack down` returns once the
            // server has actually exited.
            let _ = reader.read(&mut [0]).await;
        }
        Err(_) => {}
    }
}

/// Collects what ratatui renders so it can be sent as one frame. Shared
/// because the backend owns its writer and ratatui only exposes it behind an
/// unstable feature.
#[derive(Clone, Default)]
struct OutputBuf(Arc<Mutex<Vec<u8>>>);

impl OutputBuf {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut self.0.lock().expect("never poisoned"))
    }
}

impl io::Write for OutputBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("never poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub enum Drawn {
    Sent,
    /// A frame is still queued behind the one being written; nothing was
    /// rendered.
    Busy,
    Disconnected,
}

pub struct Client {
    /// Frames and goodbyes, in order, for the writer task. Dropping it ends
    /// the task once the queue is delivered.
    outbox: mpsc::Sender<ServerMsg>,
    writer: JoinHandle<()>,
    msgs: mpsc::Receiver<ClientMsg>,
    output: OutputBuf,
    terminal: Terminal<CrosstermBackend<OutputBuf>>,
}

impl Client {
    fn new(writer: OwnedWriteHalf, msgs: mpsc::Receiver<ClientMsg>, cols: u16, rows: u16) -> Self {
        // A fixed viewport keeps ratatui from asking the backend for its size,
        // which would query the server's own (nonexistent) terminal.
        let output = OutputBuf::default();
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(output.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)),
            },
        )
        .expect("OutputBuf writes cannot fail");
        terminal.clear().expect("OutputBuf writes cannot fail");
        let (outbox, outbox_rx) = mpsc::channel(OUTPUT_QUEUE_SIZE);
        Self {
            outbox,
            writer: tokio::spawn(write_output(writer, outbox_rx)),
            msgs,
            output,
            terminal,
        }
    }

    pub async fn recv(&mut self) -> Option<ClientMsg> {
        self.msgs.recv().await
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.terminal
            .resize(Rect::new(0, 0, cols, rows))
            .expect("OutputBuf writes cannot fail");
    }

    /// Renders a frame and queues it for the client without waiting. Each
    /// frame is a diff against the one before, so none may be dropped;
    /// instead, while the client is behind, nothing is rendered and the next
    /// frame covers everything that changed meanwhile.
    pub fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> Drawn {
        if self.outbox.capacity() < self.outbox.max_capacity() {
            return Drawn::Busy;
        }
        self.terminal
            .draw(render)
            .expect("OutputBuf writes cannot fail");
        match self.outbox.try_send(ServerMsg::Output(self.output.take())) {
            Ok(()) => Drawn::Sent,
            Err(TrySendError::Closed(_)) => Drawn::Disconnected,
            Err(TrySendError::Full(_)) => {
                unreachable!("a frame is only queued when none is waiting")
            }
        }
    }

    /// Queues a goodbye after any pending frames. The returned task ends
    /// once it's delivered or the client hangs up.
    pub fn detach(self) -> JoinHandle<()> {
        self.say_goodbye(ServerMsg::Detached)
    }

    pub fn exit(self) -> JoinHandle<()> {
        self.say_goodbye(ServerMsg::Exited)
    }

    fn say_goodbye(self, msg: ServerMsg) -> JoinHandle<()> {
        let _ = self.outbox.try_send(msg);
        self.writer
    }
}

/// Sends a client what the App queues for it. The App never waits on this,
/// so a client that stops reading can't stall it.
async fn write_output(mut writer: OwnedWriteHalf, mut outbox: mpsc::Receiver<ServerMsg>) {
    while let Some(msg) = outbox.recv().await {
        if msg.write(&mut writer).await.is_err() {
            return;
        }
    }
}

/// Waits briefly for goodbye tasks from `Client::detach` and `Client::exit`
/// so shutdown doesn't cut them off, then closes any client still not
/// reading.
pub async fn flush_goodbyes(mut writers: Vec<JoinHandle<()>>) {
    let all = async {
        for writer in &mut writers {
            let _ = writer.await;
        }
    };
    if tokio::time::timeout(GOODBYE_TIMEOUT, all).await.is_err() {
        let stuck = writers.iter().filter(|w| !w.is_finished()).count();
        eprintln!(
            "closed {stuck} client(s) still receiving output {GOODBYE_TIMEOUT:?} after goodbye"
        );
        for writer in writers {
            writer.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths(dir: &Path) -> Paths {
        Paths {
            config: dir.join("mtack.kdl"),
            socket: dir.join("s.sock"),
            log: dir.join("s.log"),
            lock: dir.join("s.lock"),
        }
    }

    #[tokio::test]
    async fn running_server_cannot_be_displaced() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let _first = bind(&paths).unwrap();
        // What a racing server sees between another's lock and its bind.
        std::fs::remove_file(&paths.socket).unwrap();
        assert!(matches!(bind(&paths).unwrap(), Bound::AlreadyRunning));
    }

    #[tokio::test]
    async fn stale_socket_is_replaced_and_removed_on_exit() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        drop(std::os::unix::net::UnixListener::bind(&paths.socket).unwrap());
        assert!(paths.socket.exists());

        let Bound::Listening(_listener, guard) = bind(&paths).unwrap() else {
            panic!("stale socket blocked startup");
        };
        UnixStream::connect(&paths.socket).await.unwrap();
        drop(guard);
        assert!(!paths.socket.exists());
        assert!(matches!(bind(&paths).unwrap(), Bound::Listening(..)));
    }

    #[tokio::test]
    async fn running_servers_are_listed_by_config() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        assert!(list_running(dir.path()).is_empty());

        let Bound::Listening(_listener, guard) = bind(&paths).unwrap() else {
            panic!("bind failed");
        };
        assert_eq!(list_running(dir.path()), vec![paths.config.clone()]);
        drop(guard);
        assert!(list_running(dir.path()).is_empty());
    }
}
