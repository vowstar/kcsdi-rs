// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Explicit metadata lookups, separate from the instrument session.

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use kcsdi_core::control::CancellationToken;
use kcsdi_core::discovery::{self, DiscoverySnapshot};
use kcsdi_core::transport::serial::{self, SerialPortInfo};

#[derive(Debug)]
struct Job<T> {
    generation: u64,
    cancel: CancellationToken,
    handle: JoinHandle<Result<T, String>>,
}

/// A single replaceable full snapshot. Publishing never waits for the UI.
#[derive(Debug)]
pub struct Publication<T>(Arc<Mutex<Option<(u64, T)>>>, u64);

impl<T> Publication<T> {
    fn publish(&self, value: T) {
        if let Ok(mut slot) = self.0.try_lock() {
            *slot = Some((self.1, value));
        }
    }
}

#[derive(Debug)]
pub struct Lookup<T> {
    pub data: Option<T>,
    pub error: Option<String>,
    generation: u64,
    closed: bool,
    slot: Arc<Mutex<Option<(u64, T)>>>,
    job: Option<Job<T>>,
}

impl<T> Default for Lookup<T> {
    fn default() -> Self {
        Self {
            data: None,
            error: None,
            generation: 0,
            closed: false,
            slot: Arc::default(),
            job: None,
        }
    }
}

impl<T> Lookup<T> {
    pub fn is_pending(&self) -> bool {
        self.job.is_some()
    }

    pub fn cancel(&mut self) {
        if let Some(job) = &self.job {
            job.cancel.cancel();
            self.generation = self.generation.wrapping_add(1);
        }
    }

    fn shutdown(&mut self) {
        self.closed = true;
        self.cancel();
    }

    pub fn poll(&mut self) {
        if let Ok(mut slot) = self.slot.try_lock()
            && let Some((generation, data)) = slot.take()
            && generation == self.generation
        {
            self.data = Some(data);
        }
        if self
            .job
            .as_ref()
            .is_some_and(|job| job.handle.is_finished())
        {
            let job = self.job.take().expect("finished job exists");
            let result = job
                .handle
                .join()
                .unwrap_or_else(|_| Err("Lookup worker stopped unexpectedly".into()));
            if job.generation == self.generation {
                match result {
                    Ok(data) => self.data = Some(data),
                    Err(error) => self.error = Some(error),
                }
            }
        }
    }
}

impl<T: Send + 'static> Lookup<T> {
    #[cfg(test)]
    pub(crate) fn start_test(
        &mut self,
        work: impl FnOnce(CancellationToken) -> Result<T, String> + Send + 'static,
    ) {
        self.start(|cancel, _| work(cancel));
    }

    fn start(
        &mut self,
        work: impl FnOnce(CancellationToken, Publication<T>) -> Result<T, String> + Send + 'static,
    ) {
        self.poll();
        if self.closed || self.is_pending() {
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.data = None;
        self.error = None;
        let cancel = CancellationToken::default();
        let worker_cancel = cancel.clone();
        let publication = Publication(self.slot.clone(), generation);
        match std::thread::Builder::new()
            .name("device-lookup".into())
            .spawn(move || work(worker_cancel, publication))
        {
            Ok(handle) => {
                self.job = Some(Job {
                    generation,
                    cancel,
                    handle,
                })
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }
}

impl<T> Drop for Lookup<T> {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            job.cancel.cancel();
            let _ = job.handle.join();
        }
    }
}

impl Lookup<Vec<SerialPortInfo>> {
    pub fn refresh_ports(&mut self) {
        self.start(|cancel, _| {
            let mut ports =
                serial::available_ports_controlled(&cancel).map_err(|error| error.to_string())?;
            ports.truncate(256);
            Ok(ports)
        });
    }
}

impl Lookup<DiscoverySnapshot> {
    pub fn start_scan(&mut self) {
        self.start(|cancel, publication| {
            discovery::scan_controlled(&cancel, |snapshot| publication.publish(snapshot))
                .map_err(|error| error.to_string())
        });
    }
}

#[derive(Debug, Default)]
pub struct DeviceLookup {
    pub ports: Lookup<Vec<SerialPortInfo>>,
    pub discovery: Lookup<DiscoverySnapshot>,
    pub discovery_open: bool,
}

impl DeviceLookup {
    pub fn poll(&mut self) {
        self.ports.poll();
        self.discovery.poll();
        if let Some(snapshot) = &mut self.discovery.data {
            snapshot.retain_fresh(Instant::now());
        }
    }

    pub fn cancel(&mut self) {
        self.ports.shutdown();
        self.discovery.shutdown();
    }

    pub fn is_pending(&self) -> bool {
        self.ports.is_pending() || self.discovery.is_pending()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn finish<T>(lookup: &mut Lookup<T>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while lookup.is_pending() && Instant::now() < deadline {
            lookup.poll();
            std::thread::yield_now();
        }
        assert!(!lookup.is_pending());
    }

    #[test]
    fn cancellation_rejects_late_progress_and_completion() {
        let mut lookup = Lookup::default();
        let (tx, rx) = mpsc::channel();
        lookup.start(move |_, publication| {
            rx.recv().unwrap();
            publication.publish(41);
            Ok(42)
        });
        lookup.cancel();
        tx.send(()).unwrap();
        finish(&mut lookup);
        assert_eq!(lookup.data, None);
        assert_eq!(lookup.error, None);
        lookup.start(|_, _| Ok(7));
        finish(&mut lookup);
        assert_eq!(lookup.data, Some(7));
    }

    #[test]
    fn snapshots_coalesce_and_contended_publication_never_waits() {
        let slot = Arc::new(Mutex::new(None));
        let publisher = Publication(slot.clone(), 9);
        for value in 0..10000 {
            publisher.publish(value);
        }
        assert_eq!(*slot.lock().unwrap(), Some((9, 9999)));
        let guard = slot.lock().unwrap();
        publisher.publish(123);
        assert_eq!(*guard, Some((9, 9999)));
    }

    #[test]
    fn single_job_and_cancel_do_not_touch_device_tokens() {
        let mut lookup = Lookup::default();
        let device = CancellationToken::default();
        lookup.start(|cancel, _| {
            while !cancel.is_cancelled() {
                std::thread::yield_now();
            }
            Err("cancelled".into())
        });
        lookup.start(|_, _| -> Result<u32, String> { panic!("second job started") });
        lookup.cancel();
        finish(&mut lookup);
        assert!(!device.is_cancelled());
        assert!(lookup.error.is_none());
    }

    #[test]
    fn shutdown_prevents_later_ui_clicks_from_starting_jobs() {
        let mut lookup = Lookup::<u32>::default();
        lookup.shutdown();
        lookup.start(|_, _| panic!("closed lookup restarted"));
        assert!(!lookup.is_pending());
    }

    #[test]
    fn expiry_does_not_touch_serial_results_or_device_session() {
        use kcsdi_core::connection::ConnectionTarget;
        use kcsdi_core::discovery::DiscoveredDevice;
        let now = Instant::now();
        let mut lookup = DeviceLookup::default();
        lookup.ports.data = Some(vec![SerialPortInfo {
            path: "COM7".into(),
            label: "USB".into(),
        }]);
        lookup.discovery.data = Some(DiscoverySnapshot {
            devices: vec![DiscoveredDevice {
                target: ConnectionTarget::Tcp {
                    host: "192.0.2.1".into(),
                    port: 4321,
                },
                hostname: "synthetic.local".into(),
                product: "KC901V".into(),
                observed_at: now - Duration::from_secs(31),
                expires_at: now,
            }],
        });
        lookup.poll();
        assert!(lookup.discovery.data.as_ref().unwrap().devices.is_empty());
        assert_eq!(lookup.ports.data.as_ref().unwrap()[0].path, "COM7");
    }
}
