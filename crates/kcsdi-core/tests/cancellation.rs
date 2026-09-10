// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Local peer replays exercise host cancellation, not instrument behavior.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use kcsdi_core::commands::{Cal, Format, Lo};
use kcsdi_core::control::CancellationToken;
use kcsdi_core::data::SweepData;
use kcsdi_core::device::{
    Device, PointParams, PointSettings, S11Params, S21Params, SpecParams, SweepProgress,
};
use kcsdi_core::model::Rbw;
use kcsdi_core::protocol::StreamMode;
use kcsdi_core::transport::TcpTransport;
use kcsdi_core::{Error, Result};

const PEER_TIMEOUT: Duration = Duration::from_secs(5);
const FRAGMENT_WAIT: Duration = Duration::from_millis(150);
const IDENTITY: &[u8] = b"$start,device\n\
    $Synthetic peer\n\
    $<-User @ :replay>\n\
    $<-Software ver:test>\n\
    $<-Hardware ver:test>\n\
    $<-Serial num:000000000001>\n\
    $<-Copyright:Test fixture>\n\
    $end\n";

fn connected_pair() -> (Device<TcpTransport>, BufReader<TcpStream>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let transport =
        TcpTransport::connect("127.0.0.1", listener.local_addr().unwrap().port()).unwrap();
    // The connection is already established, so accept cannot wait for a
    // client that failed to start. Every subsequent peer wait is bounded.
    let (stream, _) = listener.accept().unwrap();
    stream.set_read_timeout(Some(PEER_TIMEOUT)).unwrap();
    stream.set_write_timeout(Some(PEER_TIMEOUT)).unwrap();
    stream.set_nodelay(true).unwrap();
    (Device::new(transport), BufReader::new(stream))
}

fn read_line(peer: &mut BufReader<TcpStream>) -> String {
    let mut line = String::new();
    assert_ne!(peer.read_line(&mut line).unwrap(), 0, "unexpected EOF");
    line
}

fn handshake(peer: &mut BufReader<TcpStream>) {
    let mut byte = [0];
    peer.read_exact(&mut byte).unwrap();
    assert_eq!(&byte, b"C");
    peer.get_mut()
        .write_all(b"$start,id\n$000000000001\n$end\n")
        .unwrap();
}

fn expect_run(peer: &mut BufReader<TcpStream>, mode: StreamMode) {
    assert_eq!(read_line(peer), "$s11,stop\n");
    assert_eq!(read_line(peer), "$s21,stop\n");
    assert_eq!(read_line(peer), "$spec,stop\n");
    assert_eq!(read_line(peer), "$rfsource,stop\n");
    assert_eq!(read_line(peer), "$afsource,stop\n");
    expect_receiver_run(peer, mode);
}

fn expect_receiver_run(peer: &mut BufReader<TcpStream>, mode: StreamMode) {
    if mode == StreamMode::Spec {
        assert_eq!(read_line(peer), "$spec,init\n");
        assert_eq!(read_line(peer), "$bw,10k\n");
        assert_eq!(read_line(peer), "$specref,-10\n");
        assert_eq!(
            read_line(peer),
            "$spec,run,caloff,highlo,2,ss,1000000,2000000\n"
        );
    } else if mode == StreamMode::S21 {
        assert_eq!(read_line(peer), "$s21,init\n");
        assert_eq!(
            read_line(peer),
            "$s21,run,caloff,loss,highlo,2,ss,1000000,2000000\n"
        );
    } else {
        assert_eq!(read_line(peer), "$s11,init\n");
        assert_eq!(
            read_line(peer),
            "$s11,run,caloff,loss,2,ss,1000000,2000000\n"
        );
    }
}

#[test]
fn initial_front_panel_sources_are_stopped_before_each_receiver_mode() {
    for source in ["rfsource", "afsource"] {
        for mode in [StreamMode::S11, StreamMode::S21, StreamMode::Spec] {
            let (mut device, mut peer) = connected_pair();
            thread::scope(|scope| {
                let server = scope.spawn(move || {
                    handshake(&mut peer);
                    let mut active_source = Some(source);
                    for command in [
                        "$s11,stop\n",
                        "$s21,stop\n",
                        "$spec,stop\n",
                        "$rfsource,stop\n",
                        "$afsource,stop\n",
                    ] {
                        assert_eq!(read_line(&mut peer), command);
                        if command == format!("${source},stop\n") {
                            active_source = None;
                        }
                    }
                    // This peer models the documented source-mode conflict.
                    // No receiver init is accepted before the old source stop.
                    assert_eq!(active_source, None);
                    expect_receiver_run(&mut peer, mode);
                    let header = if mode == StreamMode::Spec {
                        "$start,spec\n".to_owned()
                    } else {
                        format!("$start,{},loss\n", mode.name())
                    };
                    for pass in 0..2 {
                        if pass == 1 {
                            // Repeating the initialized receiver must not replay
                            // source stops or any source initialization/output.
                            if mode == StreamMode::Spec {
                                assert_eq!(read_line(&mut peer), "$bw,10k\n");
                                assert_eq!(read_line(&mut peer), "$specref,-10\n");
                            }
                            let run = match mode {
                                StreamMode::S11 => "$s11,run,caloff,loss,2,ss,1000000,2000000\n",
                                StreamMode::S21 => {
                                    "$s21,run,caloff,loss,highlo,2,ss,1000000,2000000\n"
                                }
                                StreamMode::Spec => {
                                    "$spec,run,caloff,highlo,2,ss,1000000,2000000\n"
                                }
                                _ => unreachable!(),
                            };
                            assert_eq!(read_line(&mut peer), run);
                        }
                        peer.get_mut().write_all(header.as_bytes()).unwrap();
                        peer.get_mut()
                            .write_all(b"$1000000,-1\n$1500000,-2\n$2000000,-3\n$end\n")
                            .unwrap();
                    }
                    assert_eq!(read_line(&mut peer), format!("${},stop\n", mode.name()));
                    assert_eq!(read_line(&mut peer), "$local\n");
                    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
                });
                device.handshake().unwrap();
                for _ in 0..2 {
                    let data =
                        sweep(&mut device, mode, &CancellationToken::default(), |_| {}).unwrap();
                    assert_eq!(data.points.len(), 3);
                    assert_eq!(
                        device.source_report().state,
                        kcsdi_core::source::SourceOutputState::NotStarted
                    );
                }
                device.close();
                drop(device);
                server.join().unwrap();
            });
        }
    }
}

#[test]
fn cancellation_during_source_normalization_never_initializes_or_retries_output() {
    let (mut device, mut peer) = connected_pair();
    let cancel = CancellationToken::default();
    let peer_cancel = cancel.clone();
    thread::scope(|scope| {
        let server = scope.spawn(move || {
            handshake(&mut peer);
            for command in [
                "$s11,stop\n",
                "$s21,stop\n",
                "$spec,stop\n",
                "$rfsource,stop\n",
            ] {
                assert_eq!(read_line(&mut peer), command);
            }
            peer_cancel.cancel();
            let mut byte = [0];
            peer.read_exact(&mut byte).unwrap();
            assert_eq!(byte, [3]);
            assert_eq!(read_line(&mut peer), "$device\n");
            peer.get_mut().write_all(IDENTITY).unwrap();
            // Cancellation has no automatic retry or further init/run command.
            assert_eq!(read_line(&mut peer), "$local\n");
            assert_eq!(peer.read(&mut byte).unwrap(), 0);
        });
        device.handshake().unwrap();
        assert!(matches!(
            sweep(&mut device, StreamMode::S11, &cancel, |_| {}),
            Err(Error::Cancelled)
        ));
        assert!(!device.requires_reconnect());
        device.close();
        drop(device);
        server.join().unwrap();
    });
}

fn sweep(
    device: &mut Device<TcpTransport>,
    mode: StreamMode,
    cancel: &CancellationToken,
    progress: impl FnMut(SweepProgress<'_>),
) -> Result<SweepData> {
    if mode == StreamMode::Spec {
        device.sweep_spec_controlled(
            &SpecParams {
                cal: Cal::CalOff,
                lo: Lo::HighLo,
                points: 3,
                start_hz: 1_000_000,
                stop_hz: 2_000_000,
                rbw: Rbw::R10k,
                ref_level_dbm: -10,
            },
            cancel,
            progress,
        )
    } else if mode == StreamMode::S21 {
        device.sweep_s21_controlled(
            &S21Params {
                cal: Cal::CalOff,
                format: Format::Loss,
                lo: Lo::HighLo,
                points: 3,
                start_hz: 1_000_000,
                stop_hz: 2_000_000,
                rbw: None,
            },
            cancel,
            progress,
        )
    } else {
        device.sweep_s11_controlled(
            &S11Params {
                cal: Cal::CalOff,
                format: Format::Loss,
                points: 3,
                start_hz: 1_000_000,
                stop_hz: 2_000_000,
                rbw: None,
            },
            cancel,
            progress,
        )
    }
}

fn cancel_fragmented_sweep(mode: StreamMode) {
    let (mut device, mut peer) = connected_pair();
    let cancel = CancellationToken::default();
    let peer_cancel = cancel.clone();
    let (prefix_seen, wait_for_prefix) = mpsc::channel();
    thread::scope(|scope| {
        let server = scope.spawn(move || {
            handshake(&mut peer);
            expect_run(&mut peer, mode);
            let header = if mode == StreamMode::Spec {
                "$start,spec\n"
            } else if mode == StreamMode::S21 {
                "$start,s21,loss\n"
            } else {
                "$start,s11,loss\n"
            };
            peer.get_mut().write_all(header.as_bytes()).unwrap();
            peer.get_mut().write_all(b"$1000000,1\n$1500000,").unwrap();
            wait_for_prefix.recv_timeout(PEER_TIMEOUT).unwrap();
            // Leave the second line incomplete across several 50 ms reads.
            thread::sleep(FRAGMENT_WAIT);
            peer_cancel.cancel();
            let mut interrupt = [0];
            peer.read_exact(&mut interrupt).unwrap();
            assert_eq!(interrupt, [3]);
            assert_eq!(read_line(&mut peer), "$device\n");

            // This replay assumes an ordered peer. Residual measurement bytes
            // precede the fresh identity reply, including the fragmented row.
            peer.get_mut().write_all(b"2\n$2000000,3\n$end\n").unwrap();
            peer.get_mut().write_all(IDENTITY).unwrap();
            expect_run(&mut peer, mode);
            peer.get_mut().write_all(header.as_bytes()).unwrap();
            peer.get_mut()
                .write_all(b"$1000000,10\n$1500000,20\n$2000000,30\n$end\n")
                .unwrap();
            assert_eq!(read_line(&mut peer), format!("${},stop\n", mode.name()));
            assert_eq!(read_line(&mut peer), "$local\n");
            let mut extra = [0];
            assert_eq!(peer.read(&mut extra).unwrap(), 0);
        });

        assert_eq!(device.handshake().unwrap(), "000000000001");
        let mut prefixes = Vec::new();
        let result = sweep(&mut device, mode, &cancel, |prefix| {
            prefixes.push(prefix.points.len());
            if prefix.points.len() == 1 {
                prefix_seen.send(()).unwrap();
            }
        });
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        assert_eq!(prefixes, [0, 1]);
        assert!(!device.requires_reconnect());
        let complete = sweep(&mut device, mode, &CancellationToken::default(), |_| {}).unwrap();
        assert_eq!(complete.points.len(), 3);
        assert_eq!(
            complete
                .points
                .iter()
                .map(|point| point.values[0])
                .collect::<Vec<_>>(),
            [10.0, 20.0, 30.0]
        );
        device.close();
        drop(device);
        server.join().unwrap();
    });
}

#[test]
fn s11_cancellation_drains_a_fragmented_tail_before_reusing_the_socket() {
    cancel_fragmented_sweep(StreamMode::S11);
}

#[test]
fn spectrum_cancellation_drains_a_fragmented_tail_before_reusing_the_socket() {
    cancel_fragmented_sweep(StreamMode::Spec);
}

#[test]
fn s21_cancellation_drains_a_fragmented_tail_before_reusing_the_socket() {
    cancel_fragmented_sweep(StreamMode::S21);
}

#[test]
fn interrupted_fragmented_query_retires_the_socket_without_another_query() {
    let (mut device, mut peer) = connected_pair();
    let cancel = CancellationToken::default();
    let peer_cancel = cancel.clone();
    thread::scope(|scope| {
        let server = scope.spawn(move || {
            handshake(&mut peer);
            assert_eq!(read_line(&mut peer), "$temp\n");
            peer.get_mut().write_all(b"$start,temp\n$4").unwrap();
            thread::sleep(FRAGMENT_WAIT);
            peer_cancel.cancel();
            assert_eq!(read_line(&mut peer), "$local\n");
            let mut extra = [0];
            assert_eq!(peer.read(&mut extra).unwrap(), 0);
        });
        device.handshake().unwrap();
        assert!(matches!(
            device.temperature_controlled(&cancel),
            Err(Error::Cancelled)
        ));
        assert!(device.requires_reconnect());
        assert!(matches!(device.temperature(), Err(Error::NotConnected)));
        device.close();
        drop(device);
        server.join().unwrap();
    });
}

fn point_params(mode: StreamMode) -> PointParams {
    PointParams {
        settings: match mode {
            StreamMode::S11 => PointSettings::S11 {
                cal: Cal::CalOff,
                format: Format::Loss,
                rbw: Some(Rbw::R10k),
            },
            StreamMode::S21 => PointSettings::S21 {
                cal: Cal::CalOff,
                format: Format::Loss,
                lo: Lo::HighLo,
                rbw: Some(Rbw::R10k),
            },
            StreamMode::Spec => PointSettings::Spec {
                cal: Cal::CalOff,
                lo: Lo::HighLo,
                rbw: Rbw::R10k,
                ref_level_dbm: -10,
            },
            _ => unreachable!(),
        },
        frequency_hz: 1_000_000,
    }
}

fn expect_point_run(peer: &mut BufReader<TcpStream>, mode: StreamMode) {
    for command in [
        "$s11,stop\n",
        "$s21,stop\n",
        "$spec,stop\n",
        "$rfsource,stop\n",
        "$afsource,stop\n",
    ] {
        assert_eq!(read_line(peer), command);
    }
    assert_eq!(read_line(peer), format!("${},init\n", mode.name()));
    assert_eq!(read_line(peer), "$bw,10k\n");
    let run = match mode {
        StreamMode::S11 => "$s11,run,caloff,loss,1,ss,1000000\n",
        StreamMode::S21 => "$s21,run,caloff,loss,highlo,1,ss,1000000\n",
        StreamMode::Spec => {
            assert_eq!(read_line(peer), "$specref,-10\n");
            "$spec,run,caloff,highlo,1,ss,1000000\n"
        }
        _ => unreachable!(),
    };
    assert_eq!(read_line(peer), run);
}

fn point_header(mode: StreamMode) -> String {
    if mode == StreamMode::Spec {
        "$start,spec\n".into()
    } else {
        format!("$start,{},loss\n", mode.name())
    }
}

fn expect_interrupt_query(peer: &mut BufReader<TcpStream>) {
    let mut interrupt = [0];
    peer.read_exact(&mut interrupt).unwrap();
    assert_eq!(interrupt, [3]);
    assert_eq!(read_line(peer), "$device\n");
}

fn replay_point_then_finite_sweep(mode: StreamMode, cancel_fragment: bool) {
    let (mut device, mut peer) = connected_pair();
    let cancel = CancellationToken::default();
    let peer_cancel = cancel.clone();
    let (returned, wait_for_return) = mpsc::channel();
    thread::scope(|scope| {
        let server = scope.spawn(move || {
            handshake(&mut peer);
            expect_point_run(&mut peer, mode);
            let header = point_header(mode);
            peer.get_mut().write_all(header.as_bytes()).unwrap();
            // The repeated header shape also occurs in recorded point replies.
            peer.get_mut().write_all(header.as_bytes()).unwrap();
            if cancel_fragment {
                peer.get_mut().write_all(b"$999999.").unwrap();
                thread::sleep(FRAGMENT_WAIT);
                peer_cancel.cancel();
                expect_interrupt_query(&mut peer);
                peer.get_mut().write_all(b"25,-12.5\n$end\n").unwrap();
            } else {
                peer.get_mut()
                    .write_all(b"$999999.25,-12.5\n$end\n")
                    .unwrap();
                expect_interrupt_query(&mut peer);
            }
            // Another continuous frame is residual data, not the next result.
            peer.get_mut().write_all(header.as_bytes()).unwrap();
            peer.get_mut().write_all(b"$999999.25,999\n$end\n").unwrap();
            assert!(matches!(
                wait_for_return.recv_timeout(FRAGMENT_WAIT),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            peer.get_mut().write_all(IDENTITY).unwrap();
            wait_for_return.recv_timeout(PEER_TIMEOUT).unwrap();

            // A point fence resets mode state. The next finite sweep still
            // sends its original count and retains all three returned rows.
            expect_run(&mut peer, mode);
            peer.get_mut().write_all(header.as_bytes()).unwrap();
            peer.get_mut()
                .write_all(b"$1000000,10\n$1500000,20\n$2000000,30\n$end\n")
                .unwrap();
            assert_eq!(read_line(&mut peer), format!("${},stop\n", mode.name()));
            assert_eq!(read_line(&mut peer), "$local\n");
            let mut extra = [0];
            assert_eq!(peer.read(&mut extra).unwrap(), 0);
        });
        device.handshake().unwrap();
        let point = device.measure_point_controlled(&point_params(mode), &cancel);
        returned.send(()).unwrap();
        if cancel_fragment {
            assert!(matches!(point, Err(Error::Cancelled)), "{point:?}");
        } else {
            let point = point.unwrap();
            assert_eq!(point.mode, mode);
            assert_eq!(point.points.len(), 1);
            assert_eq!(point.points[0].freq_hz, 999_999.25);
            assert_eq!(point.points[0].values, [-12.5]);
        }
        assert!(!device.requires_reconnect());
        let sweep = sweep(&mut device, mode, &CancellationToken::default(), |_| {}).unwrap();
        assert_eq!(sweep.points.len(), 3);
        assert_eq!(
            sweep
                .points
                .iter()
                .map(|point| point.values[0])
                .collect::<Vec<_>>(),
            [10.0, 20.0, 30.0]
        );
        device.close();
        drop(device);
        server.join().unwrap();
    });
}

#[test]
fn point_results_wait_for_the_identity_fence_before_finite_sweep_reuse() {
    for mode in [StreamMode::S11, StreamMode::S21, StreamMode::Spec] {
        replay_point_then_finite_sweep(mode, false);
    }
}

#[test]
fn point_cancellation_drains_fragmented_and_complete_residual_frames() {
    for mode in [StreamMode::S11, StreamMode::S21, StreamMode::Spec] {
        replay_point_then_finite_sweep(mode, true);
    }
}
