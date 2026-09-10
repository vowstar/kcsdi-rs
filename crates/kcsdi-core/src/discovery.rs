// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Explicit, bounded KC901V discovery without opening a control connection.

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use mdns_sd::{DaemonEvent, DaemonStatus, Receiver, ResolvedService, ServiceDaemon, ServiceEvent};

use crate::connection::ConnectionTarget;
use crate::control::CancellationToken;
use crate::{Error, Result};

// Section 1.3. These are compatibility aliases, not a scan of other services.
const SERVICE_TYPES: [&str; 2] = [
    "__KCMA_DEVICE_SERVICE_._udp.local.",
    "_KCMA_DEVICE_SERVICE_._udp.local.",
];
const SCAN_DURATION: Duration = Duration::from_secs(3);
const FRESHNESS: Duration = Duration::from_secs(30);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_DEVICES: usize = 64;
const MAX_ADDRESSES: usize = 16;
const MAX_NAME_BYTES: usize = 255;
const OWNER_IDLE: u8 = 0;
const OWNER_ACTIVE: u8 = 1;
const OWNER_FAILED: u8 = 2;
static DISCOVERY_OWNER: AtomicU8 = AtomicU8::new(OWNER_IDLE);

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct DiscoverySnapshot {
    pub devices: Vec<DiscoveredDevice>,
}

impl DiscoverySnapshot {
    pub fn retain_fresh(&mut self, now: Instant) {
        self.devices.retain(|device| device.is_fresh(now));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredDevice {
    pub target: ConnectionTarget,
    pub hostname: String,
    pub product: String,
    /// Time the host received a resolved service event, not a reachability check.
    pub observed_at: Instant,
    /// Host freshness limit. The discovery library does not expose DNS TTL here.
    pub expires_at: Instant,
}

impl DiscoveredDevice {
    pub fn is_fresh(&self, now: Instant) -> bool {
        now < self.expires_at
    }
}

/// Browse the two KC901 service aliases for three seconds, then stop the daemon.
///
/// The callback receives complete, bounded snapshots and must not block. This
/// function neither connects to a discovered device nor verifies its identity.
/// Cancellation is polled during the scan. Cleanup has a separate two-second
/// budget and runs on success, cancellation and error. A failed cleanup disables
/// further scans in this process because daemon termination is then uncertain.
pub fn scan_controlled(
    cancel: &CancellationToken,
    callback: impl FnMut(DiscoverySnapshot),
) -> Result<DiscoverySnapshot> {
    scan_with_backend(cancel, callback, SCAN_DURATION, MdnsBrowser::new)
}

#[derive(Default)]
struct Collector {
    sightings: BTreeMap<(String, String), Sighting>,
}

struct Sighting {
    hostname: String,
    port: u16,
    addresses: BTreeSet<Ipv4Addr>,
    observed_at: Instant,
    expires_at: Instant,
}

impl Collector {
    fn apply(&mut self, event: ServiceEvent, now: Instant) -> Result<()> {
        self.sightings
            .retain(|_, sighting| now < sighting.expires_at);
        match event {
            ServiceEvent::ServiceResolved(service) => {
                let Some(key) = service_key(&service.ty_domain, &service.fullname) else {
                    return Ok(());
                };
                // An invalid replacement must not leave an older endpoint visible.
                self.sightings.remove(&key);
                if self.sightings.len() < MAX_DEVICES
                    && let Some(sighting) = Sighting::from_service(&service, now)
                {
                    self.sightings.insert(key, sighting);
                }
            }
            ServiceEvent::ServiceRemoved(service_type, fullname) => {
                if let Some(key) = service_key(&service_type, &fullname) {
                    self.sightings.remove(&key);
                }
            }
            ServiceEvent::SearchStopped(_) => {
                return Err(discovery_error("discovery stopped before its deadline"));
            }
            _ => {}
        }
        Ok(())
    }

    fn snapshot(&self, now: Instant) -> DiscoverySnapshot {
        let mut endpoints = BTreeMap::<(Ipv4Addr, u16), &Sighting>::new();
        for sighting in self.sightings.values().filter(|item| now < item.expires_at) {
            for &address in &sighting.addresses {
                let entry = endpoints
                    .entry((address, sighting.port))
                    .or_insert(sighting);
                if sighting.observed_at > entry.observed_at {
                    *entry = sighting;
                }
                // Keep a stable endpoint order without retaining an unbounded list.
                if endpoints.len() > MAX_DEVICES {
                    endpoints.pop_last();
                }
            }
        }
        DiscoverySnapshot {
            devices: endpoints
                .into_iter()
                .map(|((address, port), sighting)| DiscoveredDevice {
                    target: ConnectionTarget::Tcp {
                        host: address.to_string(),
                        port,
                    },
                    hostname: sighting.hostname.clone(),
                    product: "KC901V".into(),
                    observed_at: sighting.observed_at,
                    expires_at: sighting.expires_at,
                })
                .collect(),
        }
    }
}

impl Sighting {
    fn from_service(service: &ResolvedService, now: Instant) -> Option<Self> {
        if !valid_name(&service.host)
            || service.port == 0
            || service.addresses.len() > MAX_ADDRESSES
            || service.get_property_val("product").flatten() != Some(b"KC901V")
        {
            return None;
        }
        let addresses: BTreeSet<_> = service
            .addresses
            .iter()
            .filter_map(|address| match address {
                mdns_sd::ScopedIp::V4(address) => Some(*address.addr()),
                _ => None,
            })
            .filter(|address| {
                !address.is_unspecified() && !address.is_multicast() && !address.is_broadcast()
            })
            .collect();
        if addresses.is_empty() {
            return None;
        }
        Some(Self {
            hostname: service.host.clone(),
            port: service.port,
            addresses,
            observed_at: now,
            expires_at: now.checked_add(FRESHNESS)?,
        })
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && !name.chars().any(char::is_control)
        && !name.trim().is_empty()
}

fn service_key(service_type: &str, fullname: &str) -> Option<(String, String)> {
    if !valid_name(fullname)
        || !SERVICE_TYPES
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(service_type))
    {
        return None;
    }
    let service_type = service_type.to_ascii_lowercase();
    let fullname = fullname.to_ascii_lowercase();
    let instance = fullname.strip_suffix(&service_type)?.strip_suffix('.')?;
    if instance.is_empty() {
        return None;
    }
    Some((service_type, fullname))
}

trait ScanBackend {
    fn next_event(
        &mut self,
        cancel: &CancellationToken,
        timeout: Duration,
    ) -> Result<Option<ServiceEvent>>;
    fn close(&mut self) -> Result<()>;
}

fn scan_with_backend<B: ScanBackend>(
    cancel: &CancellationToken,
    mut callback: impl FnMut(DiscoverySnapshot),
    duration: Duration,
    factory: impl FnOnce() -> Result<B>,
) -> Result<DiscoverySnapshot> {
    cancel.check()?;
    let started = Instant::now();
    let mut backend = factory()?;
    let result = (|| {
        cancel.check()?;
        let mut collector = Collector::default();
        let mut last_snapshot = DiscoverySnapshot::default();
        callback(last_snapshot.clone());
        loop {
            cancel.check()?;
            let Some(remaining) = duration.checked_sub(started.elapsed()) else {
                return Ok(collector.snapshot(Instant::now()));
            };
            if let Some(event) = backend.next_event(cancel, remaining.min(POLL_INTERVAL))? {
                collector.apply(event, Instant::now())?;
            }
            let snapshot = collector.snapshot(Instant::now());
            if snapshot != last_snapshot {
                callback(snapshot.clone());
                last_snapshot = snapshot;
            }
        }
    })();
    let cleanup = backend.close();
    match (result, cleanup) {
        (Ok(snapshot), Ok(())) => Ok(snapshot),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(discovery_error(format!("{error}. {cleanup}"))),
    }
}

struct Owner<'a> {
    state: &'a AtomicU8,
    stopped: bool,
}

impl<'a> Owner<'a> {
    fn acquire(state: &'a AtomicU8) -> Result<Self> {
        state
            .compare_exchange(OWNER_IDLE, OWNER_ACTIVE, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|state| {
                discovery_error(if state == OWNER_FAILED {
                    "previous discovery cleanup was not confirmed. Restart the application before scanning again"
                } else {
                    "a discovery scan is already active"
                })
            })?;
        Ok(Self {
            state,
            stopped: true,
        })
    }
}

impl Drop for Owner<'_> {
    fn drop(&mut self) {
        self.state.store(
            if self.stopped {
                OWNER_IDLE
            } else {
                OWNER_FAILED
            },
            Ordering::Release,
        );
    }
}

struct MdnsBrowser {
    daemon: Option<ServiceDaemon>,
    receivers: Vec<Receiver<ServiceEvent>>,
    monitor: Option<Receiver<DaemonEvent>>,
    next_receiver: usize,
    owner: Owner<'static>,
}

impl MdnsBrowser {
    fn new() -> Result<Self> {
        let mut owner = Owner::acquire(&DISCOVERY_OWNER)?;
        let daemon = ServiceDaemon::new().map_err(mdns_error)?;
        owner.stopped = false;
        // Establish cleanup ownership before any further fallible setup.
        let mut browser = Self {
            daemon: Some(daemon),
            receivers: Vec::with_capacity(SERVICE_TYPES.len()),
            monitor: None,
            next_receiver: 0,
            owner,
        };
        let setup = (|| {
            let daemon = browser
                .daemon
                .as_ref()
                .expect("daemon is owned during setup");
            browser.monitor = Some(daemon.monitor().map_err(mdns_error)?);
            for service_type in SERVICE_TYPES {
                browser
                    .receivers
                    .push(daemon.browse(service_type).map_err(mdns_error)?);
            }
            Ok(())
        })();
        if let Err(error) = setup {
            return match browser.close() {
                Ok(()) => Err(error),
                Err(cleanup) => Err(discovery_error(format!("{error}. {cleanup}"))),
            };
        }
        Ok(browser)
    }
}

impl ScanBackend for MdnsBrowser {
    fn next_event(
        &mut self,
        cancel: &CancellationToken,
        timeout: Duration,
    ) -> Result<Option<ServiceEvent>> {
        cancel.check()?;
        if let Some(monitor) = &self.monitor {
            for _ in 0..16 {
                match monitor.try_recv() {
                    Ok(DaemonEvent::Error(error)) => return Err(mdns_error(error)),
                    Ok(_) => {}
                    Err(mdns_sd::TryRecvError::Empty) => break,
                    Err(mdns_sd::TryRecvError::Disconnected) => {
                        return Err(discovery_error("discovery daemon stopped unexpectedly"));
                    }
                }
            }
        }
        for _ in 0..self.receivers.len() {
            let index = self.next_receiver;
            self.next_receiver = (index + 1) % self.receivers.len();
            match self.receivers[index].try_recv() {
                Ok(event) => return Ok(Some(event)),
                Err(mdns_sd::TryRecvError::Empty) => {}
                Err(mdns_sd::TryRecvError::Disconnected) => {
                    return Err(discovery_error(
                        "discovery event channel closed unexpectedly",
                    ));
                }
            }
        }
        cancel.pause(timeout)?;
        Ok(None)
    }

    fn close(&mut self) -> Result<()> {
        // mdns-sd may block delivering browse events. Drop every receiver before
        // requesting shutdown so that those sends can unwind.
        self.receivers.clear();
        self.monitor = None;
        let Some(daemon) = self.daemon.take() else {
            return Ok(());
        };
        match shutdown_daemon(&daemon) {
            Ok(()) => {
                self.owner.stopped = true;
                Ok(())
            }
            Err(error) => Err(discovery_error(format!(
                "discovery cleanup was not confirmed. Restart the application before scanning again. {error}"
            ))),
        }
    }
}

impl Drop for MdnsBrowser {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn shutdown_daemon(daemon: &ServiceDaemon) -> Result<()> {
    let started = Instant::now();
    loop {
        let Some(remaining) = CLEANUP_TIMEOUT.checked_sub(started.elapsed()) else {
            return Err(Error::Timeout);
        };
        match daemon.shutdown() {
            Ok(receiver) => {
                let remaining = CLEANUP_TIMEOUT
                    .checked_sub(started.elapsed())
                    .ok_or(Error::Timeout)?;
                return match receiver.recv_timeout(remaining) {
                    Ok(DaemonStatus::Shutdown) => Ok(()),
                    Ok(_) => Err(discovery_error("unexpected discovery shutdown status")),
                    Err(mdns_sd::RecvTimeoutError::Timeout) => Err(Error::Timeout),
                    Err(mdns_sd::RecvTimeoutError::Disconnected) => Err(discovery_error(
                        "discovery shutdown acknowledgement was lost",
                    )),
                };
            }
            Err(mdns_sd::Error::DaemonShutdown) => return Ok(()),
            Err(mdns_sd::Error::Again) => std::thread::sleep(remaining.min(POLL_INTERVAL)),
            Err(error) => return Err(mdns_error(error)),
        }
    }
}

fn discovery_error(message: impl Into<String>) -> Error {
    Error::Io(std::io::Error::other(message.into()))
}

fn mdns_error(error: mdns_sd::Error) -> Error {
    discovery_error(format!("discovery failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    use mdns_sd::{ServiceInfo, TxtProperties};

    // ServiceInfo only constructs owned records here. These tests never create
    // a daemon, enumerate interfaces or verify raw DNS TTL-zero processing.
    fn service(alias: usize, name: &str, addresses: &str, port: u16) -> ResolvedService {
        ServiceInfo::new(
            SERVICE_TYPES[alias],
            name,
            "instrument.local.",
            addresses,
            port,
            [("product", "KC901V")].as_slice(),
        )
        .unwrap()
        .as_resolved_service()
    }

    fn resolved(service: ResolvedService) -> ServiceEvent {
        ServiceEvent::ServiceResolved(Box::new(service))
    }

    fn endpoints(snapshot: &DiscoverySnapshot) -> Vec<(String, u16)> {
        snapshot
            .devices
            .iter()
            .map(|device| match &device.target {
                ConnectionTarget::Tcp { host, port } => (host.clone(), *port),
                ConnectionTarget::Serial { .. } => panic!("discovery emitted a serial target"),
            })
            .collect()
    }

    #[test]
    fn resolved_aliases_produce_tcp_ipv4_rows_with_host_freshness() {
        let now = Instant::now();
        let mut collector = Collector::default();
        collector
            .apply(
                ServiceEvent::ServiceFound(SERVICE_TYPES[0].into(), "incomplete".into()),
                now,
            )
            .unwrap();
        assert!(collector.snapshot(now).devices.is_empty());
        for alias in 0..SERVICE_TYPES.len() {
            collector
                .apply(
                    resolved(service(alias, "fixture", "192.0.2.7,2001:db8::7", 901)),
                    now,
                )
                .unwrap();
        }
        let snapshot = collector.snapshot(now);
        assert_eq!(endpoints(&snapshot), vec![("192.0.2.7".into(), 901)]);
        let device = &snapshot.devices[0];
        assert_eq!(device.hostname, "instrument.local.");
        assert_eq!(device.product, "KC901V");
        assert_eq!(device.observed_at, now);
        assert_eq!(device.expires_at, now + FRESHNESS);
        assert!(device.is_fresh(now + FRESHNESS - Duration::from_nanos(1)));
        assert!(!device.is_fresh(now + FRESHNESS));
        let mut expired = snapshot;
        expired.retain_fresh(now + FRESHNESS);
        assert!(expired.devices.is_empty());
    }

    #[test]
    fn exact_product_required_and_malformed_metadata_is_rejected() {
        let now = Instant::now();
        let base = service(0, "fixture", "192.0.2.7", 901);
        let mut cases = Vec::new();
        for txt in [
            b"".as_slice(),
            b"\x07product",
            b"\x08product=",
            b"\x0eproduct=kc901v",
            b"\x0fproduct=KC901V ",
            b"\x0eproduct=KC901S",
            b"\x0eproduct=KC901\xff",
            b"\x20product=KC901V",
        ] {
            let mut item = base.clone();
            item.txt_properties = TxtProperties::from(txt);
            cases.push(item);
        }
        for hostname in [
            "".into(),
            " ".into(),
            "bad\nhost.local.".into(),
            "x".repeat(256),
        ] {
            let mut item = base.clone();
            item.host = hostname;
            cases.push(item);
        }
        let mut item = base.clone();
        item.port = 0;
        cases.push(item);
        let mut item = base.clone();
        item.addresses.clear();
        cases.push(item);
        let mut item = base.clone();
        item.ty_domain = "_http._tcp.local.".into();
        cases.push(item);
        let mut item = base.clone();
        item.fullname = "fixture._http._tcp.local.".into();
        cases.push(item);
        let mut item = base.clone();
        item.fullname = format!(".{}", SERVICE_TYPES[0]);
        cases.push(item);
        let mut item = base.clone();
        item.fullname = format!("{}.{}", "x".repeat(256), SERVICE_TYPES[0]);
        cases.push(item);
        for item in cases {
            let mut collector = Collector::default();
            collector.apply(resolved(item.clone()), now).unwrap();
            assert!(collector.snapshot(now).devices.is_empty(), "{item:?}");
        }
        let mut item = base;
        item.txt_properties = TxtProperties::from(b"\x0ePRODUCT=KC901V".as_slice());
        let mut collector = Collector::default();
        collector.apply(resolved(item), now).unwrap();
        assert_eq!(collector.snapshot(now).devices.len(), 1);
    }

    #[test]
    fn ipv6_only_and_nonunicast_ipv4_do_not_produce_endpoints() {
        let now = Instant::now();
        for addresses in ["2001:db8::7", "0.0.0.0", "224.0.0.251", "255.255.255.255"] {
            let mut collector = Collector::default();
            collector
                .apply(resolved(service(0, "fixture", addresses, 901)), now)
                .unwrap();
            assert!(collector.snapshot(now).devices.is_empty());
        }
        let mut collector = Collector::default();
        collector
            .apply(
                resolved(service(
                    0,
                    "fixture",
                    "192.0.2.9,192.0.2.2,224.0.0.251",
                    902,
                )),
                now,
            )
            .unwrap();
        assert_eq!(
            endpoints(&collector.snapshot(now)),
            vec![("192.0.2.2".into(), 902), ("192.0.2.9".into(), 902)]
        );
    }

    #[test]
    fn removal_is_per_alias_and_instance_and_case_insensitive() {
        let now = Instant::now();
        let mut collector = Collector::default();
        let first = service(0, "fixture", "192.0.2.7", 901);
        let second = service(1, "fixture", "192.0.2.7", 901);
        for item in [&first, &second] {
            collector.apply(resolved(item.clone()), now).unwrap();
        }
        // An injected removal tests event ownership, not DNS TTL-zero decoding.
        collector
            .apply(
                ServiceEvent::ServiceRemoved(
                    first.ty_domain.to_ascii_lowercase(),
                    first.fullname.to_ascii_uppercase(),
                ),
                now,
            )
            .unwrap();
        assert_eq!(collector.snapshot(now).devices.len(), 1);
        collector
            .apply(
                ServiceEvent::ServiceRemoved(second.ty_domain, second.fullname),
                now,
            )
            .unwrap();
        assert!(collector.snapshot(now).devices.is_empty());
    }

    #[test]
    fn latest_sighting_replaces_endpoint_and_invalid_update_retires_it() {
        let now = Instant::now();
        let mut collector = Collector::default();
        collector
            .apply(resolved(service(0, "fixture", "192.0.2.7", 901)), now)
            .unwrap();
        let mut replacement = service(0, "fixture", "192.0.2.8", 902);
        replacement.host = "replacement.local.".into();
        let later = now + Duration::from_secs(1);
        collector
            .apply(resolved(replacement.clone()), later)
            .unwrap();
        let snapshot = collector.snapshot(later);
        assert_eq!(endpoints(&snapshot), vec![("192.0.2.8".into(), 902)]);
        assert_eq!(snapshot.devices[0].hostname, "replacement.local.");
        assert_eq!(snapshot.devices[0].observed_at, later);
        replacement.port = 0;
        collector.apply(resolved(replacement), later).unwrap();
        assert!(collector.snapshot(later).devices.is_empty());
    }

    #[test]
    fn fresher_alias_wins_metadata_and_removal_restores_older_sighting() {
        let now = Instant::now();
        let mut collector = Collector::default();
        collector
            .apply(resolved(service(0, "older", "192.0.2.7", 901)), now)
            .unwrap();
        let mut newer = service(1, "newer", "192.0.2.7", 901);
        newer.host = "newer.local.".into();
        let later = now + Duration::from_secs(2);
        collector.apply(resolved(newer.clone()), later).unwrap();
        assert_eq!(
            collector.snapshot(later).devices[0].hostname,
            "newer.local."
        );
        collector
            .apply(
                ServiceEvent::ServiceRemoved(newer.ty_domain, newer.fullname),
                later,
            )
            .unwrap();
        assert_eq!(collector.snapshot(later).devices[0].observed_at, now);
        assert!(collector.snapshot(now + FRESHNESS).devices.is_empty());
    }

    #[test]
    fn sightings_addresses_and_results_have_independent_caps() {
        let now = Instant::now();
        let mut collector = Collector::default();
        for index in 0..100 {
            let addresses = (1..=MAX_ADDRESSES)
                .map(|last| format!("192.0.{index}.{last}"))
                .collect::<Vec<_>>()
                .join(",");
            collector
                .apply(
                    resolved(service(0, &format!("fixture{index}"), &addresses, 901)),
                    now,
                )
                .unwrap();
        }
        assert_eq!(collector.sightings.len(), MAX_DEVICES);
        assert!(
            collector
                .sightings
                .values()
                .all(|item| item.addresses.len() == MAX_ADDRESSES)
        );
        let snapshot = collector.snapshot(now);
        assert_eq!(snapshot.devices.len(), MAX_DEVICES);
        assert_eq!(endpoints(&snapshot)[0], ("192.0.0.1".into(), 901));
        assert_eq!(endpoints(&snapshot)[63], ("192.0.3.16".into(), 901));

        // A known instance can update even after the sightings table is full.
        collector
            .apply(resolved(service(0, "fixture0", "192.0.2.200", 903)), now)
            .unwrap();
        assert_eq!(collector.sightings.len(), MAX_DEVICES);
        assert!(endpoints(&collector.snapshot(now)).contains(&("192.0.2.200".into(), 903)));

        let addresses = (1..=MAX_ADDRESSES + 1)
            .map(|last| format!("192.0.2.{last}"))
            .collect::<Vec<_>>()
            .join(",");
        let later = now + FRESHNESS;
        collector
            .apply(resolved(service(0, "too-many", &addresses, 901)), later)
            .unwrap();
        assert!(collector.sightings.is_empty());
        collector
            .apply(resolved(service(0, "fresh", "192.0.2.7", 901)), later)
            .unwrap();
        assert_eq!(collector.snapshot(later).devices.len(), 1);
    }

    #[test]
    fn endpoint_sorting_does_not_depend_on_event_order() {
        let now = Instant::now();
        let records = [
            service(0, "third", "192.0.2.20", 901),
            service(0, "second", "192.0.2.2", 902),
            service(1, "first", "192.0.2.2", 901),
        ];
        let mut forward = Collector::default();
        let mut reverse = Collector::default();
        for item in &records {
            forward.apply(resolved(item.clone()), now).unwrap();
        }
        for item in records.into_iter().rev() {
            reverse.apply(resolved(item), now).unwrap();
        }
        assert_eq!(forward.snapshot(now), reverse.snapshot(now));
        assert_eq!(
            endpoints(&forward.snapshot(now)),
            vec![
                ("192.0.2.2".into(), 901),
                ("192.0.2.2".into(), 902),
                ("192.0.2.20".into(), 901)
            ]
        );
    }

    struct FakeBrowser {
        events: VecDeque<Result<ServiceEvent>>,
        closed: Rc<Cell<bool>>,
        close_error: bool,
    }

    impl ScanBackend for FakeBrowser {
        fn next_event(
            &mut self,
            cancel: &CancellationToken,
            timeout: Duration,
        ) -> Result<Option<ServiceEvent>> {
            assert!(!self.closed.get());
            if let Some(event) = self.events.pop_front() {
                return event.map(Some);
            }
            cancel.pause(timeout)?;
            Ok(None)
        }

        fn close(&mut self) -> Result<()> {
            self.closed.set(true);
            if self.close_error {
                Err(discovery_error("injected cleanup failure"))
            } else {
                Ok(())
            }
        }
    }

    fn fake(events: Vec<Result<ServiceEvent>>) -> (FakeBrowser, Rc<Cell<bool>>) {
        let closed = Rc::new(Cell::new(false));
        (
            FakeBrowser {
                events: events.into(),
                closed: closed.clone(),
                close_error: false,
            },
            closed,
        )
    }

    #[test]
    fn precancelled_scan_never_constructs_backend_or_calls_callback() {
        let cancel = CancellationToken::default();
        cancel.cancel();
        let result = scan_with_backend::<FakeBrowser>(
            &cancel,
            |_| panic!("cancelled scan called callback"),
            SCAN_DURATION,
            || panic!("cancelled scan constructed backend"),
        );
        assert!(matches!(result, Err(Error::Cancelled)));
    }

    #[test]
    fn callback_cancellation_still_closes_backend_and_preserves_full_snapshots() {
        let cancel = CancellationToken::default();
        let (backend, closed) = fake(vec![Ok(resolved(service(0, "fixture", "192.0.2.7", 901)))]);
        let snapshots = RefCell::new(Vec::new());
        let result = scan_with_backend(
            &cancel,
            |snapshot| {
                assert!(!closed.get());
                if !snapshot.devices.is_empty() {
                    cancel.cancel();
                }
                snapshots.borrow_mut().push(snapshot);
            },
            SCAN_DURATION,
            || Ok(backend),
        );
        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(closed.get());
        let snapshots = snapshots.into_inner();
        assert_eq!(snapshots.len(), 2);
        assert!(snapshots[0].devices.is_empty());
        assert_eq!(snapshots[1].devices.len(), 1);
    }

    #[test]
    fn scan_error_and_unexpected_stop_still_close_backend() {
        for event in [
            Err(discovery_error("injected daemon failure")),
            Ok(ServiceEvent::SearchStopped(SERVICE_TYPES[0].into())),
        ] {
            let (backend, closed) = fake(vec![event]);
            let result = scan_with_backend(
                &CancellationToken::default(),
                |_| {},
                SCAN_DURATION,
                || Ok(backend),
            );
            assert!(result.is_err());
            assert!(closed.get());
        }
    }

    #[test]
    fn expired_scan_returns_after_cleanup_without_waiting_for_events() {
        let (backend, closed) = fake(Vec::new());
        let result = scan_with_backend(
            &CancellationToken::default(),
            |_| {},
            Duration::ZERO,
            || Ok(backend),
        );
        assert_eq!(result.unwrap(), DiscoverySnapshot::default());
        assert!(closed.get());
    }

    #[test]
    fn backend_creation_failure_does_not_publish_success() {
        let result = scan_with_backend::<FakeBrowser>(
            &CancellationToken::default(),
            |_| panic!("failed setup called callback"),
            SCAN_DURATION,
            || Err(discovery_error("injected construction failure")),
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("injected construction failure")
        );
    }

    #[test]
    fn cleanup_failure_is_reported_after_success_or_cancellation() {
        for cancelled in [false, true] {
            let cancel = CancellationToken::default();
            let (mut backend, closed) = fake(Vec::new());
            backend.close_error = true;
            let result = scan_with_backend(
                &cancel,
                |_| {
                    if cancelled {
                        cancel.cancel();
                    }
                },
                Duration::ZERO,
                || Ok(backend),
            );
            assert!(closed.get());
            let message = result.unwrap_err().to_string();
            assert!(message.contains("injected cleanup failure"));
            if cancelled {
                assert!(message.contains("cancelled"));
            }
        }
    }

    #[test]
    fn discovery_owner_is_single_flight_and_released_only_after_confirmed_stop() {
        let state = AtomicU8::new(OWNER_IDLE);
        let owner = Owner::acquire(&state).unwrap();
        assert!(Owner::acquire(&state).is_err());
        drop(owner);
        assert_eq!(state.load(Ordering::Acquire), OWNER_IDLE);

        let mut owner = Owner::acquire(&state).unwrap();
        owner.stopped = false;
        assert!(Owner::acquire(&state).is_err());
        owner.stopped = true;
        drop(owner);
        assert_eq!(state.load(Ordering::Acquire), OWNER_IDLE);

        let mut owner = Owner::acquire(&state).unwrap();
        owner.stopped = false;
        drop(owner);
        assert_eq!(state.load(Ordering::Acquire), OWNER_FAILED);
        let Err(error) = Owner::acquire(&state) else {
            panic!("failed daemon cleanup allowed another scan");
        };
        assert!(error.to_string().contains("Restart"));
    }
}
