// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Atomic file publication with explicit permission to replace a destination.

use std::io::{self, Write as _};
use std::path::Path;

/// Write and sync a temporary file beside the destination, then publish it.
/// An absent destination remains protected from competing writers unless
/// `overwrite` is true. Temporary files are removed on failure.
pub fn write(path: &Path, bytes: &[u8], overwrite: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    let result = if overwrite {
        temporary.persist(path)
    } else {
        temporary.persist_noclobber(path)
    };
    result.map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn entries(path: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn writes_exact_bytes_and_replaces_only_when_requested() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("measurement.csv");
        let first = b"freq_hz,loss_db\n5000,-2.5\n";
        let second = b"freq_hz,loss_db\n7000000200,-1e-9\n";
        write(&path, first, false).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), first);
        assert!(write(&path, second, false).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), first);
        write(&path, second, true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), second);
        assert_eq!(entries(directory.path()), vec![path.file_name().unwrap()]);
        write(&path, b"", true).unwrap();
        assert!(std::fs::read(&path).unwrap().is_empty());
    }

    #[test]
    fn a_destination_created_after_a_prior_check_is_not_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("measurement.xlsx");
        assert!(!path.try_exists().unwrap());
        std::fs::write(&path, b"created by another writer").unwrap();
        assert!(write(&path, b"our document", false).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"created by another writer");
        assert_eq!(entries(directory.path()), vec![path.file_name().unwrap()]);
    }

    #[test]
    fn concurrent_no_clobber_writers_publish_exactly_one_complete_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("measurement.bin");
        let barrier = Arc::new(Barrier::new(5));
        let writers: Vec<_> = (1..=4u8)
            .map(|value| {
                let barrier = barrier.clone();
                let path = path.clone();
                std::thread::spawn(move || {
                    let bytes = vec![value; 64 * 1024];
                    barrier.wait();
                    (value, write(&path, &bytes, false))
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = writers
            .into_iter()
            .map(|writer| writer.join().unwrap())
            .collect();
        let winners: Vec<_> = results
            .iter()
            .filter(|(_, result)| result.is_ok())
            .collect();
        assert_eq!(winners.len(), 1);
        for (_, result) in &results {
            if let Err(error) = result {
                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            }
        }
        assert_eq!(std::fs::read(&path).unwrap(), vec![winners[0].0; 64 * 1024]);
        assert_eq!(entries(directory.path()), vec![path.file_name().unwrap()]);
    }

    #[test]
    fn missing_parent_and_directory_destination_leave_existing_data_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing").join("measurement.csv");
        assert!(write(&missing, b"data", false).is_err());
        assert!(entries(directory.path()).is_empty());
        let target = directory.path().join("measurement.xlsx");
        std::fs::create_dir(&target).unwrap();
        let sentinel = target.join("keep.txt");
        std::fs::write(&sentinel, b"keep").unwrap();
        for overwrite in [false, true] {
            assert!(write(&target, b"data", overwrite).is_err());
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
            assert_eq!(entries(directory.path()), vec![target.file_name().unwrap()]);
            assert_eq!(entries(&target), vec![sentinel.file_name().unwrap()]);
        }
    }
}
