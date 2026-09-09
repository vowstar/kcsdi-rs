// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Packet and measurement-stream parsers (protocol doc sections 2 and 4).
//!
//! Both parsers are fed whole lines (see [`crate::transport::Transport`]) and
//! follow the KCSDI parsing semantics documented in the protocol reference:
//!
//! - [`PacketParser`] accumulates `$start,<name>` ... `$end` blocks, ignores
//!   garbage before `$start` on a line, and tolerates non-`$` noise lines.
//! - [`StreamParser`] is the `start`/`data`/`end` state machine for the five
//!   measurement modes `s21`, `s11`, `spec`, `s22`, `s12`.

use crate::error::{Error, Result};
use crate::transport::MAX_LINE_BYTES;

/// Upper bound on one packet, including its framing and line separators.
const MAX_PACKET_BYTES: usize = 1024 * 1024;
/// Bound per-row allocations even when a packet contains very short lines.
const MAX_PACKET_ROWS: usize = 16 * 1024;

fn start_header(line: &str) -> Option<&str> {
    let (_, header) = line.split_once("$start,")?;
    let name = header.split(',').next()?;
    if name.is_empty() || name.chars().any(char::is_whitespace) {
        return None;
    }
    Some(header)
}

fn is_end(line: &str) -> bool {
    line.trim_end().ends_with("$end")
}

/// A parsed `$start,<name>` ... `$end` packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// Full header name, e.g. `device` or `s11,ri`.
    pub name: String,
    /// Part before the first comma (`s11`), empty when the name has none.
    pub sort: String,
    /// Part after the first comma (`ri`), or the whole name when no comma.
    pub command: String,
    /// Body rows: `$` stripped, blank lines dropped, split at `,`.
    pub args: Vec<Vec<String>>,
}

impl Packet {
    fn build(header: &str, body: &[String]) -> Self {
        let (sort, command) = match header.split_once(',') {
            Some((s, c)) => (s.to_string(), c.to_string()),
            None => (String::new(), header.to_string()),
        };
        let args = body
            .iter()
            .map(|line| line.replace('$', ""))
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.split(',').map(str::to_string).collect())
            .collect();
        Self {
            name: header.to_string(),
            sort,
            command,
            args,
        }
    }

    /// True for `err_*` device error packets (protocol doc 5.1).
    pub fn is_error(&self) -> bool {
        self.name.starts_with("err_")
    }
}

/// Accumulates `$start` ... `$end` packets from a line stream.
///
/// A genuine `$start,<name>` resynchronizes even inside an incomplete packet.
/// Noise before framing is tolerated (protocol doc 2.3). Packet size and row
/// limits prevent malformed input from accumulating indefinitely.
#[derive(Debug, Default)]
pub struct PacketParser {
    header: Option<String>,
    body: Vec<String>,
    bytes: usize,
}

impl PacketParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one line. An end marker returns the completed packet. Oversized
    /// input resets the parser and returns a protocol error.
    pub fn feed_line(&mut self, line: &str) -> Result<Option<Packet>> {
        if line.len() > MAX_LINE_BYTES {
            return self.reject("packet line exceeds the size limit");
        }
        if let Some(header) = start_header(line) {
            self.header = Some(header.to_string());
            self.body.clear();
            self.bytes = line.len() + 1;
            return Ok(None);
        }
        if self.header.is_none() {
            return Ok(None);
        }
        self.bytes += line.len() + 1;
        if self.bytes > MAX_PACKET_BYTES {
            return self.reject("packet exceeds the size limit");
        }
        if is_end(line) {
            let header = self.header.take().unwrap_or_default();
            let packet = Packet::build(&header, &self.body);
            self.body.clear();
            self.bytes = 0;
            return Ok(Some(packet));
        }
        if self.body.len() >= MAX_PACKET_ROWS {
            return self.reject("packet exceeds the row limit");
        }
        self.body.push(line.to_string());
        Ok(None)
    }

    fn reject(&mut self, message: &str) -> Result<Option<Packet>> {
        self.header = None;
        self.body.clear();
        self.bytes = 0;
        Err(Error::Protocol(message.to_string()))
    }
}

/// Measurement stream modes (protocol doc 4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamMode {
    S21,
    S11,
    Spec,
    S22,
    S12,
}

impl StreamMode {
    /// Map a header mode field to a stream mode; `None` for anything else.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "s21" => Some(Self::S21),
            "s11" => Some(Self::S11),
            "spec" => Some(Self::Spec),
            "s22" => Some(Self::S22),
            "s12" => Some(Self::S12),
            _ => None,
        }
    }

    /// Wire name of the mode.
    pub fn name(self) -> &'static str {
        match self {
            Self::S21 => "s21",
            Self::S11 => "s11",
            Self::Spec => "spec",
            Self::S22 => "s22",
            Self::S12 => "s12",
        }
    }
}

/// Events emitted by [`StreamParser`].
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// `$start,<mode>,<format>` (format empty for `spec`).
    Start { mode: StreamMode, format: String },
    /// One data row, split at `,`; `fields[0]` is the frequency (with `$`).
    Data {
        mode: StreamMode,
        format: String,
        fields: Vec<String>,
    },
    /// `$end` closing the current stream.
    End { mode: StreamMode, format: String },
}

/// Measurement data stream state machine (protocol doc 4.1).
///
/// A `$start` line whose mode is not one of the five stream modes still
/// switches the machine to streaming (the block is skipped silently), so
/// interleaved info packets never leak into measurement consumers.
#[derive(Debug, Default)]
pub struct StreamParser {
    active: Option<(Option<StreamMode>, String)>,
}

impl StreamParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one line. Returns at most one event.
    pub fn feed_line(&mut self, line: &str) -> Option<StreamEvent> {
        if let Some(header) = start_header(line) {
            let mut parts = header.split(',');
            let mode = StreamMode::from_name(parts.next().unwrap_or(""));
            let format = parts.next().unwrap_or("").to_string();
            self.active = Some((mode, format.clone()));
            mode.map(|mode| StreamEvent::Start { mode, format })
        } else if self.active.is_none() {
            None
        } else if is_end(line) {
            let (mode, format) = self.active.take().expect("state checked above");
            mode.map(|mode| StreamEvent::End { mode, format })
        } else {
            let (mode, format) = self.active.as_ref().expect("state checked above");
            let fields = line.split(',').map(str::to_string).collect();
            mode.map(|mode| StreamEvent::Data {
                mode,
                format: format.clone(),
                fields,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_packet(parser: &mut PacketParser, lines: &[&str]) -> Packet {
        let mut out = None;
        for line in lines {
            if let Some(p) = parser.feed_line(line).unwrap() {
                out = Some(p);
            }
        }
        out.expect("expected a complete packet")
    }

    #[test]
    fn parses_verified_handshake_id_packet() {
        // Verified on KC901V firmware V1.6.1 (protocol doc 1.4).
        let mut p = PacketParser::new();
        let packet = collect_packet(&mut p, &["$start,id", "$000000000001", "$end"]);
        assert_eq!(packet.name, "id");
        assert_eq!(packet.sort, "");
        assert_eq!(packet.command, "id");
        assert_eq!(packet.args, vec![vec!["000000000001".to_string()]]);
    }

    #[test]
    fn resyncs_at_start_and_ignores_garbage() {
        let mut p = PacketParser::new();
        assert_eq!(p.feed_line("random noise").unwrap(), None);
        assert_eq!(p.feed_line("$47.3 outside any packet").unwrap(), None);
        let packet = collect_packet(
            &mut p,
            &["garbage prefix $start,temp", "$47.3", "junk $end"],
        );
        assert_eq!(packet.name, "temp");
        assert_eq!(packet.args, vec![vec!["47.3".to_string()]]);
    }

    #[test]
    fn packet_markers_require_real_delimiters() {
        let mut p = PacketParser::new();
        for line in ["$startled,temp", "$start", "$start,", "$start, temp"] {
            assert!(p.feed_line(line).unwrap().is_none(), "{line}");
            assert!(p.header.is_none(), "{line}");
        }
        let packet = collect_packet(
            &mut p,
            &["$start,temp", "$ending", "$end,extra", "$47.3", "$end"],
        );
        assert_eq!(packet.name, "temp");
        assert_eq!(
            packet.args,
            vec![vec!["ending"], vec!["end", "extra"], vec!["47.3"]]
        );
    }

    #[test]
    fn nested_start_discards_the_incomplete_packet() {
        let mut p = PacketParser::new();
        let packet = collect_packet(
            &mut p,
            &[
                "$start,s11,ri",
                "$5000,0.1,0.2",
                "noise $start,err_par5",
                "$end",
            ],
        );
        assert_eq!(packet.name, "err_par5");
        assert!(packet.is_error());
        assert!(packet.args.is_empty());
        assert!(p.header.is_none());
        assert_eq!(p.bytes, 0);
    }

    #[test]
    fn packet_overflow_resets_and_allows_a_fresh_packet() {
        let mut p = PacketParser::new();
        p.feed_line("$start,s11,ri").unwrap();
        let row = "0".repeat(MAX_LINE_BYTES);
        for _ in 0..(MAX_PACKET_BYTES / (MAX_LINE_BYTES + 1)) {
            assert!(p.feed_line(&row).unwrap().is_none());
        }
        assert!(matches!(p.feed_line(&row), Err(Error::Protocol(_))));
        assert!(p.header.is_none());
        assert!(p.body.is_empty());
        assert_eq!(p.bytes, 0);
        assert!(p.feed_line("$end").unwrap().is_none());
        let packet = collect_packet(&mut p, &["$start,temp", "$47.3", "$end"]);
        assert_eq!(packet.name, "temp");
        assert_eq!(packet.args, vec![vec!["47.3"]]);
    }

    #[test]
    fn packet_row_limit_bounds_short_line_allocations() {
        let mut p = PacketParser::new();
        p.feed_line("$start,s11,ri").unwrap();
        for _ in 0..MAX_PACKET_ROWS {
            assert!(p.feed_line("").unwrap().is_none());
        }
        assert!(p.bytes < MAX_PACKET_BYTES);
        assert!(matches!(p.feed_line(""), Err(Error::Protocol(_))));
        assert!(p.header.is_none());
        assert!(p.body.is_empty());
        assert_eq!(p.bytes, 0);
    }

    #[test]
    fn packet_accepts_exact_byte_limit_and_rejects_one_more_byte() {
        for extra_byte in [false, true] {
            let mut p = PacketParser::new();
            p.feed_line("$start,s11,ri").unwrap();
            let row = "0".repeat(MAX_LINE_BYTES);
            let end_bytes = "$end".len() + 1;
            while p.bytes + MAX_LINE_BYTES + 1 + end_bytes < MAX_PACKET_BYTES {
                p.feed_line(&row).unwrap();
            }
            let last_row_bytes = MAX_PACKET_BYTES - p.bytes - end_bytes;
            let last_row = "0".repeat(last_row_bytes - 1 + usize::from(extra_byte));
            p.feed_line(&last_row).unwrap();
            let result = p.feed_line("$end");
            if extra_byte {
                assert!(matches!(result, Err(Error::Protocol(_))));
            } else {
                assert_eq!(result.unwrap().unwrap().name, "s11,ri");
            }
        }
    }

    #[test]
    fn packet_rejects_oversized_header_and_body_lines() {
        for header in [false, true] {
            let mut p = PacketParser::new();
            p.feed_line("$start,s11,ri").unwrap();
            let prefix = if header { "$start," } else { "$" };
            let line = format!("{prefix}{}", "0".repeat(MAX_LINE_BYTES));
            assert!(matches!(p.feed_line(&line), Err(Error::Protocol(_))));
            assert!(p.header.is_none());
            assert!(p.body.is_empty());
            assert_eq!(p.bytes, 0);
            let packet = collect_packet(&mut p, &["$start,id", "$000000000001", "$end"]);
            assert_eq!(packet.name, "id");
        }
    }

    #[test]
    fn parses_device_packet_and_splits_body_rows() {
        // Manual 3.2.11 example packet.
        let mut p = PacketParser::new();
        let packet = collect_packet(
            &mut p,
            &[
                "$start,device",
                "$<----------------------KC901 network analyzer----------------->",
                "$<-User @ :BBBB567895>",
                "$<-Software ver:V2.0.8------------------------------------------>",
                "$<-Hardware ver:MB-V1.2 RB-V1.1--------------------------------->",
                "$<-Serial num:002015123456-------------------------------------->",
                "$<-Copyright:KeXinShe Co.,Ltd & KeChuang measurement association>",
                "$<-------------------------------------------------------------->",
                "$end",
            ],
        );
        assert_eq!(packet.name, "device");
        assert_eq!(packet.args.len(), 7);
        assert_eq!(packet.args[1][0], "<-User @ :BBBB567895>");
        // The copyright row contains commas and is split into several fields.
        assert_eq!(packet.args[5][0], "<-Copyright:KeXinShe Co.");
    }

    #[test]
    fn stream_header_name_split() {
        let mut p = PacketParser::new();
        let packet = collect_packet(&mut p, &["$start,s11,ri", "$1,2,3", "$end"]);
        assert_eq!(packet.name, "s11,ri");
        assert_eq!(packet.sort, "s11");
        assert_eq!(packet.command, "ri");
    }

    #[test]
    fn stream_parser_walks_manual_s11_example() {
        // Manual 3.3.1 worked example (protocol doc section 9).
        let lines = [
            "$start,s11,ri",
            "$75000000,0.528e0,-0.269e0",
            "$100000000,0.471e0,-0.406e0",
            "$125000000,0.370e0,-0.475e0",
            "$end",
        ];
        let mut p = StreamParser::new();
        let events: Vec<_> = lines.iter().filter_map(|l| p.feed_line(l)).collect();
        assert_eq!(
            events[0],
            StreamEvent::Start {
                mode: StreamMode::S11,
                format: "ri".to_string()
            }
        );
        assert_eq!(
            events[1],
            StreamEvent::Data {
                mode: StreamMode::S11,
                format: "ri".to_string(),
                fields: vec![
                    "$75000000".to_string(),
                    "0.528e0".to_string(),
                    "-0.269e0".to_string()
                ],
            }
        );
        assert_eq!(
            events[4],
            StreamEvent::End {
                mode: StreamMode::S11,
                format: "ri".to_string()
            }
        );
        assert_eq!(events.len(), 5);
    }

    #[test]
    fn stream_parser_spec_has_no_format_field() {
        let mut p = StreamParser::new();
        assert_eq!(
            p.feed_line("$start,spec"),
            Some(StreamEvent::Start {
                mode: StreamMode::Spec,
                format: String::new()
            })
        );
        assert_eq!(
            p.feed_line("$75000000,-74.166"),
            Some(StreamEvent::Data {
                mode: StreamMode::Spec,
                format: String::new(),
                fields: vec!["$75000000".to_string(), "-74.166".to_string()],
            })
        );
        assert_eq!(
            p.feed_line("$end"),
            Some(StreamEvent::End {
                mode: StreamMode::Spec,
                format: String::new()
            })
        );
    }

    #[test]
    fn stream_parser_skips_non_stream_packets() {
        // A $start whose mode is not a stream mode is swallowed silently.
        let mut p = StreamParser::new();
        assert_eq!(p.feed_line("$start,device"), None);
        assert_eq!(p.feed_line("$<-User @ :X>"), None);
        assert_eq!(p.feed_line("$end"), None);
        // Machine is back to IDLE afterwards.
        assert_eq!(
            p.feed_line("$start,s11,loss"),
            Some(StreamEvent::Start {
                mode: StreamMode::S11,
                format: "loss".to_string()
            })
        );
    }

    #[test]
    fn stream_parser_resynchronizes_at_a_nested_start() {
        let mut p = StreamParser::new();
        p.feed_line("$start,s11,ri");
        assert!(matches!(
            p.feed_line("noise $start,spec"),
            Some(StreamEvent::Start {
                mode: StreamMode::Spec,
                ..
            })
        ));
        assert!(matches!(
            p.feed_line("$5000,-80"),
            Some(StreamEvent::Data {
                mode: StreamMode::Spec,
                ..
            })
        ));
        assert!(matches!(
            p.feed_line("noise $end"),
            Some(StreamEvent::End {
                mode: StreamMode::Spec,
                ..
            })
        ));
    }

    #[test]
    fn interleaved_error_does_not_become_measurement_data() {
        let mut packets = PacketParser::new();
        let mut streams = StreamParser::new();
        for line in ["$start,s11,ri", "$5000,0.1,0.2"] {
            assert!(packets.feed_line(line).unwrap().is_none());
            assert!(streams.feed_line(line).is_some());
        }
        for line in ["noise $start,err_par5", "$invalid frequency"] {
            assert!(packets.feed_line(line).unwrap().is_none());
            assert!(streams.feed_line(line).is_none());
        }
        assert!(packets.feed_line("$end").unwrap().unwrap().is_error());
        assert!(streams.feed_line("$end").is_none());
        assert!(matches!(
            streams.feed_line("$start,s11,loss"),
            Some(StreamEvent::Start {
                mode: StreamMode::S11,
                ..
            })
        ));
    }

    #[test]
    fn stream_markers_require_real_delimiters() {
        let mut p = StreamParser::new();
        for line in ["$startled,s11,ri", "$start", "$start,", "$start, s11"] {
            assert!(p.feed_line(line).is_none(), "{line}");
        }
        p.feed_line("$start,s11,ri");
        for line in ["$ending", "$end,extra", "$startled,spec"] {
            assert!(
                matches!(p.feed_line(line), Some(StreamEvent::Data { .. })),
                "{line}"
            );
        }
        assert!(matches!(
            p.feed_line("$end"),
            Some(StreamEvent::End {
                mode: StreamMode::S11,
                ..
            })
        ));
    }

    #[test]
    fn stream_parser_ignores_lines_outside_streams() {
        let mut p = StreamParser::new();
        assert_eq!(p.feed_line("$75000000,0.5"), None);
        assert_eq!(p.feed_line("noise $startled,s11,ri"), None);
    }
}
