// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Exclusive serial ownership with bounded host waits and line buffering.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serialport::{DataBits, FlowControl, Parity, StopBits};

use super::{
    CONNECT_TIMEOUT, Transport, is_timeout, remaining_timeout, take_buffered_line,
    write_with_deadline,
};
use crate::control::{CancellationToken, POLL_INTERVAL};
use crate::{Error, Result};

static OPEN_BUSY: AtomicBool = AtomicBool::new(false);
static ENUMERATION_BUSY: AtomicBool = AtomicBool::new(false);

struct NativeGuard(&'static AtomicBool);

impl Drop for NativeGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

// Field order keeps the gate held while a rejected or late handle is dropped.
struct NativeResult<T> {
    value: Option<Result<T>>,
    _guard: NativeGuard,
}

fn native_task<T: Send + 'static>(
    name: &'static str,
    busy: &'static AtomicBool,
    timeout: Duration,
    cancel: &CancellationToken,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    cancel.check()?;
    let started = Instant::now();
    remaining_timeout(started, timeout)?;
    busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{name} is still running"),
            )
        })?;
    let guard = NativeGuard(busy);
    let worker_cancel = cancel.clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let guard = guard;
            let value = (|| {
                worker_cancel.check()?;
                remaining_timeout(started, timeout)?;
                work()
            })();
            let _ = sender.send(NativeResult {
                value: Some(value),
                _guard: guard,
            });
        })?;
    loop {
        cancel.check()?;
        let remaining = remaining_timeout(started, timeout)?;
        let response = receiver.recv_timeout(remaining.min(POLL_INTERVAL));
        cancel.check()?;
        remaining_timeout(started, timeout)?;
        match response {
            Ok(mut response) => {
                return response.value.take().expect("native result owns its value");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other(format!("{name} terminated without a result")).into());
            }
        }
    }
}

/// A listed path and optional USB metadata. Listing does not establish availability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialPortInfo {
    pub path: String,
    pub label: String,
}

/// Read OS port metadata without opening a control connection.
/// A timed-out native enumeration remains the only pending enumeration.
pub fn available_ports_controlled(cancel: &CancellationToken) -> Result<Vec<SerialPortInfo>> {
    native_task(
        "serial-enumeration",
        &ENUMERATION_BUSY,
        CONNECT_TIMEOUT,
        cancel,
        || {
            let mut ports: Vec<_> = serialport::available_ports()
                .map_err(io::Error::from)?
                .into_iter()
                .map(port_info)
                .collect();
            ports.sort_by(|left, right| left.path.cmp(&right.path));
            ports.dedup_by(|left, right| left.path == right.path);
            Ok(ports)
        },
    )
}

fn port_info(info: serialport::SerialPortInfo) -> SerialPortInfo {
    let label = match info.port_type {
        serialport::SerialPortType::UsbPort(usb) => {
            let mut parts = vec![info.port_name.clone()];
            for text in [usb.manufacturer, usb.product, usb.serial_number]
                .into_iter()
                .flatten()
            {
                let text = text.trim();
                if !text.is_empty() && !parts.iter().any(|part| part == text) {
                    parts.push(text.to_owned());
                }
            }
            parts.join(" | ")
        }
        _ => info.port_name.clone(),
    };
    SerialPortInfo {
        path: info.port_name,
        label,
    }
}

trait SerialIo: Send {
    fn read(&mut self, data: &mut [u8], timeout: Duration) -> io::Result<usize>;
    fn write(&mut self, data: &[u8], timeout: Duration) -> io::Result<usize>;
}

struct NativePort(Box<dyn serialport::SerialPort>);

impl SerialIo for NativePort {
    fn read(&mut self, data: &mut [u8], timeout: Duration) -> io::Result<usize> {
        self.0.set_timeout(native_timeout(timeout))?;
        self.0.read(data)
    }

    fn write(&mut self, data: &[u8], timeout: Duration) -> io::Result<usize> {
        self.0.set_timeout(native_timeout(timeout))?;
        self.0.write(&data[..data.len().min(4096)])
    }
}

fn native_timeout(timeout: Duration) -> Duration {
    // Windows truncates Duration to milliseconds. A zero write timeout disables
    // its deadline, so positive sub-millisecond budgets must round upward.
    #[cfg(windows)]
    {
        rounded_milliseconds(timeout)
    }
    #[cfg(not(windows))]
    {
        timeout
    }
}

#[cfg(any(windows, test))]
fn rounded_milliseconds(timeout: Duration) -> Duration {
    Duration::from_millis(
        timeout
            .as_millis()
            .saturating_add(u128::from(
                !timeout.subsec_nanos().is_multiple_of(1_000_000),
            ))
            .clamp(1, u64::MAX as u128) as u64,
    )
}

fn builder(path: &str, baud_rate: u32) -> serialport::SerialPortBuilder {
    serialport::new(path, baud_rate)
        .data_bits(DataBits::Eight)
        .stop_bits(StopBits::One)
        .parity(Parity::None)
        .flow_control(FlowControl::None)
        .dtr_on_open(true)
        .timeout(POLL_INTERVAL)
}

fn open_native(path: &str, baud_rate: u32) -> Result<NativePort> {
    let options = builder(path, baud_rate);
    #[cfg(unix)]
    {
        use nix::fcntl::{FcntlArg, OFlag, fcntl};
        use std::os::fd::AsRawFd;

        let port = options.open_native().map_err(io::Error::from)?;
        let flags = fcntl(port.as_raw_fd(), FcntlArg::F_GETFL).map_err(io::Error::from)?;
        fcntl(
            port.as_raw_fd(),
            FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
        )
        .map_err(io::Error::from)?;
        Ok(NativePort(Box::new(port)))
    }
    #[cfg(not(unix))]
    {
        Ok(NativePort(options.open().map_err(io::Error::from)?))
    }
}

/// Serial byte stream. Only Device performs the KC901 handshake and LOCAL.
pub struct SerialTransport {
    port: Option<Box<dyn SerialIo>>,
    buf: Vec<u8>,
}

pub(crate) fn validate_path(path: &str) -> Result<()> {
    if path.trim().is_empty() || path.len() > 4096 || path.chars().any(char::is_control) {
        return Err(Error::InvalidParameter(
            "serial requires a port path of at most 4096 bytes without control characters".into(),
        ));
    }
    Ok(())
}

impl SerialTransport {
    /// Open with 8N1 and no flow control. The caller wait is bounded, while an
    /// unfinished native open remains single-flight and sends no protocol bytes.
    pub fn open_controlled(path: &str, baud_rate: u32, cancel: &CancellationToken) -> Result<Self> {
        cancel.check()?;
        validate_path(path)?;
        if baud_rate == 0 {
            return Err(Error::InvalidParameter(
                "serial baud rate must be nonzero".into(),
            ));
        }
        let path = path.to_owned();
        let port = native_task(
            "serial-open",
            &OPEN_BUSY,
            CONNECT_TIMEOUT,
            cancel,
            move || open_native(&path, baud_rate),
        )?;
        Ok(Self::from_port(port))
    }

    fn from_port(port: impl SerialIo + 'static) -> Self {
        Self {
            port: Some(Box::new(port)),
            buf: Vec::with_capacity(4096),
        }
    }

    fn invalidate(&mut self) {
        self.buf.clear();
        self.port = None;
    }

    fn receive(&mut self, timeout: Duration) -> Result<String> {
        if self.port.is_none() {
            return Err(Error::NotConnected);
        }
        let started = Instant::now();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(line) = take_buffered_line(&mut self.buf)? {
                return Ok(line);
            }
            let remaining = remaining_timeout(started, timeout)?;
            match self
                .port
                .as_mut()
                .ok_or(Error::NotConnected)?
                .read(&mut chunk, remaining)
            {
                // A serial read can finish without bytes. Preserve partial lines
                // and let the owning Device enforce its total response deadline.
                Ok(0) => return Err(Error::Timeout),
                Ok(count) => self.buf.extend_from_slice(&chunk[..count]),
                Err(error) if is_timeout(&error) => return Err(Error::Timeout),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl Transport for SerialTransport {
    fn send_with_timeout(&mut self, data: &[u8], timeout: Duration) -> Result<()> {
        let port = self.port.as_mut().ok_or(Error::NotConnected)?;
        let result =
            write_with_deadline(data, timeout, |data, remaining| port.write(data, remaining));
        if result.is_err() {
            self.invalidate();
        }
        result
    }

    fn recv_line(&mut self, timeout: Duration) -> Result<String> {
        let result = self.receive(timeout);
        if result
            .as_ref()
            .is_err_and(|error| !matches!(error, Error::Timeout))
        {
            self.invalidate();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    const TEST_WAIT: Duration = Duration::from_secs(3);

    #[derive(Default)]
    struct Calls {
        writes: Vec<Vec<u8>>,
        budgets: Vec<Duration>,
        drops: usize,
    }

    enum ReadStep {
        Data(Vec<u8>),
        Error(io::ErrorKind),
        Empty,
    }

    struct FakePort {
        calls: Arc<Mutex<Calls>>,
        reads: VecDeque<ReadStep>,
        write_limit: usize,
        write_delay: Duration,
        write_error: Option<io::ErrorKind>,
    }

    impl FakePort {
        fn new(reads: impl IntoIterator<Item = ReadStep>) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Calls::default())),
                reads: reads.into_iter().collect(),
                write_limit: usize::MAX,
                write_delay: Duration::ZERO,
                write_error: None,
            }
        }
    }

    impl Drop for FakePort {
        fn drop(&mut self) {
            self.calls.lock().unwrap().drops += 1;
        }
    }

    impl SerialIo for FakePort {
        fn read(&mut self, data: &mut [u8], timeout: Duration) -> io::Result<usize> {
            self.calls.lock().unwrap().budgets.push(timeout);
            match self
                .reads
                .pop_front()
                .unwrap_or(ReadStep::Error(io::ErrorKind::TimedOut))
            {
                ReadStep::Data(mut bytes) => {
                    let count = bytes.len().min(data.len());
                    data[..count].copy_from_slice(&bytes[..count]);
                    if count < bytes.len() {
                        self.reads
                            .push_front(ReadStep::Data(bytes.split_off(count)));
                    }
                    Ok(count)
                }
                ReadStep::Error(kind) => Err(kind.into()),
                ReadStep::Empty => Ok(0),
            }
        }

        fn write(&mut self, data: &[u8], timeout: Duration) -> io::Result<usize> {
            let mut calls = self.calls.lock().unwrap();
            calls.writes.push(data.to_vec());
            calls.budgets.push(timeout);
            drop(calls);
            if !self.write_delay.is_zero() {
                std::thread::sleep(self.write_delay);
            }
            if let Some(kind) = self.write_error.take() {
                return Err(kind.into());
            }
            Ok(data.len().min(self.write_limit))
        }
    }

    #[test]
    fn partial_lines_survive_timeout_zero_read_and_interruption() {
        let port = FakePort::new([
            ReadStep::Data(b"$sta".to_vec()),
            ReadStep::Error(io::ErrorKind::TimedOut),
            ReadStep::Empty,
            ReadStep::Error(io::ErrorKind::Interrupted),
            ReadStep::Data(b"rt,id\r\n$serial\n".to_vec()),
        ]);
        let mut transport = SerialTransport::from_port(port);
        assert!(matches!(
            transport.recv_line(TEST_WAIT),
            Err(Error::Timeout)
        ));
        assert!(matches!(
            transport.recv_line(TEST_WAIT),
            Err(Error::Timeout)
        ));
        assert_eq!(transport.recv_line(TEST_WAIT).unwrap(), "$start,id");
        assert_eq!(transport.recv_line(Duration::ZERO).unwrap(), "$serial");
    }

    #[test]
    fn oversized_lines_and_read_errors_drop_the_owner() {
        for step in [
            ReadStep::Data(vec![b'x'; super::super::MAX_LINE_BYTES + 1]),
            ReadStep::Error(io::ErrorKind::BrokenPipe),
        ] {
            let port = FakePort::new([step]);
            let calls = port.calls.clone();
            let mut transport = SerialTransport::from_port(port);
            assert!(transport.recv_line(TEST_WAIT).is_err());
            assert_eq!(calls.lock().unwrap().drops, 1);
            assert!(matches!(transport.send(b"C"), Err(Error::NotConnected)));
            assert!(matches!(
                transport.recv_line(TEST_WAIT),
                Err(Error::NotConnected)
            ));
        }
    }

    #[test]
    fn partial_writes_only_send_the_remaining_suffix() {
        let mut port = FakePort::new([]);
        port.write_limit = 2;
        port.write_error = Some(io::ErrorKind::Interrupted);
        let calls = port.calls.clone();
        let mut transport = SerialTransport::from_port(port);
        transport.send_with_timeout(b"$local\n", TEST_WAIT).unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.writes,
            [
                b"$local\n".to_vec(),
                b"$local\n".to_vec(),
                b"ocal\n".to_vec(),
                b"al\n".to_vec(),
                b"\n".to_vec()
            ]
        );
        assert!(calls.budgets.windows(2).all(|pair| pair[1] <= pair[0]));
    }

    #[test]
    fn stalled_failed_or_late_final_writes_cannot_be_retried() {
        for case in 0..4 {
            let mut port = FakePort::new([]);
            match case {
                0 => port.write_limit = 0,
                1 => port.write_error = Some(io::ErrorKind::TimedOut),
                2 => port.write_error = Some(io::ErrorKind::BrokenPipe),
                _ => port.write_delay = Duration::from_millis(30),
            }
            let calls = port.calls.clone();
            let mut transport = SerialTransport::from_port(port);
            assert!(
                transport
                    .send_with_timeout(b"C", Duration::from_millis(10))
                    .is_err()
            );
            assert_eq!(calls.lock().unwrap().drops, 1);
            assert!(matches!(transport.send(b"C"), Err(Error::NotConnected)));
            assert_eq!(calls.lock().unwrap().writes.len(), 1);
        }
    }

    #[test]
    fn expired_send_performs_no_backend_io() {
        let port = FakePort::new([]);
        let calls = port.calls.clone();
        let mut transport = SerialTransport::from_port(port);
        assert!(matches!(
            transport.send_with_timeout(b"C", Duration::ZERO),
            Err(Error::Timeout)
        ));
        assert!(calls.lock().unwrap().writes.is_empty());
    }

    #[test]
    fn native_windows_timeouts_cannot_be_disabled_by_rounding() {
        for (input, expected) in [
            (Duration::ZERO, 1),
            (Duration::from_nanos(1), 1),
            (Duration::from_micros(999), 1),
            (Duration::from_millis(1), 1),
            (Duration::from_micros(1001), 2),
            (Duration::from_secs(10), 10000),
        ] {
            assert_eq!(rounded_milliseconds(input), Duration::from_millis(expected));
        }
        assert_eq!(
            rounded_milliseconds(Duration::MAX),
            Duration::from_millis(u64::MAX)
        );
    }

    #[test]
    fn configuration_and_metadata_are_explicit_without_enumeration() {
        let expected = serialport::new("fixture", 921_600)
            .data_bits(DataBits::Eight)
            .stop_bits(StopBits::One)
            .parity(Parity::None)
            .flow_control(FlowControl::None)
            .dtr_on_open(true)
            .timeout(POLL_INTERVAL);
        assert_eq!(builder("fixture", 921_600), expected);
        let info = port_info(serialport::SerialPortInfo {
            port_name: "fixture".into(),
            port_type: serialport::SerialPortType::UsbPort(serialport::UsbPortInfo {
                vid: 0,
                pid: 0,
                manufacturer: Some(" Vendor ".into()),
                product: Some("Instrument".into()),
                serial_number: Some("123".into()),
            }),
        });
        assert_eq!(info.path, "fixture");
        assert_eq!(info.label, "fixture | Vendor | Instrument | 123");
        assert_eq!(
            port_info(serialport::SerialPortInfo {
                port_name: "missing".into(),
                port_type: serialport::SerialPortType::Unknown
            })
            .label,
            "missing"
        );
    }

    #[test]
    fn invalid_or_cancelled_public_requests_do_not_open_or_enumerate() {
        let cancelled = CancellationToken::default();
        cancelled.cancel();
        assert!(matches!(
            available_ports_controlled(&cancelled),
            Err(Error::Cancelled)
        ));
        assert!(matches!(
            SerialTransport::open_controlled("fixture", 921_600, &cancelled),
            Err(Error::Cancelled)
        ));
        for path in [
            "",
            "   ",
            "bad\0path",
            "bad\npath",
            "bad\rpath",
            "bad\tpath",
            "bad\u{7f}path",
            "bad\u{85}path",
        ] {
            assert!(matches!(
                SerialTransport::open_controlled(path, 921_600, &CancellationToken::default()),
                Err(Error::InvalidParameter(_))
            ));
        }
        assert!(matches!(
            SerialTransport::open_controlled("fixture", 0, &CancellationToken::default()),
            Err(Error::InvalidParameter(_))
        ));
        assert!(matches!(
            SerialTransport::open_controlled(
                &"x".repeat(4097),
                921_600,
                &CancellationToken::default()
            ),
            Err(Error::InvalidParameter(_))
        ));
        assert!(validate_path(&"x".repeat(4096)).is_ok());
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let started = Instant::now();
        while !condition() {
            assert!(started.elapsed() < TEST_WAIT);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn native_tasks_release_their_gate_after_success_error_and_panic() {
        static BUSY: AtomicBool = AtomicBool::new(false);
        let cancel = CancellationToken::default();
        assert_eq!(
            native_task("fixture", &BUSY, TEST_WAIT, &cancel, || Ok(17)).unwrap(),
            17
        );
        assert!(!BUSY.load(Ordering::Acquire));
        assert!(matches!(
            native_task::<()>("fixture", &BUSY, TEST_WAIT, &cancel, || Err(
                io::Error::other("injected").into()
            )),
            Err(Error::Io(_))
        ));
        assert!(!BUSY.load(Ordering::Acquire));
        assert!(matches!(
            native_task::<()>("fixture", &BUSY, TEST_WAIT, &cancel, || panic!(
                "injected helper panic"
            )),
            Err(Error::Io(_))
        ));
        wait_until(|| !BUSY.load(Ordering::Acquire));
        cancel.cancel();
        assert!(matches!(
            native_task::<()>("fixture", &BUSY, TEST_WAIT, &cancel, || panic!(
                "must not run"
            )),
            Err(Error::Cancelled)
        ));
        assert!(!BUSY.load(Ordering::Acquire));
    }

    #[test]
    fn timed_out_native_task_keeps_one_owner_until_late_handle_disposal() {
        static BUSY: AtomicBool = AtomicBool::new(false);
        struct LateHandle(mpsc::SyncSender<()>, mpsc::Receiver<()>);
        impl Drop for LateHandle {
            fn drop(&mut self) {
                self.0.send(()).unwrap();
                self.1.recv_timeout(TEST_WAIT).unwrap();
            }
        }
        let (release, wait) = mpsc::sync_channel(1);
        let (entered, ready) = mpsc::sync_channel(1);
        let (dropping, drop_started) = mpsc::sync_channel(1);
        let (finish_drop, drop_wait) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            native_task(
                "fixture",
                &BUSY,
                Duration::from_millis(100),
                &CancellationToken::default(),
                move || {
                    entered.send(()).unwrap();
                    wait.recv_timeout(TEST_WAIT).unwrap();
                    Ok(LateHandle(dropping, drop_wait))
                },
            )
            .map(|_| ())
        });
        ready.recv_timeout(TEST_WAIT).unwrap();
        assert!(matches!(worker.join().unwrap(), Err(Error::Timeout)));
        assert!(
            matches!(native_task("fixture", &BUSY, TEST_WAIT, &CancellationToken::default(), || Ok(())), Err(Error::Io(error)) if error.kind() == io::ErrorKind::WouldBlock)
        );
        release.send(()).unwrap();
        drop_started.recv_timeout(TEST_WAIT).unwrap();
        assert!(BUSY.load(Ordering::Acquire));
        finish_drop.send(()).unwrap();
        wait_until(|| !BUSY.load(Ordering::Acquire));
    }

    #[test]
    fn cancellation_interrupts_a_native_wait_and_drops_the_late_result() {
        static BUSY: AtomicBool = AtomicBool::new(false);
        let cancel = CancellationToken::default();
        let worker_cancel = cancel.clone();
        let (entered, ready) = mpsc::sync_channel(1);
        let (release, wait) = mpsc::sync_channel(1);
        let port = FakePort::new([]);
        let calls = port.calls.clone();
        let worker = std::thread::spawn(move || {
            native_task("fixture", &BUSY, TEST_WAIT, &worker_cancel, move || {
                entered.send(()).unwrap();
                wait.recv_timeout(TEST_WAIT).unwrap();
                Ok(port)
            })
            .map(|_| ())
        });
        ready.recv_timeout(TEST_WAIT).unwrap();
        cancel.cancel();
        assert!(matches!(worker.join().unwrap(), Err(Error::Cancelled)));
        assert!(BUSY.load(Ordering::Acquire));
        release.send(()).unwrap();
        wait_until(|| !BUSY.load(Ordering::Acquire));
        assert_eq!(calls.lock().unwrap().drops, 1);
        assert!(calls.lock().unwrap().writes.is_empty());
    }

    #[cfg(unix)]
    fn pty() -> (serialport::TTYPort, SerialTransport, String) {
        use serialport::SerialPort;
        // Some Unix backends obtain the slave name through shared storage.
        static ALLOCATION: Mutex<()> = Mutex::new(());
        let (mut peer, slave) = {
            let _allocation = ALLOCATION.lock().unwrap();
            serialport::TTYPort::pair().unwrap()
        };
        peer.set_timeout(TEST_WAIT).unwrap();
        let path = slave.name().unwrap();
        drop(slave);
        let port = open_native(&path, pty_baud()).unwrap();
        (peer, SerialTransport::from_port(port), path)
    }

    #[cfg(unix)]
    fn pty_baud() -> u32 {
        if cfg!(target_os = "macos") {
            0
        } else {
            921_600
        }
    }

    #[cfg(unix)]
    #[test]
    fn pty_exclusive_owner_can_reopen_after_drop_without_sending_bytes() {
        use serialport::SerialPort;
        let (mut peer, transport, path) = pty();
        assert!(open_native(&path, pty_baud()).is_err());
        peer.set_timeout(Duration::from_millis(30)).unwrap();
        assert!(peer.read(&mut [0u8; 1]).is_err());
        drop(transport);
        drop(open_native(&path, pty_baud()).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn pty_fragmented_handshake_and_local_share_the_device_lifecycle() {
        let (mut peer, transport, _) = pty();
        let (release, wait) = mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let mut byte = [0];
            peer.read_exact(&mut byte).unwrap();
            assert_eq!(&byte, b"C");
            peer.write_all(b"$start,i").unwrap();
            std::thread::sleep(Duration::from_millis(130));
            peer.write_all(b"d\r\n$PTY_FIXTURE\n$end\n").unwrap();
            let mut local = [0; 7];
            peer.read_exact(&mut local).unwrap();
            assert_eq!(&local, b"$local\n");
            wait.recv_timeout(TEST_WAIT).unwrap();
        });
        let mut device = crate::device::Device::new(transport);
        assert_eq!(device.handshake().unwrap(), "PTY_FIXTURE");
        device.close();
        release.send(()).unwrap();
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn pty_missing_newline_survives_polls_and_peer_close_retires() {
        let (mut peer, mut transport, _) = pty();
        peer.write_all(b"$partial").unwrap();
        assert!(matches!(
            transport.recv_line(Duration::from_millis(30)),
            Err(Error::Timeout)
        ));
        peer.write_all(b"\n").unwrap();
        assert_eq!(transport.recv_line(TEST_WAIT).unwrap(), "$partial");
        drop(peer);
        assert!(transport.recv_line(TEST_WAIT).is_err());
        assert!(matches!(transport.send(b"C"), Err(Error::NotConnected)));
    }

    #[cfg(unix)]
    #[test]
    fn pty_backpressure_has_a_total_deadline_and_retires_the_writer() {
        let (_peer, mut transport, _) = pty();
        let started = Instant::now();
        assert!(
            transport
                .send_with_timeout(&vec![b'x'; 1024 * 1024], Duration::from_millis(80))
                .is_err()
        );
        assert!(started.elapsed() < TEST_WAIT);
        assert!(matches!(transport.send(b"C"), Err(Error::NotConnected)));
    }

    #[cfg(unix)]
    #[test]
    fn pty_partial_handshake_cancellation_still_attempts_local() {
        let (mut peer, transport, _) = pty();
        let cancel = CancellationToken::default();
        let server_cancel = cancel.clone();
        let (release, wait) = mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let mut byte = [0];
            peer.read_exact(&mut byte).unwrap();
            assert_eq!(&byte, b"C");
            peer.write_all(b"$start,id\n$PTY").unwrap();
            server_cancel.cancel();
            let mut local = [0; 7];
            peer.read_exact(&mut local).unwrap();
            assert_eq!(&local, b"$local\n");
            wait.recv_timeout(TEST_WAIT).unwrap();
        });
        let mut device = crate::device::Device::new(transport);
        assert!(matches!(
            device.handshake_controlled(&cancel),
            Err(Error::Cancelled)
        ));
        device.close();
        release.send(()).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn missing_path_fails_without_creating_a_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing-serial-port");
        assert!(open_native(path.to_str().unwrap(), 921_600).is_err());
        assert!(!path.exists());
    }
}
