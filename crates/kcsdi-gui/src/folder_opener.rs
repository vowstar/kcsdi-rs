// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Explicit directory opening with bounded host waits and one native launcher.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kcsdi_core::control::CancellationToken;

static NATIVE_BUSY: AtomicBool = AtomicBool::new(false);
const TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);

#[derive(Default)]
pub struct FolderOpener {
    worker: Option<JoinHandle<Result<(), String>>>,
    cancel: CancellationToken,
    closing: bool,
    error: Option<String>,
}

impl FolderOpener {
    #[cfg(test)]
    pub fn start_test_blocked(&mut self) -> (mpsc::Sender<()>, mpsc::Receiver<()>) {
        let (release, wait) = mpsc::channel();
        let wait = std::sync::Mutex::new(wait);
        let (ready, started) = mpsc::channel();
        self.start(
            std::env::temp_dir(),
            egui::Context::default(),
            Arc::new(move |_, _, _| {
                let _ = ready.send(());
                let _ = wait.lock().unwrap().recv_timeout(Duration::from_secs(3));
                Ok(())
            }),
        );
        (release, started)
    }

    pub fn request(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.start(path, ctx.clone(), Arc::new(open_directory));
    }

    fn start(&mut self, path: PathBuf, ctx: egui::Context, launch: Arc<Launcher>) {
        if self.closing || self.is_pending() {
            return;
        }
        self.cancel = CancellationToken::default();
        let cancel = self.cancel.clone();
        self.error = None;
        match std::thread::Builder::new()
            .name("folder-launch".into())
            .spawn(move || {
                let result = bounded_launch(path, cancel, launch, TIMEOUT);
                ctx.request_repaint();
                result
            }) {
            Ok(worker) => self.worker = Some(worker),
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    pub fn poll(&mut self) -> Option<Result<(), String>> {
        if let Some(error) = self.error.take() {
            return Some(Err(error));
        }
        if !self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            return None;
        }
        let result = self
            .worker
            .take()
            .expect("finished folder task")
            .join()
            .unwrap_or_else(|_| Err("Folder launcher stopped unexpectedly".into()));
        (!self.cancel.is_cancelled()).then_some(result)
    }

    pub fn cancel(&mut self) {
        self.closing = true;
        self.cancel.cancel();
        self.error = None;
    }

    pub fn is_pending(&self) -> bool {
        self.worker.is_some()
    }
}

impl Drop for FolderOpener {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

type Launcher = dyn Fn(PathBuf, &CancellationToken, Instant) -> Result<(), String> + Send + Sync;

fn bounded_launch(
    path: PathBuf,
    cancel: CancellationToken,
    launch: Arc<Launcher>,
    timeout: Duration,
) -> Result<(), String> {
    if cancel.is_cancelled() {
        return Ok(());
    }
    let deadline = Instant::now() + timeout;
    let (tx, rx) = mpsc::sync_channel(1);
    let native_cancel = cancel.clone();
    std::thread::Builder::new()
        .name("folder-launch-native".into())
        .spawn(move || {
            let result = launch(path, &native_cancel, deadline);
            let _ = tx.send(result);
        })
        .map_err(|error| error.to_string())?;
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Opening the folder timed out".into());
        }
        match rx.recv_timeout(remaining.min(POLL)) {
            Ok(result) => {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err("Opening the folder timed out".into());
                }
                return result;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("Folder launcher stopped unexpectedly".into());
            }
        }
    }
}

fn guarded_launch(
    gate: &AtomicBool,
    launch: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    struct NativeGuard<'a>(&'a AtomicBool);
    impl Drop for NativeGuard<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    gate.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| "A previous folder launcher is still running".to_owned())?;
    let _guard = NativeGuard(gate);
    launch()
}

fn open_directory(
    path: PathBuf,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(), String> {
    guarded_launch(&NATIVE_BUSY, || {
        checked_open(
            &path,
            cancel,
            deadline,
            |path| {
                let metadata = std::fs::metadata(path)
                    .map_err(|error| format!("Cannot open folder: {error}"))?;
                if metadata.is_dir() {
                    Ok(())
                } else {
                    Err("The selected path is not a folder".into())
                }
            },
            |path| open::that(path).map_err(|error| format!("Cannot open folder: {error}")),
        )
    })
}

fn checked_open(
    path: &std::path::Path,
    cancel: &CancellationToken,
    deadline: Instant,
    check: impl FnOnce(&std::path::Path) -> Result<(), String>,
    launch: impl FnOnce(&std::path::Path) -> Result<(), String>,
) -> Result<(), String> {
    if cancel.is_cancelled() {
        return Ok(());
    }
    if Instant::now() >= deadline {
        return Err("Opening the folder timed out".into());
    }
    if !path.is_absolute() {
        return Err("Folder path must be absolute".into());
    }
    check(path)?;
    if cancel.is_cancelled() {
        return Ok(());
    }
    if Instant::now() >= deadline {
        return Err("Opening the folder timed out".into());
    }
    launch(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_releases_the_host_without_waiting_for_a_blocked_native_launcher() {
        let mut opener = FolderOpener::default();
        let (release, started) = opener.start_test_blocked();
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        let before = Instant::now();
        drop(opener);
        assert!(before.elapsed() < Duration::from_secs(1));
        release.send(()).unwrap();
    }

    #[test]
    fn cancellation_after_validation_prevents_launch_and_gate_is_exclusive() {
        let gate = AtomicBool::new(false);
        let cancel = CancellationToken::default();
        guarded_launch(&gate, || {
            assert!(guarded_launch(&gate, || panic!("second native operation")).is_err());
            checked_open(
                &std::env::temp_dir(),
                &cancel,
                Instant::now() + TIMEOUT,
                |_| {
                    cancel.cancel();
                    Ok(())
                },
                |_| panic!("launch after cancellation"),
            )
        })
        .unwrap();
        assert!(!gate.load(Ordering::Acquire));
        guarded_launch(&gate, || Ok(())).unwrap();
    }

    #[test]
    fn expired_validation_cannot_launch_after_host_timeout() {
        let deadline = Instant::now() + Duration::from_millis(10);
        let launched = AtomicBool::new(false);
        let result = checked_open(
            &std::env::temp_dir(),
            &CancellationToken::default(),
            deadline,
            |_| {
                while Instant::now() < deadline {
                    std::thread::yield_now();
                }
                Ok(())
            },
            |_| {
                launched.store(true, Ordering::Release);
                Ok(())
            },
        );
        assert!(result.unwrap_err().contains("timed out"));
        assert!(!launched.load(Ordering::Acquire));
    }

    fn finish(opener: &mut FolderOpener) -> Option<Result<(), String>> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Some(result) = opener.poll() {
                return Some(result);
            }
            if !opener.is_pending() {
                return None;
            }
            std::thread::yield_now();
        }
        panic!("folder task did not complete");
    }

    #[test]
    fn explicit_request_keeps_the_exact_path_and_reports_failure() {
        let mut opener = FolderOpener::default();
        let path = std::env::temp_dir().join("folder with spaces and 'quotes' 中文");
        let expected = path.clone();
        opener.start(
            path,
            egui::Context::default(),
            Arc::new(move |path, _, _| {
                assert_eq!(path, expected);
                Err("synthetic opener failure".into())
            }),
        );
        assert_eq!(
            finish(&mut opener),
            Some(Err("synthetic opener failure".into()))
        );
    }

    #[test]
    fn native_wait_is_bounded_and_cancellation_does_not_wait_for_the_manager() {
        let (release, wait) = mpsc::channel();
        let wait = std::sync::Mutex::new(wait);
        let started = Instant::now();
        let result = bounded_launch(
            PathBuf::new(),
            CancellationToken::default(),
            Arc::new(move |_, _, _| {
                wait.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap();
                Ok(())
            }),
            Duration::from_millis(30),
        );
        assert!(result.unwrap_err().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
        release.send(()).unwrap();

        let mut opener = FolderOpener::default();
        let (release, wait) = mpsc::channel();
        let wait = std::sync::Mutex::new(wait);
        opener.start(
            PathBuf::new(),
            egui::Context::default(),
            Arc::new(move |_, _, _| {
                wait.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap();
                Ok(())
            }),
        );
        opener.start(
            PathBuf::new(),
            egui::Context::default(),
            Arc::new(|_, _, _| panic!("duplicate launch")),
        );
        opener.cancel();
        assert_eq!(finish(&mut opener), None);
        let _ = release.send(());
        opener.start(
            PathBuf::new(),
            egui::Context::default(),
            Arc::new(|_, _, _| panic!("launch after close")),
        );
        assert!(!opener.is_pending());
    }

    #[test]
    fn invalid_paths_never_reach_the_file_manager() {
        let cancel = CancellationToken::default();
        let deadline = Instant::now() + TIMEOUT;
        assert!(open_directory(PathBuf::from("relative"), &cancel, deadline).is_err());
        let missing = tempfile::tempdir().unwrap();
        assert!(open_directory(missing.path().join("not-created"), &cancel, deadline).is_err());
        assert!(!missing.path().join("not-created").exists());
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(open_directory(file.path().to_owned(), &cancel, deadline).is_err());
    }
}
