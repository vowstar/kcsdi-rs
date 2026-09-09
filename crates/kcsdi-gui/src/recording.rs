// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Bounded recording writes and retention confined to one fresh run directory.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};

use kcsdi_core::atomic_file;
use kcsdi_core::control::CancellationToken;
use same_file::Handle;
use sha2::{Digest, Sha256};

use crate::run_settings::{RecordingFormat, RecordingSettings, Retention, RunSettings};
use crate::spreadsheet::FrozenSnapshots;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordKey {
    pub session_id: u64,
    pub request_id: u64,
    pub pass_id: u64,
}

#[derive(Debug)]
pub struct RecordResult {
    pub key: RecordKey,
    /// Cancellation before publication returns no path. Published files remain saved.
    pub result: Result<Option<PathBuf>, String>,
}

struct RecordTask {
    key: RecordKey,
    settings: RecordingSettings,
    snapshots: FrozenSnapshots,
    cancel: CancellationToken,
}

/// At most one accepted task exists, including a result not yet polled.
pub struct RecordWriter {
    sender: Option<SyncSender<RecordTask>>,
    results: Receiver<RecordResult>,
    thread: Option<JoinHandle<()>>,
    pending: Option<RecordKey>,
}

impl RecordWriter {
    pub fn new() -> Result<Self, String> {
        let mut store = RecordStore::default();
        Self::spawn(move |task| store.save(task))
    }

    #[cfg(test)]
    pub(crate) fn with_test_save(
        mut save: impl FnMut(RecordKey) -> Result<Option<PathBuf>, String> + Send + 'static,
    ) -> Result<Self, String> {
        Self::spawn(move |task| save(task.key))
    }

    fn spawn(
        mut save: impl FnMut(RecordTask) -> Result<Option<PathBuf>, String> + Send + 'static,
    ) -> Result<Self, String> {
        let (sender, jobs) = mpsc::sync_channel::<RecordTask>(1);
        let (results_tx, results) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("recording-writer".into())
            .spawn(move || {
                while let Ok(task) = jobs.recv() {
                    let key = task.key;
                    let result = save(task);
                    if results_tx.send(RecordResult { key, result }).is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| format!("cannot start recording writer: {error}"))?;
        Ok(Self {
            sender: Some(sender),
            results,
            thread: Some(thread),
            pending: None,
        })
    }

    pub fn try_save(
        &mut self,
        key: RecordKey,
        settings: RecordingSettings,
        snapshots: FrozenSnapshots,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        if self.pending.is_some() {
            return Err("a recording write is already pending".into());
        }
        validate_settings(key, &settings)?;
        self.sender
            .as_ref()
            .ok_or("recording writer has closed")?
            .try_send(RecordTask {
                key,
                settings,
                snapshots,
                cancel,
            })
            .map_err(|error| format!("cannot queue recording write: {error}"))?;
        self.pending = Some(key);
        Ok(())
    }

    pub fn poll(&mut self) -> Option<RecordResult> {
        match self.results.try_recv() {
            Ok(result) => {
                self.pending = None;
                Some(result)
            }
            Err(TryRecvError::Disconnected) => self.pending.take().map(|key| RecordResult {
                key,
                result: Err("recording writer stopped before acknowledging the write".into()),
            }),
            Err(TryRecvError::Empty) => None,
        }
    }
}

impl Drop for RecordWriter {
    fn drop(&mut self) {
        self.sender.take();
        // Filesystem calls are not interruptible. The device must be closed first.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn validate_settings(key: RecordKey, settings: &RecordingSettings) -> Result<(), String> {
    if key.session_id == 0 || key.request_id == 0 || key.pass_id == 0 {
        return Err("recording identity must be nonzero".into());
    }
    if !settings.enabled {
        return Err("recording requires an enabled, selected directory".into());
    }
    RunSettings {
        recording: settings.clone(),
        ..RunSettings::default()
    }
    .validate()
}

#[derive(Default)]
struct RecordStore {
    run: Option<RunDirectory>,
}

impl RecordStore {
    fn save(&mut self, task: RecordTask) -> Result<Option<PathBuf>, String> {
        self.save_using(task, atomic_file::write)
    }

    fn save_using(
        &mut self,
        task: RecordTask,
        write: impl FnOnce(&Path, &[u8], bool) -> io::Result<()>,
    ) -> Result<Option<PathBuf>, String> {
        validate_settings(task.key, &task.settings)?;
        if task.cancel.is_cancelled() {
            return Ok(None);
        }
        let bytes = match task.settings.format {
            RecordingFormat::Csv => task.snapshots.csv_bytes(),
            RecordingFormat::Xlsx => task.snapshots.xlsx_bytes(),
        }?;
        if task.cancel.is_cancelled() {
            return Ok(None);
        }
        if self.run.as_ref().is_none_or(|run| {
            run.session_id != task.key.session_id || run.request_id != task.key.request_id
        }) {
            // Dropping the old ledger releases handles, never removes its files.
            self.run = None;
            self.run = Some(RunDirectory::new(task.key, task.settings.clone())?);
        }
        let run = self.run.as_mut().expect("recording run initialized");
        if run.settings != task.settings {
            return Err("recording settings changed without a new request".into());
        }
        if run.failed {
            return Err("recording run has stopped".into());
        }
        let result = run.save(task.key.pass_id, &bytes, &task.cancel, write);
        if result.is_err() || task.cancel.is_cancelled() {
            run.failed = true;
        }
        result
    }
}

struct RunDirectory {
    session_id: u64,
    request_id: u64,
    settings: RecordingSettings,
    root: CheckedDirectory,
    child: CheckedDirectory,
    last_pass_id: u64,
    files: VecDeque<OwnedFile>,
    failed: bool,
}

impl RunDirectory {
    fn new(key: RecordKey, settings: RecordingSettings) -> Result<Self, String> {
        let root = CheckedDirectory::open(&settings.directory)?;
        root.check()?;
        let child_path = tempfile::Builder::new()
            .prefix("recording-")
            .tempdir_in(&root.canonical)
            .map_err(|error| format!("cannot create recording run directory: {error}"))?
            .keep();
        let child = CheckedDirectory::open(&child_path)?;
        root.check()?;
        Ok(Self {
            session_id: key.session_id,
            request_id: key.request_id,
            settings,
            root,
            child,
            last_pass_id: 0,
            files: VecDeque::new(),
            failed: false,
        })
    }

    fn check_directories(&self) -> Result<(), String> {
        self.root.check()?;
        self.child.check()
    }

    fn save(
        &mut self,
        pass_id: u64,
        bytes: &[u8],
        cancel: &CancellationToken,
        write: impl FnOnce(&Path, &[u8], bool) -> io::Result<()>,
    ) -> Result<Option<PathBuf>, String> {
        if pass_id <= self.last_pass_id {
            return Err("recording pass is repeated or out of order".into());
        }
        self.check_directories()?;
        if cancel.is_cancelled() {
            return Ok(None);
        }
        let path = self.child.canonical.join(format!(
            "pass-{pass_id:020}.{}",
            self.settings.format.extension()
        ));
        write(&path, bytes, false)
            .map_err(|error| format!("cannot save recording {}: {error}", path.display()))?;
        self.last_pass_id = pass_id;
        self.check_directories()?;
        if let Retention::KeepLast(count) = self.settings.retention {
            self.files.push_back(OwnedFile::capture(&path, bytes)?);
            // A cancelled save may already be published. Preserve it and skip pruning.
            if !cancel.is_cancelled() {
                self.prune(count as usize, cancel)?;
            }
        }
        Ok(Some(path))
    }

    fn prune(&mut self, count: usize, cancel: &CancellationToken) -> Result<(), String> {
        while self.files.len() > count && !cancel.is_cancelled() {
            self.check_directories()?;
            let file = self.files.front().expect("nonempty retention ledger");
            let checked = file.check()?;
            self.check_directories()?;
            if cancel.is_cancelled() {
                break;
            }
            // Portable path APIs cannot make identity-check and unlink atomic.
            // Refuse observed changes, retain the checked handle until after unlink.
            fs::remove_file(&file.path).map_err(|error| {
                format!(
                    "cannot remove recorded file {}: {error}",
                    file.path.display()
                )
            })?;
            drop(checked);
            self.files.pop_front();
        }
        Ok(())
    }
}

struct CheckedDirectory {
    original: PathBuf,
    canonical: PathBuf,
    identity: Handle,
}

impl CheckedDirectory {
    fn open(path: &Path) -> Result<Self, String> {
        let original = path.to_owned();
        require_directory(&original)?;
        let canonical = fs::canonicalize(&original).map_err(|error| error.to_string())?;
        let identity = Handle::from_path(&canonical).map_err(|error| error.to_string())?;
        let directory = Self {
            original,
            canonical,
            identity,
        };
        directory.check()?;
        Ok(directory)
    }

    fn check(&self) -> Result<(), String> {
        require_directory(&self.original)?;
        let canonical = fs::canonicalize(&self.original).map_err(|error| error.to_string())?;
        if canonical != self.canonical
            || Handle::from_path(&canonical).map_err(|error| error.to_string())? != self.identity
        {
            return Err(format!(
                "recording directory identity changed: {}",
                self.original.display()
            ));
        }
        Ok(())
    }
}

fn require_directory(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "cannot inspect recording directory {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "recording directory is not a real directory: {}",
            path.display()
        ));
    }
    Ok(())
}

struct OwnedFile {
    path: PathBuf,
    identity: Handle,
    length: u64,
    digest: [u8; 32],
}

impl OwnedFile {
    fn capture(path: &Path, bytes: &[u8]) -> Result<Self, String> {
        let identity = open_regular_file(path)?;
        let file = Self {
            path: path.to_owned(),
            identity,
            length: bytes.len() as u64,
            digest: Sha256::digest(bytes).into(),
        };
        file.check()?;
        Ok(file)
    }

    fn check(&self) -> Result<Handle, String> {
        let handle = open_regular_file(&self.path)?;
        if handle != self.identity
            || handle
                .as_file()
                .metadata()
                .map_err(|error| error.to_string())?
                .len()
                != self.length
            || file_digest(&handle, self.length)? != self.digest
        {
            return Err(format!(
                "recorded file identity or contents changed: {}",
                self.path.display()
            ));
        }
        Ok(handle)
    }
}

fn open_regular_file(path: &Path) -> Result<Handle, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "recorded path is not a regular file: {}",
            path.display()
        ));
    }
    let handle = Handle::from_path(path).map_err(|error| error.to_string())?;
    if !handle
        .as_file()
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err(format!("recorded path changed: {}", path.display()));
    }
    Ok(handle)
}

fn file_digest(handle: &Handle, length: u64) -> Result<[u8; 32], String> {
    let mut reader = handle.as_file().take(length.saturating_add(1));
    let mut digest = Sha256::new();
    let mut buffer = [0; 8192];
    let mut read_length = 0;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        read_length += count as u64;
        digest.update(&buffer[..count]);
    }
    if read_length != length {
        return Err("recorded file length changed while checking its contents".into());
    }
    Ok(digest.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::Arc;
    use std::time::{Duration, Instant, UNIX_EPOCH};

    use calamine::{Reader, Xlsx};
    use kcsdi_core::data::{SweepData, SweepPoint};

    use crate::acquisition::{AcquisitionSettings, CompletedSweep, TraceId};
    use crate::run_settings::MAX_RETAINED_FILES;

    fn key(pass_id: u64) -> RecordKey {
        RecordKey {
            session_id: 1,
            request_id: 2,
            pass_id,
        }
    }

    fn settings(directory: &Path, retention: Retention) -> RecordingSettings {
        RecordingSettings {
            enabled: true,
            directory: directory.to_owned(),
            format: RecordingFormat::Csv,
            retention,
        }
    }

    fn snapshots() -> FrozenSnapshots {
        let conditions = [
            AcquisitionSettings::S11(crate::acquisition::tests::s11()),
            AcquisitionSettings::S21(crate::acquisition::tests::s21()),
            AcquisitionSettings::Spec(crate::acquisition::tests::spec()),
        ];
        FrozenSnapshots::new(
            conditions
                .into_iter()
                .enumerate()
                .map(|(index, settings)| {
                    let width = kcsdi_core::table::columns(settings.mode(), settings.format())
                        .unwrap()
                        .len();
                    let data = SweepData {
                        mode: settings.mode(),
                        format: settings.format().to_owned(),
                        points: [1_000_000.25, 1_456_789.5, 2_000_000.75]
                            .into_iter()
                            .map(|freq_hz| SweepPoint {
                                freq_hz,
                                values: vec![index as f64 - 1.5; width],
                            })
                            .collect(),
                    };
                    (
                        TraceId(index as u64 + 1),
                        Arc::new(CompletedSweep {
                            data,
                            settings,
                            session_id: 1,
                            completed_at: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                        }),
                    )
                })
                .collect(),
        )
        .unwrap()
    }

    fn task(settings: &RecordingSettings, pass_id: u64) -> RecordTask {
        RecordTask {
            key: key(pass_id),
            settings: settings.clone(),
            snapshots: snapshots(),
            cancel: CancellationToken::default(),
        }
    }

    fn save(store: &mut RecordStore, settings: &RecordingSettings, pass_id: u64) -> PathBuf {
        store.save(task(settings, pass_id)).unwrap().unwrap()
    }

    fn await_result(writer: &mut RecordWriter) -> RecordResult {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = writer.poll() {
                return result;
            }
            assert!(
                Instant::now() < deadline,
                "recording acknowledgement timed out"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn capacity_one_includes_unread_acknowledgement() {
        let directory = tempfile::tempdir().unwrap();
        let settings = settings(directory.path(), Retention::KeepAll);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut writer = RecordWriter::spawn(move |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(None)
        })
        .unwrap();
        writer
            .try_save(
                key(1),
                settings.clone(),
                snapshots(),
                CancellationToken::default(),
            )
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let while_running = writer.try_save(
            key(2),
            settings.clone(),
            snapshots(),
            CancellationToken::default(),
        );
        release_tx.send(()).unwrap();
        assert!(while_running.is_err());
        // Joining the worker after closing its sender proves the result is queued.
        writer.sender.take();
        writer.thread.take().unwrap().join().unwrap();
        assert!(
            writer
                .try_save(key(2), settings, snapshots(), CancellationToken::default(),)
                .unwrap_err()
                .contains("pending")
        );
        let result = await_result(&mut writer);
        assert_eq!(result.key, key(1));
        assert_eq!(result.result.unwrap(), None);
        assert!(writer.pending.is_none());
    }

    #[test]
    fn cancelled_task_is_acknowledged_without_creating_a_directory() {
        let directory = tempfile::tempdir().unwrap();
        let settings = settings(directory.path(), Retention::KeepAll);
        let cancel = CancellationToken::default();
        cancel.cancel();
        let mut writer = RecordWriter::new().unwrap();
        writer
            .try_save(key(1), settings.clone(), snapshots(), cancel)
            .unwrap();
        let result = await_result(&mut writer);
        assert_eq!(result.key, key(1));
        assert_eq!(result.result.unwrap(), None);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        writer
            .try_save(key(2), settings, snapshots(), CancellationToken::default())
            .unwrap();
        assert!(await_result(&mut writer).result.unwrap().unwrap().is_file());
    }

    #[test]
    fn cancellation_after_queueing_still_acknowledges_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let settings = settings(directory.path(), Retention::KeepAll);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut store = RecordStore::default();
        let mut writer = RecordWriter::spawn(move |task| {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            store.save(task)
        })
        .unwrap();
        let cancel = CancellationToken::default();
        writer
            .try_save(key(1), settings, snapshots(), cancel.clone())
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        cancel.cancel();
        release_tx.send(()).unwrap();
        let result = await_result(&mut writer);
        assert_eq!(result.key, key(1));
        assert_eq!(result.result.unwrap(), None);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn writer_failure_acknowledges_pending_key() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = RecordWriter::spawn(|_| panic!("simulated writer failure")).unwrap();
        writer
            .try_save(
                key(1),
                settings(directory.path(), Retention::KeepAll),
                snapshots(),
                CancellationToken::default(),
            )
            .unwrap();
        let result = await_result(&mut writer);
        assert_eq!(result.key, key(1));
        assert!(result.result.unwrap_err().contains("acknowledging"));
        assert!(writer.poll().is_none());
    }

    #[test]
    fn csv_and_xlsx_contain_every_trace_and_actual_frequency() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = settings(directory.path(), Retention::KeepAll);
        let mut store = RecordStore::default();
        let csv_path = save(&mut store, &settings, 1);
        let mut csv = csv::Reader::from_path(csv_path).unwrap();
        let rows = csv.records().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(rows.len(), 15);
        assert_eq!(&rows[0][2], "1000000.25");
        assert_eq!(&rows[3][2], "1456789.5");
        assert_eq!(&rows[12][7], "spec");
        assert_eq!(&rows[0][16], "1700000000.000000000");
        assert_eq!(&rows[0][17], "1");
        settings.format = RecordingFormat::Xlsx;
        let mut next = task(&settings, 1);
        next.key.request_id += 1;
        let path = store.save(next).unwrap().unwrap();
        let mut book: Xlsx<_> = Xlsx::new(Cursor::new(fs::read(path).unwrap())).unwrap();
        assert_eq!(book.sheet_names(), ["T1", "T2", "T3", "Metadata"]);
        let trace = book.worksheet_range("T2").unwrap();
        assert_eq!(
            trace.get((1, 0)),
            Some(&calamine::Data::Float(1_000_000.25))
        );
        assert_eq!(trace.get((1, 1)), Some(&calamine::Data::Float(-0.5)));
    }

    #[test]
    fn runs_are_isolated_and_keep_all_has_no_file_ledger() {
        let directory = tempfile::tempdir().unwrap();
        let settings = settings(directory.path(), Retention::KeepAll);
        let mut store = RecordStore::default();
        let first = save(&mut store, &settings, 1);
        for pass in 2..40 {
            let path = save(&mut store, &settings, pass);
            assert_eq!(path.parent(), first.parent());
            assert!(store.run.as_ref().unwrap().files.is_empty());
        }
        let mut next = task(&settings, 1);
        next.key.request_id += 1;
        let second = store.save(next).unwrap().unwrap();
        let mut next = task(&settings, 1);
        next.key.session_id += 1;
        let third = store.save(next).unwrap().unwrap();
        assert_ne!(first.parent(), second.parent());
        assert_ne!(second.parent(), third.parent());
        assert!(first.is_file() && second.is_file() && third.is_file());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 3);
    }

    #[test]
    fn retention_removes_only_registered_files_after_success() {
        let directory = tempfile::tempdir().unwrap();
        let settings = settings(directory.path(), Retention::KeepLast(2));
        let mut store = RecordStore::default();
        let first = save(&mut store, &settings, 1);
        let unrelated = first.parent().unwrap().join("unrelated.csv");
        fs::write(&unrelated, b"keep me").unwrap();
        let second = save(&mut store, &settings, 2);
        let third = save(&mut store, &settings, 3);
        assert!(!first.exists());
        assert!(second.is_file() && third.is_file());
        assert_eq!(fs::read(unrelated).unwrap(), b"keep me");
        assert_eq!(store.run.as_ref().unwrap().files.len(), 2);
        for pass in 4..20 {
            save(&mut store, &settings, pass);
            assert_eq!(store.run.as_ref().unwrap().files.len(), 2);
        }
    }

    #[test]
    fn invalid_retention_and_settings_changes_do_not_write() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RecordStore::default();
        for count in [0, MAX_RETAINED_FILES + 1] {
            let invalid = settings(directory.path(), Retention::KeepLast(count));
            assert!(store.save(task(&invalid, 1)).is_err());
        }
        let relative = settings(Path::new("relative-recording"), Retention::KeepAll);
        assert!(store.save(task(&relative, 1)).is_err());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        let mut settings = settings(directory.path(), Retention::KeepAll);
        let first = save(&mut store, &settings, 1);
        settings.retention = Retention::KeepLast(1);
        assert!(
            store
                .save(task(&settings, 2))
                .unwrap_err()
                .contains("settings changed")
        );
        assert!(first.is_file());
        assert_eq!(fs::read_dir(first.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn collision_never_overwrites_or_prunes() {
        let directory = tempfile::tempdir().unwrap();
        let settings = settings(directory.path(), Retention::KeepLast(1));
        let mut store = RecordStore::default();
        let first = save(&mut store, &settings, 1);
        let collision = first
            .parent()
            .unwrap()
            .join("pass-00000000000000000002.csv");
        fs::write(&collision, b"unrelated collision").unwrap();
        assert!(store.save(task(&settings, 2)).is_err());
        assert!(first.is_file());
        assert_eq!(fs::read(&collision).unwrap(), b"unrelated collision");
        assert_eq!(fs::read_dir(first.parent().unwrap()).unwrap().count(), 2);
    }

    #[test]
    fn disk_full_preserves_old_files_and_does_not_prune() {
        let directory = tempfile::tempdir().unwrap();
        let settings = settings(directory.path(), Retention::KeepLast(1));
        let mut store = RecordStore::default();
        let first = save(&mut store, &settings, 1);
        let error = store
            .save_using(task(&settings, 2), |_, _, overwrite| {
                assert!(!overwrite);
                Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "simulated full disk",
                ))
            })
            .unwrap_err();
        assert!(error.contains("simulated full disk"));
        assert!(first.is_file());
        assert_eq!(fs::read_dir(first.parent().unwrap()).unwrap().count(), 1);
        assert_eq!(store.run.as_ref().unwrap().files.len(), 1);
    }

    #[test]
    fn changed_contents_refuse_pruning_but_keep_new_file() {
        let directory = tempfile::tempdir().unwrap();
        let settings = settings(directory.path(), Retention::KeepLast(1));
        let mut store = RecordStore::default();
        let first = save(&mut store, &settings, 1);
        let mut changed = fs::read(&first).unwrap();
        changed[0] ^= 1;
        fs::write(&first, &changed).unwrap();
        assert!(
            store
                .save(task(&settings, 2))
                .unwrap_err()
                .contains("contents changed")
        );
        assert_eq!(fs::read(&first).unwrap(), changed);
        assert!(
            first
                .parent()
                .unwrap()
                .join("pass-00000000000000000002.csv")
                .is_file()
        );
        assert_eq!(store.run.as_ref().unwrap().files.len(), 2);
        assert!(
            store
                .save(task(&settings, 3))
                .unwrap_err()
                .contains("stopped")
        );
    }

    #[test]
    fn changed_new_file_is_not_registered_and_does_not_prune_old_data() {
        let directory = tempfile::tempdir().unwrap();
        let settings = settings(directory.path(), Retention::KeepLast(1));
        let mut store = RecordStore::default();
        let first = save(&mut store, &settings, 1);
        let result = store.save_using(task(&settings, 2), |path, bytes, overwrite| {
            atomic_file::write(path, bytes, overwrite)?;
            let mut changed = bytes.to_vec();
            changed[0] ^= 1;
            fs::write(path, changed)
        });
        assert!(result.unwrap_err().contains("contents changed"));
        assert!(first.is_file());
        assert_eq!(store.run.as_ref().unwrap().files.len(), 1);
        assert_eq!(fs::read_dir(first.parent().unwrap()).unwrap().count(), 2);
    }

    #[test]
    fn identical_replacement_is_not_an_owned_file() {
        let directory = tempfile::tempdir().unwrap();
        let settings = settings(directory.path(), Retention::KeepLast(1));
        let mut store = RecordStore::default();
        let first = save(&mut store, &settings, 1);
        let bytes = fs::read(&first).unwrap();
        let moved = first.with_extension("moved");
        fs::rename(&first, &moved).unwrap();
        fs::write(&first, &bytes).unwrap();
        assert!(
            store
                .save(task(&settings, 2))
                .unwrap_err()
                .contains("identity")
        );
        assert_eq!(fs::read(first).unwrap(), bytes);
        assert_eq!(fs::read(moved).unwrap(), bytes);
    }

    #[test]
    fn directory_replacement_is_rejected_or_blocks_publication() {
        for replace_root in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("root");
            fs::create_dir(&root).unwrap();
            let settings = settings(&root, Retention::KeepLast(1));
            let mut store = RecordStore::default();
            let first = save(&mut store, &settings, 1);
            let target = if replace_root {
                root.clone()
            } else {
                first.parent().unwrap().to_owned()
            };
            let moved = target.with_extension("moved");
            if let Err(error) = fs::rename(&target, &moved) {
                // Windows can reject moving a directory with open descendants.
                // In that case the attempted replacement never happened.
                assert!(
                    cfg!(windows) && error.kind() == std::io::ErrorKind::PermissionDenied,
                    "unexpected directory rename failure: {error}"
                );
                assert!(target.is_dir() && !moved.exists());
                assert!(first.is_file());
                let run = store.run.as_ref().unwrap();
                run.root.check().unwrap();
                run.child.check().unwrap();
                let second = save(&mut store, &settings, 2);
                assert!(second.is_file());
                assert!(!first.exists());
                continue;
            }
            fs::create_dir(&target).unwrap();
            assert!(
                store
                    .save(task(&settings, 2))
                    .unwrap_err()
                    .contains("identity")
            );
            assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
            assert!(moved.is_dir());
        }
    }

    #[test]
    fn cancellation_after_publication_keeps_new_and_old_files() {
        let directory = tempfile::tempdir().unwrap();
        let settings = settings(directory.path(), Retention::KeepLast(1));
        let mut store = RecordStore::default();
        let first = save(&mut store, &settings, 1);
        let pending = task(&settings, 2);
        let cancel = pending.cancel.clone();
        let second = store
            .save_using(pending, |path, bytes, overwrite| {
                atomic_file::write(path, bytes, overwrite)?;
                cancel.cancel();
                Ok(())
            })
            .unwrap()
            .unwrap();
        assert!(first.is_file() && second.is_file());
        assert_eq!(store.run.as_ref().unwrap().files.len(), 2);
        assert!(
            store
                .save(task(&settings, 3))
                .unwrap_err()
                .contains("stopped")
        );
    }

    #[test]
    fn repeated_or_out_of_order_pass_cannot_reuse_a_pruned_name() {
        let directory = tempfile::tempdir().unwrap();
        let settings = settings(directory.path(), Retention::KeepLast(1));
        let mut store = RecordStore::default();
        let first = save(&mut store, &settings, 1);
        let second = save(&mut store, &settings, 2);
        assert!(!first.exists());
        assert!(
            store
                .save(task(&settings, 1))
                .unwrap_err()
                .contains("out of order")
        );
        assert!(!first.exists());
        assert!(second.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_boundaries_never_touch_external_files() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let external = directory.path().join("external");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&external).unwrap();
        let alias = directory.path().join("alias");
        symlink(&root, &alias).unwrap();
        let mut store = RecordStore::default();
        let invalid = settings(&alias, Retention::KeepAll);
        assert!(store.save(task(&invalid, 1)).is_err());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);

        let settings = settings(&root, Retention::KeepLast(1));
        let first = save(&mut store, &settings, 1);
        let protected = external.join("protected.csv");
        let bytes = fs::read(&first).unwrap();
        fs::write(&protected, &bytes).unwrap();
        fs::remove_file(&first).unwrap();
        symlink(&protected, &first).unwrap();
        assert!(
            store
                .save(task(&settings, 2))
                .unwrap_err()
                .contains("regular file")
        );
        assert_eq!(fs::read(&protected).unwrap(), bytes);
        assert!(
            fs::symlink_metadata(&first)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        let mut next = task(&settings, 1);
        next.key.request_id += 1;
        let next = store.save(next).unwrap().unwrap();
        let child = next.parent().unwrap();
        fs::rename(child, child.with_extension("moved")).unwrap();
        symlink(&external, child).unwrap();
        let mut next = task(&settings, 2);
        next.key.request_id += 1;
        assert!(store.save(next).unwrap_err().contains("real directory"));
        assert_eq!(fs::read_dir(&external).unwrap().count(), 1);
        assert_eq!(fs::read(protected).unwrap(), bytes);
    }
}
