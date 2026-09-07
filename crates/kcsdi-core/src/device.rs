// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! High-level device session API.
//!
//! A [`Device`] wraps a [`Transport`] and implements the session flow from
//! the protocol reference: handshake with `C`, query identity and status
//! packets, run sweeps (`stop` -> `init` -> `run` -> consume the stream
//! until `end`), and exit remote mode with `$local` on close.

use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::commands::{self, Cal, Format, Lo, ScanMode};
use crate::data::{DeviceInfo, SweepData, SweepPoint, Voltage, parse_f64};
use crate::error::{Error, Result};
use crate::model::{self, Rbw};
use crate::protocol::{Packet, PacketParser, StreamEvent, StreamMode, StreamParser};
use crate::transport::{GENERIC_TIMEOUT, TcpTransport, Transport};

/// Pause between mode-control commands to pace the instrument's command
/// buffer (protocol doc 8.1 uses 100 ms gaps).
const COMMAND_GAP: Duration = Duration::from_millis(100);

/// Parameters of an S11 sweep.
#[derive(Debug, Clone)]
pub struct S11Params {
    pub cal: Cal,
    pub format: Format,
    pub points: u32,
    pub start_hz: u64,
    pub stop_hz: u64,
    /// When set, `$bw,<rbw>` is pushed before the run and the RBW factor is
    /// used for the sweep timeout; otherwise the 30k fallback factor is
    /// used for the timeout only.
    pub rbw: Option<Rbw>,
}

/// Parameters of a spectrum sweep.
#[derive(Debug, Clone)]
pub struct SpecParams {
    pub cal: Cal,
    pub lo: Lo,
    pub points: u32,
    pub start_hz: u64,
    pub stop_hz: u64,
    pub rbw: Rbw,
    pub ref_level_dbm: i32,
}

/// A remote-control session with a KC901 instrument.
pub struct Device<T: Transport> {
    transport: T,
    remote: bool,
    packets: PacketParser,
    streams: StreamParser,
}

impl Device<TcpTransport> {
    /// Connect over TCP and perform the handshake (send `C`, wait for the
    /// `id` packet). Returns the ready session.
    pub fn connect(host: &str, port: u16) -> Result<Self> {
        let transport = TcpTransport::connect(host, port)?;
        let mut device = Self::new(transport);
        device.handshake()?;
        Ok(device)
    }
}

impl<T: Transport> Device<T> {
    /// Wrap an already-connected transport. No handshake is performed; call
    /// [`Device::handshake`] explicitly.
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            remote: false,
            packets: PacketParser::new(),
            streams: StreamParser::new(),
        }
    }

    /// Send `C` and wait for the identity reply. Returns the serial number.
    ///
    /// Accepts both the verified `$start,id` packet form and the manual's
    /// `[KC901]<serial>` plain-text form (doc 1.4). A `ConFail` packet maps
    /// to [`Error::DeviceBusy`].
    pub fn handshake(&mut self) -> Result<String> {
        self.transport.send(commands::HANDSHAKE)?;
        loop {
            let line = self.transport.recv_line(GENERIC_TIMEOUT)?;
            if let Some(serial) = line.strip_prefix("[KC901]") {
                self.remote = true;
                return Ok(serial.trim().to_string());
            }
            if let Some(packet) = self.packets.feed_line(&line) {
                match packet.name.as_str() {
                    "id" => {
                        self.remote = true;
                        return Ok(packet
                            .args
                            .first()
                            .and_then(|row| row.first())
                            .cloned()
                            .unwrap_or_default());
                    }
                    "ConFail" => {
                        return Err(Error::DeviceBusy(
                            "front panel window open, exit it first".into(),
                        ));
                    }
                    name if name.starts_with("err_") => {
                        return Err(Error::Device(name.to_string()));
                    }
                    name => log::debug!("handshake: ignoring packet {name}"),
                }
            }
        }
    }

    /// `$device` -> parsed identity information.
    pub fn device_info(&mut self) -> Result<DeviceInfo> {
        self.transport.send(commands::DEVICE.as_bytes())?;
        DeviceInfo::from_packet(&self.expect_packet("device")?)
    }

    /// `$temp` -> internal temperature in deg C.
    pub fn temperature(&mut self) -> Result<f64> {
        self.transport.send(commands::TEMP.as_bytes())?;
        let packet = self.expect_packet("temp")?;
        let field = packet
            .args
            .first()
            .and_then(|row| row.first())
            .ok_or_else(|| Error::Protocol("temp packet: empty body".into()))?;
        parse_f64(field)
    }

    /// `$voltage` -> external and battery voltages.
    pub fn voltage(&mut self) -> Result<Voltage> {
        self.transport.send(commands::VOLTAGE.as_bytes())?;
        Voltage::from_packet(&self.expect_packet("voltage")?)
    }

    /// Run an S11 sweep: `stop` -> `init` -> optional `$bw` -> `run`, then
    /// consume the data stream until `$end`.
    pub fn sweep_s11(&mut self, params: &S11Params) -> Result<SweepData> {
        self.transport.send(commands::S11_STOP.as_bytes())?;
        sleep(COMMAND_GAP);
        self.transport.send(commands::S11_INIT.as_bytes())?;
        sleep(COMMAND_GAP);
        if let Some(rbw) = params.rbw {
            self.transport.send(commands::set_rbw(rbw).as_bytes())?;
        }
        let run = commands::s11_run(
            params.cal,
            params.format,
            params.points,
            ScanMode::StartStop,
            params.start_hz,
            Some(params.stop_hz),
        );
        self.transport.send(run.as_bytes())?;
        self.collect_stream(
            StreamMode::S11,
            model::sweep_timeout(params.rbw, params.points),
        )
    }

    /// Run a spectrum sweep: `stop` -> `init` -> `$bw` -> `$specref` ->
    /// `run`, then consume the data stream until `$end`.
    pub fn sweep_spec(&mut self, params: &SpecParams) -> Result<SweepData> {
        self.transport.send(commands::SPEC_STOP.as_bytes())?;
        sleep(COMMAND_GAP);
        self.transport.send(commands::SPEC_INIT.as_bytes())?;
        sleep(COMMAND_GAP);
        self.transport
            .send(commands::set_rbw(params.rbw).as_bytes())?;
        self.transport
            .send(commands::set_spec_ref(params.ref_level_dbm).as_bytes())?;
        let run = commands::spec_run(
            params.cal,
            params.lo,
            params.points,
            ScanMode::StartStop,
            params.start_hz,
            Some(params.stop_hz),
            None,
        );
        self.transport.send(run.as_bytes())?;
        self.collect_stream(
            StreamMode::Spec,
            model::sweep_timeout(Some(params.rbw), params.points),
        )
    }

    /// Exit remote mode (`$local`). Best effort; also called from `Drop`.
    pub fn close(&mut self) {
        if self.remote {
            let _ = self.transport.send(commands::LOCAL.as_bytes());
            self.remote = false;
        }
    }

    /// Read lines until the packet `name` completes. `err_*` packets abort
    /// with [`Error::Device`]; other packets are logged and skipped.
    fn expect_packet(&mut self, name: &str) -> Result<Packet> {
        loop {
            let line = self.transport.recv_line(GENERIC_TIMEOUT)?;
            if let Some(packet) = self.packets.feed_line(&line) {
                if packet.is_error() {
                    return Err(Error::Device(packet.name));
                }
                if packet.name == name {
                    return Ok(packet);
                }
                log::warn!("unexpected packet {}, waiting for {name}", packet.name);
            }
        }
    }

    /// Consume a measurement stream until `$end`, collecting data rows.
    ///
    /// The overall deadline is the sweep timeout formula (doc 8.2); each
    /// individual read is capped at the generic 10 s timeout, re-armed
    /// after every line, so long sweeps survive while data keeps flowing.
    fn collect_stream(&mut self, mode: StreamMode, timeout: Duration) -> Result<SweepData> {
        let deadline = Instant::now() + timeout;
        let mut format = String::new();
        let mut points = Vec::new();
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(Error::Timeout)?;
            let line = self.transport.recv_line(remaining.min(GENERIC_TIMEOUT))?;
            // Feed the packet parser too, so err_* packets interleaved with
            // a stream are still caught.
            if let Some(packet) = self.packets.feed_line(&line)
                && packet.is_error()
            {
                return Err(Error::Device(packet.name));
            }
            match self.streams.feed_line(&line) {
                Some(StreamEvent::Start { mode: m, format: f }) if m == mode => format = f,
                Some(StreamEvent::Data {
                    mode: m, fields, ..
                }) if m == mode => {
                    let freq = parse_f64(fields.first().map_or("", String::as_str))?;
                    let values = fields
                        .get(1..)
                        .unwrap_or(&[])
                        .iter()
                        .map(|f| parse_f64(f))
                        .collect::<Result<Vec<_>>>()?;
                    points.push(SweepPoint {
                        freq_hz: freq,
                        values,
                    });
                }
                Some(StreamEvent::End { mode: m, .. }) if m == mode => {
                    return Ok(SweepData {
                        mode,
                        format,
                        points,
                    });
                }
                _ => {}
            }
        }
    }
}

impl<T: Transport> Drop for Device<T> {
    fn drop(&mut self) {
        // Never leave the instrument locked in remote mode (doc 1.2).
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct MockTransport {
        incoming: VecDeque<String>,
        sent: Vec<u8>,
    }

    impl MockTransport {
        fn with_lines(lines: &[&str]) -> Self {
            Self {
                incoming: lines.iter().map(|s| s.to_string()).collect(),
                sent: Vec::new(),
            }
        }

        fn sent_text(&self) -> String {
            String::from_utf8_lossy(&self.sent).to_string()
        }
    }

    impl Transport for MockTransport {
        fn send(&mut self, data: &[u8]) -> Result<()> {
            self.sent.extend_from_slice(data);
            Ok(())
        }

        fn recv_line(&mut self, _timeout: Duration) -> Result<String> {
            self.incoming.pop_front().ok_or(Error::Timeout)
        }
    }

    #[test]
    fn handshake_accepts_verified_id_packet() {
        let mock = MockTransport::with_lines(&["$start,id", "$000000000001", "$end"]);
        let mut dev = Device::new(mock);
        assert_eq!(dev.handshake().unwrap(), "000000000001");
        assert_eq!(dev.transport.sent, b"C");
        dev.close();
        assert_eq!(dev.transport.sent_text(), "C$local\n");
    }

    #[test]
    fn handshake_tolerates_manual_kc901_form() {
        let mock = MockTransport::with_lines(&["[KC901]002015123456"]);
        let mut dev = Device::new(mock);
        assert_eq!(dev.handshake().unwrap(), "002015123456");
    }

    #[test]
    fn handshake_confail_maps_to_device_busy() {
        let mock = MockTransport::with_lines(&[
            "$start,ConFail",
            "$Please exit the window operation first.",
            "$end",
        ]);
        let mut dev = Device::new(mock);
        assert!(matches!(dev.handshake(), Err(Error::DeviceBusy(_))));
    }

    #[test]
    fn info_queries_parse_packets() {
        let mock = MockTransport::with_lines(&[
            "$start,device",
            "$<----------------------KC901 network analyzer----------------->",
            "$<-User @ :VOWSTAR>",
            "$<-Software ver:V1.6.1------------------------------------------>",
            "$<-Hardware ver:MB-V1.2 RB-V1.0.5------------------------------->",
            "$<-Serial num:000000000001-------------------------------------->",
            "$<-Copyright:KeXinShe Co.,Ltd & KeChuang measurement association>",
            "$<-------------------------------------------------------------->",
            "$end",
            "$start,temp",
            "$47.3",
            "$end",
            "$start,voltage",
            "$12.16,8.03",
            "$end",
        ]);
        let mut dev = Device::new(mock);
        let info = dev.device_info().unwrap();
        assert_eq!(info.software, "V1.6.1");
        assert_eq!(info.serial, "000000000001");
        assert_eq!(dev.temperature().unwrap(), 47.3);
        let v = dev.voltage().unwrap();
        assert_eq!((v.external, v.battery), (12.16, 8.03));
        assert_eq!(dev.transport.sent_text(), "$device\n$temp\n$voltage\n");
    }

    #[test]
    fn err_packet_aborts_query() {
        let mock =
            MockTransport::with_lines(&["$start,err_cmd", "$error:Command input error!", "$end"]);
        let mut dev = Device::new(mock);
        assert!(matches!(dev.temperature(), Err(Error::Device(ref n)) if n == "err_cmd"));
    }

    #[test]
    fn sweep_s11_consumes_manual_example_stream() {
        let mock = MockTransport::with_lines(&[
            "$start,s11,ri",
            "$75000000,0.528e0,-0.269e0",
            "$100000000,0.471e0,-0.406e0",
            "$125000000,0.370e0,-0.475e0",
            "$end",
        ]);
        let mut dev = Device::new(mock);
        let params = S11Params {
            cal: Cal::CalOff,
            format: Format::Ri,
            points: 3,
            start_hz: 75_000_000,
            stop_hz: 125_000_000,
            rbw: None,
        };
        let data = dev.sweep_s11(&params).unwrap();
        assert_eq!(data.mode, StreamMode::S11);
        assert_eq!(data.format, "ri");
        assert_eq!(data.points.len(), 3);
        assert_eq!(data.points[0].freq_hz, 75_000_000.0);
        assert_eq!(data.points[0].values, vec![0.528, -0.269]);
        assert_eq!(
            dev.transport.sent_text(),
            "$s11,stop\n$s11,init\n$s11,run,caloff,ri,3,ss,75000000,125000000\n"
        );
    }

    #[test]
    fn sweep_s11_with_rbw_pushes_bw_command() {
        let mock = MockTransport::with_lines(&["$start,s11,loss", "$1000000,-12.5", "$end"]);
        let mut dev = Device::new(mock);
        let params = S11Params {
            cal: Cal::CalSys,
            format: Format::Loss,
            points: 1,
            start_hz: 1_000_000,
            stop_hz: 1_000_000,
            rbw: Some(Rbw::R10k),
        };
        let data = dev.sweep_s11(&params).unwrap();
        assert_eq!(data.points[0].values, vec![-12.5]);
        assert!(dev.transport.sent_text().contains("$bw,10k\n"));
    }

    #[test]
    fn sweep_spec_sends_full_sequence() {
        let mock = MockTransport::with_lines(&[
            "$start,spec",
            "$75000000,-74.166",
            "$100000000,-71.002",
            "$end",
        ]);
        let mut dev = Device::new(mock);
        let params = SpecParams {
            cal: Cal::CalOff,
            lo: Lo::HighLo,
            points: 2,
            start_hz: 75_000_000,
            stop_hz: 100_000_000,
            rbw: Rbw::R10k,
            ref_level_dbm: -10,
        };
        let data = dev.sweep_spec(&params).unwrap();
        assert_eq!(data.mode, StreamMode::Spec);
        assert_eq!(data.format, "");
        assert_eq!(data.points[1].values, vec![-71.002]);
        assert_eq!(
            dev.transport.sent_text(),
            "$spec,stop\n$spec,init\n$bw,10k\n$specref,-10\n$spec,run,caloff,highlo,2,ss,75000000,100000000\n"
        );
    }

    #[test]
    fn sweep_aborts_on_err_uninit() {
        let mock = MockTransport::with_lines(&[
            "$start,err_uninit",
            "$error:Please initialize the mode first!",
            "$end",
        ]);
        let mut dev = Device::new(mock);
        let params = SpecParams {
            cal: Cal::CalOff,
            lo: Lo::HighLo,
            points: 10,
            start_hz: 75_000_000,
            stop_hz: 125_000_000,
            rbw: Rbw::R10k,
            ref_level_dbm: -10,
        };
        assert!(matches!(dev.sweep_spec(&params), Err(Error::Device(ref n)) if n == "err_uninit"));
    }
}
