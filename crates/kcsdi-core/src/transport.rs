// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Blocking line-oriented transports.
//!
//! The KC901 byte stream is `\n` framed; a transport only needs to send raw
//! bytes and deliver whole lines. Partial lines are buffered internally so
//! callers always get complete lines without the trailing `\n`.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::error::{Error, Result};

/// TCP connect timeout (protocol doc 1.2: 5 s suggested).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Generic command/response timeout (protocol doc 8.1: 10000 ms).
pub const GENERIC_TIMEOUT: Duration = Duration::from_secs(10);

/// Byte transport for the KC901 text protocol.
pub trait Transport {
    /// Send raw bytes (usually one command line including `\n`).
    fn send(&mut self, data: &[u8]) -> Result<()>;

    /// Receive one line. The returned string has no trailing `\n` (a stray
    /// `\r` is also stripped). Returns [`Error::Timeout`] when no newline
    /// arrives within `timeout`.
    fn recv_line(&mut self, timeout: Duration) -> Result<String>;
}

/// Blocking TCP transport for network-equipped models (KC901V, ...).
pub struct TcpTransport {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl TcpTransport {
    /// Connect to `host:port` with a 5 s connect timeout.
    pub fn connect(host: &str, port: u16) -> Result<Self> {
        let mut last_err = None;
        for addr in (host, port).to_socket_addrs()? {
            match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                Ok(stream) => {
                    stream.set_nodelay(true)?;
                    return Ok(Self {
                        stream,
                        buf: Vec::with_capacity(4096),
                    });
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(Error::Io(last_err.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no address resolved")
        })))
    }
}

impl Transport for TcpTransport {
    fn send(&mut self, data: &[u8]) -> Result<()> {
        self.stream.write_all(data)?;
        Ok(())
    }

    fn recv_line(&mut self, timeout: Duration) -> Result<String> {
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                let text = String::from_utf8_lossy(&line);
                return Ok(text.trim_end_matches(['\n', '\r']).to_string());
            }
            self.stream.set_read_timeout(Some(timeout))?;
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(Error::NotConnected),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::TimedOut
                        || e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    return Err(Error::Timeout);
                }
                Err(e) => return Err(Error::Io(e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn spawn_server<F: FnOnce(TcpStream) + Send + 'static>(f: F) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            f(sock);
        });
        port
    }

    #[test]
    fn recv_line_reassembles_fragmented_writes() {
        let port = spawn_server(|mut sock| {
            sock.write_all(b"$sta").unwrap();
            std::thread::sleep(Duration::from_millis(30));
            sock.write_all(b"rt,id\n$0000000").unwrap();
            std::thread::sleep(Duration::from_millis(30));
            sock.write_all(b"00001\n$end\n").unwrap();
        });
        let mut t = TcpTransport::connect("127.0.0.1", port).unwrap();
        assert_eq!(t.recv_line(Duration::from_secs(2)).unwrap(), "$start,id");
        assert_eq!(
            t.recv_line(Duration::from_secs(2)).unwrap(),
            "$000000000001"
        );
        assert_eq!(t.recv_line(Duration::from_secs(2)).unwrap(), "$end");
    }

    #[test]
    fn recv_line_buffers_multiple_lines() {
        let port = spawn_server(|mut sock| {
            sock.write_all(b"$start,temp\n$47.3\n$end\n").unwrap();
            std::thread::sleep(Duration::from_millis(100));
        });
        let mut t = TcpTransport::connect("127.0.0.1", port).unwrap();
        assert_eq!(t.recv_line(Duration::from_secs(2)).unwrap(), "$start,temp");
        assert_eq!(t.recv_line(Duration::from_secs(2)).unwrap(), "$47.3");
        assert_eq!(t.recv_line(Duration::from_secs(2)).unwrap(), "$end");
    }

    #[test]
    fn recv_line_times_out_on_silence() {
        let port = spawn_server(|_sock| {
            std::thread::sleep(Duration::from_millis(500));
        });
        let mut t = TcpTransport::connect("127.0.0.1", port).unwrap();
        let start = std::time::Instant::now();
        let err = t.recv_line(Duration::from_millis(100)).unwrap_err();
        assert!(matches!(err, Error::Timeout));
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
