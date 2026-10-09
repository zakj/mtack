// Wire format between `mtack attach` and the background server.
//
// Every message is one frame: a tag byte, a big-endian u32 payload length, and
// the payload. Input events travel as the escape sequence a terminal would send
// for them; one frame holds exactly one event, so parsing never has to guess
// where an ambiguous sequence (like a lone ESC) ends.

use crossterm::event::Event as CtEvent;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

const TAG_ATTACH: u8 = b'a';
const TAG_QUIT: u8 = b'q';
const TAG_EVENT: u8 = b'e';
const TAG_RESIZE: u8 = b'r';
const TAG_OUTPUT: u8 = b'o';
const TAG_DETACHED: u8 = b'd';
const TAG_EXITED: u8 = b'x';

/// First message on a new connection.
#[derive(Debug, PartialEq, Eq)]
pub enum Hello {
    Attach { cols: u16, rows: u16 },
    Quit,
}

/// Messages from an attached client after `Hello::Attach`.
#[derive(Debug, PartialEq, Eq)]
pub enum ClientMsg {
    Event(CtEvent),
    Resize { cols: u16, rows: u16 },
}

#[derive(Debug, PartialEq, Eq)]
pub enum ServerMsg {
    /// Bytes to write verbatim to the client's terminal.
    Output(Vec<u8>),
    Detached,
    /// The server is shutting down normally. A connection that closes without
    /// this means the server died.
    Exited,
}

impl Hello {
    pub async fn write(&self, w: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
        match self {
            Self::Attach { cols, rows } => {
                write_frame(w, TAG_ATTACH, &encode_size(*cols, *rows)).await
            }
            Self::Quit => write_frame(w, TAG_QUIT, &[]).await,
        }
    }

    pub async fn read(r: &mut (impl AsyncRead + Unpin)) -> io::Result<Self> {
        let (tag, payload) = read_frame(r).await?;
        match tag {
            TAG_ATTACH => {
                let (cols, rows) = decode_size(&payload)?;
                Ok(Self::Attach { cols, rows })
            }
            TAG_QUIT => Ok(Self::Quit),
            _ => Err(invalid("unexpected hello")),
        }
    }
}

impl ClientMsg {
    /// Converts a terminal event for sending. Returns None for events the wire
    /// format can't carry; mtack doesn't act on those.
    pub fn from_event(event: CtEvent) -> Option<Self> {
        match event {
            CtEvent::Resize(cols, rows) => Some(Self::Resize { cols, rows }),
            event => encode_event(&event).ok().map(|_| Self::Event(event)),
        }
    }

    pub async fn write(&self, w: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
        match self {
            Self::Event(event) => write_frame(w, TAG_EVENT, &encode_event(event)?).await,
            Self::Resize { cols, rows } => {
                write_frame(w, TAG_RESIZE, &encode_size(*cols, *rows)).await
            }
        }
    }

    /// Returns None for an event this build can't decode. The frame is still
    /// consumed, so one odd key doesn't cost the client its connection.
    pub async fn read(r: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<Self>> {
        let (tag, payload) = read_frame(r).await?;
        match tag {
            TAG_EVENT => Ok(decode_event(&payload).ok().map(Self::Event)),
            TAG_RESIZE => {
                let (cols, rows) = decode_size(&payload)?;
                Ok(Some(Self::Resize { cols, rows }))
            }
            _ => Err(invalid("unexpected client message")),
        }
    }
}

impl ServerMsg {
    pub async fn write(&self, w: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
        match self {
            Self::Output(bytes) => write_frame(w, TAG_OUTPUT, bytes).await,
            Self::Detached => write_frame(w, TAG_DETACHED, &[]).await,
            Self::Exited => write_frame(w, TAG_EXITED, &[]).await,
        }
    }

    pub async fn read(r: &mut (impl AsyncRead + Unpin)) -> io::Result<Self> {
        let (tag, payload) = read_frame(r).await?;
        match tag {
            TAG_OUTPUT => Ok(Self::Output(payload)),
            TAG_DETACHED => Ok(Self::Detached),
            TAG_EXITED => Ok(Self::Exited),
            _ => Err(invalid("unexpected server message")),
        }
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

async fn write_frame(w: &mut (impl AsyncWrite + Unpin), tag: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| invalid("frame too large"))?;
    let mut header = [0u8; 5];
    header[0] = tag;
    header[1..].copy_from_slice(&len.to_be_bytes());
    w.write_all(&header).await?;
    w.write_all(payload).await?;
    w.flush().await
}

async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> io::Result<(u8, Vec<u8>)> {
    let tag = r.read_u8().await?;
    let len = r.read_u32().await? as usize;
    if len > MAX_FRAME_LEN {
        return Err(invalid("frame too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    Ok((tag, payload))
}

fn encode_size(cols: u16, rows: u16) -> [u8; 4] {
    let [c0, c1] = cols.to_be_bytes();
    let [r0, r1] = rows.to_be_bytes();
    [c0, c1, r0, r1]
}

fn decode_size(payload: &[u8]) -> io::Result<(u16, u16)> {
    let [c0, c1, r0, r1] = payload else {
        return Err(invalid("bad size"));
    };
    Ok((
        u16::from_be_bytes([*c0, *c1]),
        u16::from_be_bytes([*r0, *r1]),
    ))
}

fn encode_event(event: &CtEvent) -> io::Result<Vec<u8>> {
    let event = terminput_crossterm::to_terminput(event.clone())
        .map_err(|_| invalid("unsupported event"))?;
    let mut buf = [0u8; 64];
    let n = event.encode(&mut buf, terminput::Encoding::Xterm)?;
    Ok(buf[..n].to_vec())
}

fn decode_event(payload: &[u8]) -> io::Result<CtEvent> {
    let event =
        terminput::Event::parse_from(payload)?.ok_or_else(|| invalid("incomplete event"))?;
    terminput_crossterm::to_crossterm(event).map_err(|_| invalid("unsupported event"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    fn key(code: KeyCode, modifiers: KeyModifiers) -> CtEvent {
        CtEvent::Key(KeyEvent::new(code, modifiers))
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> CtEvent {
        CtEvent::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    // Every event mtack's keymap and mouse handling act on must arrive intact.
    #[tokio::test]
    async fn events_mtack_handles_survive_the_wire() {
        let events = [
            key(KeyCode::Char('q'), KeyModifiers::NONE),
            key(KeyCode::Char('Q'), KeyModifiers::SHIFT),
            key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            key(KeyCode::Char('f'), KeyModifiers::CONTROL),
            key(KeyCode::Char('?'), KeyModifiers::NONE),
            key(KeyCode::Char('/'), KeyModifiers::NONE),
            key(KeyCode::Char('é'), KeyModifiers::NONE),
            key(KeyCode::Esc, KeyModifiers::NONE),
            key(KeyCode::Enter, KeyModifiers::NONE),
            key(KeyCode::Tab, KeyModifiers::NONE),
            key(KeyCode::BackTab, KeyModifiers::SHIFT),
            key(KeyCode::Backspace, KeyModifiers::NONE),
            key(KeyCode::Up, KeyModifiers::NONE),
            key(KeyCode::PageDown, KeyModifiers::NONE),
            key(KeyCode::Home, KeyModifiers::NONE),
            mouse(MouseEventKind::ScrollUp, 3, 7),
            mouse(MouseEventKind::ScrollDown, 0, 0),
            mouse(MouseEventKind::Down(MouseButton::Left), 12, 0),
            CtEvent::FocusGained,
            CtEvent::FocusLost,
        ];
        let (mut a, mut b) = tokio::io::duplex(4096);
        for event in events {
            let msg = ClientMsg::from_event(event.clone()).expect("event is sendable");
            msg.write(&mut a).await.unwrap();
            assert_eq!(
                ClientMsg::read(&mut b).await.unwrap(),
                Some(ClientMsg::Event(event))
            );
        }
    }

    #[tokio::test]
    async fn undecodable_event_is_skipped_without_losing_the_stream() {
        let (mut a, mut b) = tokio::io::duplex(64);
        // Alt+O encodes to a sequence the parser reads as incomplete.
        write_frame(&mut a, TAG_EVENT, b"\x1bO").await.unwrap();
        ClientMsg::Resize { cols: 80, rows: 24 }
            .write(&mut a)
            .await
            .unwrap();
        assert_eq!(ClientMsg::read(&mut b).await.unwrap(), None);
        assert_eq!(
            ClientMsg::read(&mut b).await.unwrap(),
            Some(ClientMsg::Resize { cols: 80, rows: 24 })
        );
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let len = u32::try_from(MAX_FRAME_LEN + 1).unwrap();
        let mut header = vec![TAG_OUTPUT];
        header.extend_from_slice(&len.to_be_bytes());
        a.write_all(&header).await.unwrap();
        let err = ServerMsg::read(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn resize_and_hello_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        Hello::Attach { cols: 300, rows: 2 }
            .write(&mut a)
            .await
            .unwrap();
        assert_eq!(
            Hello::read(&mut b).await.unwrap(),
            Hello::Attach { cols: 300, rows: 2 }
        );

        let resize = ClientMsg::from_event(CtEvent::Resize(80, 24)).unwrap();
        resize.write(&mut a).await.unwrap();
        assert_eq!(
            ClientMsg::read(&mut b).await.unwrap(),
            Some(ClientMsg::Resize { cols: 80, rows: 24 })
        );
    }
}
