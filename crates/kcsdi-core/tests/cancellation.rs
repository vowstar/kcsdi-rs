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
use kcsdi_core::device::{Device, S11Params, SpecParams, SweepProgress};
use kcsdi_core::model::Rbw;
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

fn expect_run(peer: &mut BufReader<TcpStream>, spectrum: bool) {
    assert_eq!(read_line(peer), "$s11,stop\n");
    assert_eq!(read_line(peer), "$spec,stop\n");
    if spectrum {
        assert_eq!(read_line(peer), "$spec,init\n");
        assert_eq!(read_line(peer), "$bw,10k\n");
        assert_eq!(read_line(peer), "$specref,-10\n");
        assert_eq!(
            read_line(peer),
            "$spec,run,caloff,highlo,2,ss,1000000,2000000\n"
        );
    } else {
        assert_eq!(read_line(peer), "$s11,init\n");
        assert_eq!(
            read_line(peer),
            "$s11,run,caloff,loss,2,ss,1000000,2000000\n"
        );
    }
}

fn sweep(
    device: &mut Device<TcpTransport>,
    spectrum: bool,
    cancel: &CancellationToken,
    progress: impl FnMut(SweepProgress<'_>),
) -> Result<SweepData> {
    if spectrum {
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

fn cancel_fragmented_sweep(spectrum: bool) {
    let (mut device, mut peer) = connected_pair();
    let cancel = CancellationToken::default();
    let peer_cancel = cancel.clone();
    let (prefix_seen, wait_for_prefix) = mpsc::channel();
    thread::scope(|scope| {
        let server = scope.spawn(move || {
            handshake(&mut peer);
            expect_run(&mut peer, spectrum);
            let header = if spectrum {
                "$start,spec\n"
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
            expect_run(&mut peer, spectrum);
            peer.get_mut().write_all(header.as_bytes()).unwrap();
            peer.get_mut()
                .write_all(b"$1000000,10\n$1500000,20\n$2000000,30\n$end\n")
                .unwrap();
            assert_eq!(
                read_line(&mut peer),
                if spectrum {
                    "$spec,stop\n"
                } else {
                    "$s11,stop\n"
                }
            );
            assert_eq!(read_line(&mut peer), "$local\n");
            let mut extra = [0];
            assert_eq!(peer.read(&mut extra).unwrap(), 0);
        });

        assert_eq!(device.handshake().unwrap(), "000000000001");
        let mut prefixes = Vec::new();
        let result = sweep(&mut device, spectrum, &cancel, |prefix| {
            prefixes.push(prefix.points.len());
            if prefix.points.len() == 1 {
                prefix_seen.send(()).unwrap();
            }
        });
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        assert_eq!(prefixes, [0, 1]);
        assert!(!device.requires_reconnect());
        let complete = sweep(&mut device, spectrum, &CancellationToken::default(), |_| {}).unwrap();
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
    cancel_fragmented_sweep(false);
}

#[test]
fn spectrum_cancellation_drains_a_fragmented_tail_before_reusing_the_socket() {
    cancel_fragmented_sweep(true);
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
