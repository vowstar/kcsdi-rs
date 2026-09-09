// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Blocking line-oriented transports.
//!
//! The KC901 byte stream is `\n` framed; a transport only needs to send raw
//! bytes and deliver whole lines. Partial lines are buffered internally so
//! callers always get complete lines without the trailing `\n`.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// TCP connect timeout (protocol doc 1.2: 5 s suggested).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Generic command/response timeout (protocol doc 8.1: 10000 ms).
pub const GENERIC_TIMEOUT: Duration = Duration::from_secs(10);

/// Host safety limit for raw line bytes before LF, including an optional CR.
/// This is not an instrument command or measurement limit.
pub const MAX_LINE_BYTES: usize = 16 * 1024;

/// Byte transport for the KC901 text protocol.
pub trait Transport {
    /// Send raw bytes (usually one command line including `\n`).
    fn send(&mut self, data: &[u8]) -> Result<()>;

    /// Receive one line. The returned string has no trailing `\n` (a stray
    /// `\r` is also stripped). Returns [`Error::Timeout`] when no newline
    /// arrives within the total `timeout`. Incomplete bytes survive timeouts.
    fn recv_line(&mut self, timeout: Duration) -> Result<String>;
}

/// Blocking TCP transport for network-equipped models (KC901V, ...).
pub struct TcpTransport {
    stream: TcpStream,
    buf: Vec<u8>,
    failed: bool,
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
                        failed: false,
                    });
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(Error::Io(last_err.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no address resolved")
        })))
    }

    fn invalidate(&mut self) {
        self.failed = true;
        self.buf.clear();
        let _ = self.stream.shutdown(Shutdown::Both);
    }

    fn send_with_timeout(&mut self, data: &[u8], timeout: Duration) -> Result<()> {
        if self.failed {
            return Err(Error::NotConnected);
        }
        let result = write_with_deadline(data, timeout, |remaining_data, remaining_time| {
            self.stream.set_write_timeout(Some(remaining_time))?;
            self.stream.write(remaining_data)
        });
        if result.is_err() {
            // A prefix may already be on the wire. Retrying the complete
            // command on this stream could concatenate two commands.
            self.invalidate();
        }
        result
    }
}

impl Transport for TcpTransport {
    fn send(&mut self, data: &[u8]) -> Result<()> {
        self.send_with_timeout(data, GENERIC_TIMEOUT)
    }

    fn recv_line(&mut self, timeout: Duration) -> Result<String> {
        if self.failed {
            return Err(Error::NotConnected);
        }
        let started = Instant::now();
        let mut chunk = [0u8; 4096];
        loop {
            let end = self.buf.iter().position(|&b| b == b'\n');
            if end.unwrap_or(self.buf.len()) > MAX_LINE_BYTES {
                self.invalidate();
                return Err(Error::Protocol(format!(
                    "line exceeds {MAX_LINE_BYTES} bytes"
                )));
            }
            if let Some(pos) = end {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                let text = String::from_utf8_lossy(&line);
                return Ok(text.trim_end_matches(['\n', '\r']).to_string());
            }
            let remaining = remaining_timeout(started, timeout)?;
            if let Err(error) = self.stream.set_read_timeout(Some(remaining)) {
                self.invalidate();
                return Err(error.into());
            }
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    self.invalidate();
                    return Err(Error::NotConnected);
                }
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if is_timeout(&e) => return Err(Error::Timeout),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.invalidate();
                    return Err(Error::Io(e));
                }
            }
        }
    }
}

pub(crate) fn remaining_timeout(started: Instant, timeout: Duration) -> Result<Duration> {
    timeout
        .checked_sub(started.elapsed())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(Error::Timeout)
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

fn write_with_deadline(
    mut data: &[u8],
    timeout: Duration,
    mut write: impl FnMut(&[u8], Duration) -> io::Result<usize>,
) -> Result<()> {
    let started = Instant::now();
    while !data.is_empty() {
        let remaining = remaining_timeout(started, timeout)?;
        match write(data, remaining) {
            Ok(0) => {
                return Err(
                    io::Error::new(io::ErrorKind::WriteZero, "command write stalled").into(),
                );
            }
            Ok(n) => data = &data[n..],
            Err(e) if is_timeout(&e) => return Err(Error::Timeout),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn spawn_server<F: FnOnce(TcpStream) + Send + 'static>(
        f: F,
    ) -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let thread = std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            f(sock);
        });
        (port, thread)
    }

    #[test]
    fn recv_line_reassembles_fragmented_writes() {
        let (port, server) = spawn_server(|mut sock| {
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
        server.join().unwrap();
    }

    #[test]
    fn recv_line_buffers_multiple_lines() {
        let (port, server) = spawn_server(|mut sock| {
            sock.write_all(b"$start,temp\n$47.3\n$end\n").unwrap();
            std::thread::sleep(Duration::from_millis(100));
        });
        let mut t = TcpTransport::connect("127.0.0.1", port).unwrap();
        assert_eq!(t.recv_line(Duration::from_secs(2)).unwrap(), "$start,temp");
        assert_eq!(t.recv_line(Duration::from_secs(2)).unwrap(), "$47.3");
        assert_eq!(t.recv_line(Duration::from_secs(2)).unwrap(), "$end");
        server.join().unwrap();
    }

    #[test]
    fn recv_line_times_out_on_silence() {
        let (port, server) = spawn_server(|_sock| {
            std::thread::sleep(Duration::from_millis(500));
        });
        let mut t = TcpTransport::connect("127.0.0.1", port).unwrap();
        let start = std::time::Instant::now();
        let err = t.recv_line(Duration::from_millis(100)).unwrap_err();
        assert!(matches!(err, Error::Timeout));
        assert!(start.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
    }

    #[test]
    fn fragments_do_not_restart_the_line_deadline() {
        let (release, wait) = std::sync::mpsc::channel();
        let (ready, started) = std::sync::mpsc::channel();
        let (port, server) = spawn_server(move |mut sock| {
            sock.write_all(b"x").unwrap();
            ready.send(()).unwrap();
            let began = Instant::now();
            while matches!(wait.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty))
                && began.elapsed() < Duration::from_secs(10)
            {
                if sock.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let mut t = TcpTransport::connect("127.0.0.1", port).unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let began = Instant::now();
        let result = t.recv_line(Duration::from_millis(100));
        let elapsed = began.elapsed();
        let _ = release.send(());
        drop(t);
        server.join().unwrap();
        assert!(matches!(result, Err(Error::Timeout)));
        assert!(elapsed < Duration::from_secs(2));
    }

    #[test]
    fn polling_timeouts_preserve_incomplete_bytes() {
        let (release, wait) = std::sync::mpsc::channel();
        let (ready, started) = std::sync::mpsc::channel();
        let (port, server) = spawn_server(move |mut sock| {
            sock.write_all(b"$sta").unwrap();
            ready.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
            sock.write_all(b"rt,temp\n").unwrap();
        });
        let mut t = TcpTransport::connect("127.0.0.1", port).unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let result = t.recv_line(Duration::from_millis(50));
        release.send(()).unwrap();
        assert!(matches!(result, Err(Error::Timeout)));
        assert_eq!(t.buf, b"$sta");
        assert_eq!(t.recv_line(Duration::from_secs(2)).unwrap(), "$start,temp");
        server.join().unwrap();
    }

    #[test]
    fn zero_timeout_only_consumes_complete_buffered_lines() {
        let (release, wait) = std::sync::mpsc::channel();
        let (port, server) = spawn_server(move |_sock| {
            let _ = wait.recv_timeout(Duration::from_secs(2));
        });
        let mut t = TcpTransport::connect("127.0.0.1", port).unwrap();
        assert!(matches!(t.recv_line(Duration::ZERO), Err(Error::Timeout)));
        t.buf.extend_from_slice(b"$end\npartial");
        assert_eq!(t.recv_line(Duration::ZERO).unwrap(), "$end");
        assert!(matches!(t.recv_line(Duration::ZERO), Err(Error::Timeout)));
        assert_eq!(t.buf, b"partial");
        release.send(()).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn line_limit_is_per_raw_line_and_includes_carriage_return() {
        let (port, server) = spawn_server(|mut sock| {
            let mut bytes = vec![b'x'; MAX_LINE_BYTES - 1];
            bytes.extend_from_slice(b"\r\n");
            for _ in 0..MAX_LINE_BYTES {
                bytes.extend_from_slice(b"y\n");
            }
            sock.write_all(&bytes).unwrap();
        });
        let mut t = TcpTransport::connect("127.0.0.1", port).unwrap();
        assert_eq!(
            t.recv_line(Duration::from_secs(2)).unwrap(),
            "x".repeat(MAX_LINE_BYTES - 1)
        );
        for _ in 0..MAX_LINE_BYTES {
            assert_eq!(t.recv_line(Duration::from_secs(2)).unwrap(), "y");
        }
        server.join().unwrap();
    }

    #[test]
    fn overlong_lines_invalidate_the_connection_without_parsing_the_suffix() {
        for suffix in [b"".as_slice(), b"\n$start,temp\n$47\n$end\n"] {
            let suffix = suffix.to_vec();
            let (port, server) = spawn_server(move |mut sock| {
                let mut bytes = vec![b'x'; MAX_LINE_BYTES + 1];
                bytes.extend_from_slice(&suffix);
                let _ = sock.write_all(&bytes);
            });
            let mut t = TcpTransport::connect("127.0.0.1", port).unwrap();
            assert!(matches!(
                t.recv_line(Duration::from_secs(2)),
                Err(Error::Protocol(_))
            ));
            assert!(t.buf.is_empty());
            assert!(matches!(t.send(b"C"), Err(Error::NotConnected)));
            assert!(matches!(
                t.recv_line(Duration::ZERO),
                Err(Error::NotConnected)
            ));
            server.join().unwrap();
        }
    }

    #[test]
    fn eof_does_not_promote_an_incomplete_line() {
        let (port, server) = spawn_server(|mut sock| {
            sock.write_all(b"complete\npartial").unwrap();
        });
        let mut t = TcpTransport::connect("127.0.0.1", port).unwrap();
        assert_eq!(t.recv_line(Duration::from_secs(2)).unwrap(), "complete");
        assert!(matches!(
            t.recv_line(Duration::from_secs(2)),
            Err(Error::NotConnected)
        ));
        assert!(t.buf.is_empty());
        assert!(matches!(t.send(b"C"), Err(Error::NotConnected)));
        server.join().unwrap();
    }

    #[test]
    fn command_write_handles_short_writes_and_interrupts() {
        let mut sent = Vec::new();
        let mut calls = 0;
        let mut previous = Duration::from_secs(2);
        write_with_deadline(b"$local\n", previous, |data, remaining| {
            assert!(remaining <= previous);
            previous = remaining;
            calls += 1;
            if calls == 1 {
                return Err(io::ErrorKind::Interrupted.into());
            }
            sent.push(data[0]);
            Ok(1)
        })
        .unwrap();
        assert_eq!(sent, b"$local\n");
    }

    #[test]
    fn short_writes_do_not_restart_the_command_deadline() {
        let mut calls = 0;
        let result = write_with_deadline(&[b'x'; 100], Duration::from_millis(50), |_, _| {
            calls += 1;
            std::thread::sleep(Duration::from_millis(20));
            Ok(1)
        });
        assert!(matches!(result, Err(Error::Timeout)));
        assert!(calls < 100);
    }

    #[test]
    fn command_write_stops_on_zero_progress_or_timeout() {
        let result = write_with_deadline(b"C", Duration::from_secs(2), |_, _| Ok(0));
        assert!(matches!(result, Err(Error::Io(e)) if e.kind() == io::ErrorKind::WriteZero));
        for kind in [io::ErrorKind::TimedOut, io::ErrorKind::WouldBlock] {
            let result = write_with_deadline(b"C", Duration::from_secs(2), |_, _| Err(kind.into()));
            assert!(matches!(result, Err(Error::Timeout)));
        }
        assert!(matches!(
            write_with_deadline(b"C", Duration::ZERO, |_, _| unreachable!()),
            Err(Error::Timeout)
        ));
        assert!(write_with_deadline(b"", Duration::ZERO, |_, _| unreachable!()).is_ok());
        assert!(remaining_timeout(Instant::now(), Duration::MAX).is_ok());
    }

    #[test]
    fn stalled_command_write_invalidates_the_stream() {
        let bytes = vec![b'x'; 1024 * 1024];
        let (release, wait) = std::sync::mpsc::channel();
        let (ready, started) = std::sync::mpsc::channel();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        socket2::SockRef::from(&listener)
            .set_recv_buffer_size(4096)
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let socket = socket2::SockRef::from(&sock);
            socket.set_recv_buffer_size(4096).unwrap();
            ready.send(socket.recv_buffer_size().unwrap()).unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(10));
        });
        let mut t = TcpTransport::connect("127.0.0.1", port).unwrap();
        let socket = socket2::SockRef::from(&t.stream);
        socket.set_send_buffer_size(4096).unwrap();
        let send_buffer = socket.send_buffer_size().unwrap();
        let receive_buffer = started.recv_timeout(Duration::from_secs(2)).unwrap();
        let began = Instant::now();
        let result = t.send_with_timeout(&bytes, Duration::from_millis(100));
        let elapsed = began.elapsed();
        let _ = release.send(());
        server.join().unwrap();
        assert!(
            matches!(result, Err(Error::Timeout)),
            "{result:?}, send buffer {send_buffer}, receive buffer {receive_buffer}"
        );
        assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
        assert!(matches!(t.send(b"C"), Err(Error::NotConnected)));
        assert!(matches!(
            t.recv_line(Duration::ZERO),
            Err(Error::NotConnected)
        ));
    }
}
