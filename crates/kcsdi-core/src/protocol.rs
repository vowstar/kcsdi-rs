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
/// Resyncs at the first `$start` found anywhere on a line; anything before a
/// packet or after `$end` is ignored (protocol doc 2.3).
#[derive(Debug, Default)]
pub struct PacketParser {
    header: Option<String>,
    body: Vec<String>,
}

impl PacketParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one line. Returns the completed packet when a `$end` line closes
    /// the current block, `None` otherwise.
    pub fn feed_line(&mut self, line: &str) -> Option<Packet> {
        if self.header.is_none() {
            let idx = line.find("$start")?;
            let rest = &line[idx + "$start".len()..];
            let name = rest.strip_prefix(',').unwrap_or(rest);
            self.header = Some(name.to_string());
            self.body.clear();
            None
        } else if line.contains("$end") {
            let header = self.header.take().unwrap_or_default();
            let packet = Packet::build(&header, &self.body);
            self.body.clear();
            Some(packet)
        } else {
            self.body.push(line.to_string());
            None
        }
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
        if self.active.is_none() {
            if !line.starts_with("$start") {
                return None;
            }
            let mut parts = line.split(',');
            let _ = parts.next();
            let mode = StreamMode::from_name(parts.next().unwrap_or(""));
            let format = parts.next().unwrap_or("").to_string();
            self.active = Some((mode, format.clone()));
            mode.map(|mode| StreamEvent::Start { mode, format })
        } else if line.starts_with("$end") {
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
            if let Some(p) = parser.feed_line(line) {
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
        assert_eq!(p.feed_line("random noise"), None);
        assert_eq!(p.feed_line("$47.3 outside any packet"), None);
        let packet = collect_packet(
            &mut p,
            &["garbage prefix $start,temp", "$47.3", "junk $end"],
        );
        assert_eq!(packet.name, "temp");
        assert_eq!(packet.args, vec![vec!["47.3".to_string()]]);
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
    fn stream_parser_ignores_lines_outside_streams() {
        let mut p = StreamParser::new();
        assert_eq!(p.feed_line("$75000000,0.5"), None);
        assert_eq!(p.feed_line("noise $start,s11,ri"), None);
    }
}
