// --------------------------------------------------------------------------
// transport: minimal RFC6455 client over a unix socket (text frames, masked)
// --------------------------------------------------------------------------

use std::fs;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Accepted-transport classification for durable delivery observations: the
/// shared daemon took the turn. Not proof the turn produced output.
pub const TURN_START_ACCEPTED: &str = "turnStartAccepted";

/// Interrupt outcomes: the daemon aborted the running turn, or there was no
/// turn to abort (an idle thread is nothing to interrupt, not a failure).
pub const TURN_INTERRUPT_ACCEPTED: &str = "turnInterruptAccepted";
pub const NO_RUNNING_TURN: &str = "noRunningTurn";

fn _urandom(n: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    let mut file = fs::File::open("/dev/urandom")?;
    file.read_exact(&mut buf)?;
    Ok(buf)
}

fn _b64encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub(super) fn _find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

pub(super) fn _ws_send_frame(stream: &UnixStream, opcode: u8, payload: &[u8]) -> io::Result<()> {
    let n = payload.len();
    let mut frame = Vec::with_capacity(n + 14);
    frame.push(0x80 | opcode);
    if n < 126 {
        frame.push(0x80 | n as u8);
    } else if n < 65536 {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(n as u64).to_be_bytes());
    }
    let mask = _urandom(4)?;
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, c)| c ^ mask[i % 4]));
    let mut writer = stream;
    writer.write_all(&frame)
}

/// Python `_WSConn`.
pub struct WsConn {
    pub(super) stream: Arc<UnixStream>,
    rx: Vec<u8>,
}

impl WsConn {
    pub fn connect(path: &Path, timeout: Duration) -> io::Result<WsConn> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let mut conn = WsConn {
            stream: Arc::new(stream),
            rx: Vec::new(),
        };
        conn._handshake()?;
        // The timeout guards only the handshake. A live daemon can legally go
        // silent for 5s+ mid-call (its models refresh stalls exactly 5.00s on
        // a stale cache) — leaving it armed lets that silence kill the reader
        // thread right before the response.
        conn.stream.set_read_timeout(None)?;
        conn.stream.set_write_timeout(None)?;
        Ok(conn)
    }

    fn _handshake(&mut self) -> io::Result<()> {
        let key = _b64encode(&_urandom(16)?);
        let req = format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\n\
             Upgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        {
            let mut writer = &*self.stream;
            writer.write_all(req.as_bytes())?;
        }
        let mut data: Vec<u8> = Vec::new();
        while _find(&data, b"\r\n\r\n").is_none() {
            let mut chunk = [0u8; 4096];
            let n = {
                let mut reader = &*self.stream;
                reader.read(&mut chunk)?
            };
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "app-server handshake closed early",
                ));
            }
            data.extend_from_slice(&chunk[..n]);
        }
        let head_end = _find(&data, b"\r\n").unwrap_or(data.len());
        if _find(&data[..head_end], b"101").is_none() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!(
                    "app-server handshake rejected: {}",
                    String::from_utf8_lossy(&data[..data.len().min(64)])
                ),
            ));
        }
        let body_start = _find(&data, b"\r\n\r\n").unwrap_or(data.len() - 4) + 4;
        self.rx = data[body_start..].to_vec();
        Ok(())
    }

    fn _recv_exact(&mut self, n: usize) -> io::Result<Vec<u8>> {
        while self.rx.len() < n {
            let mut chunk = [0u8; 65536];
            let read = {
                let mut reader = &*self.stream;
                reader.read(&mut chunk)?
            };
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "app-server connection closed",
                ));
            }
            self.rx.extend_from_slice(&chunk[..read]);
        }
        let rest = self.rx.split_off(n);
        Ok(std::mem::replace(&mut self.rx, rest))
    }

    fn _recv_frame(&mut self) -> io::Result<(bool, u8, Vec<u8>)> {
        let header = self._recv_exact(2)?;
        let (b0, b1) = (header[0], header[1]);
        let fin = b0 & 0x80 != 0;
        let opcode = b0 & 0x0F;
        let masked = b1 & 0x80 != 0;
        let mut length = (b1 & 0x7F) as u64;
        if length == 126 {
            let bytes = self._recv_exact(2)?;
            length = u16::from_be_bytes([bytes[0], bytes[1]]) as u64;
        } else if length == 127 {
            let bytes = self._recv_exact(8)?;
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&bytes);
            length = u64::from_be_bytes(raw);
        }
        let mask = if masked {
            self._recv_exact(4)?
        } else {
            Vec::new()
        };
        let mut payload = self._recv_exact(length as usize)?;
        if masked {
            for (i, c) in payload.iter_mut().enumerate() {
                *c ^= mask[i % 4];
            }
        }
        Ok((fin, opcode, payload))
    }

    pub fn recv_text(&mut self) -> io::Result<String> {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let (fin, opcode, payload) = self._recv_frame()?;
            if opcode == 0x8 {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "app-server sent close",
                ));
            }
            if opcode == 0x9 {
                _ws_send_frame(&self.stream, 0xA, &payload)?;
                continue;
            }
            if opcode == 0xA {
                continue;
            }
            buf.extend_from_slice(&payload);
            if fin {
                return Ok(String::from_utf8_lossy(&buf).into_owned());
            }
        }
    }

    pub fn send_text(&self, text: &str) -> io::Result<()> {
        _ws_send_frame(&self.stream, 0x1, text.as_bytes())
    }

    pub fn close(&self) {
        let _ = _ws_send_frame(&self.stream, 0x8, b"");
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}
