//! Undo/redo history: a copy of the document before each edit.
//! Small copies stay in memory; large ones are saved to temporary files so
//! that editing a heavy PDF doesn't fill up memory.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Copies larger than this go to disk.
const MEMORY_LIMIT: usize = 2 * 1024 * 1024;
/// Maximum number of steps that can be undone.
const LIMIT: usize = 100;
const DIR_PREFIX: &str = "pliego-historial-";

static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

enum Snapshot {
    Memory(Vec<u8>),
    Disk(PathBuf),
}

fn dir() -> PathBuf {
    std::env::temp_dir().join(format!("{DIR_PREFIX}{}", std::process::id()))
}

impl Snapshot {
    fn new(bytes: Vec<u8>) -> Snapshot {
        if bytes.len() <= MEMORY_LIMIT {
            return Snapshot::Memory(bytes);
        }
        let dir = dir();
        let path = dir.join(format!("{}.pdf", NEXT_FILE.fetch_add(1, Ordering::Relaxed)));
        // If the file can't be written, keep it in memory anyway.
        match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, &bytes)) {
            Ok(()) => Snapshot::Disk(path),
            Err(_) => Snapshot::Memory(bytes),
        }
    }

    fn bytes(&mut self) -> Result<Vec<u8>, String> {
        match self {
            Snapshot::Memory(bytes) => Ok(std::mem::take(bytes)),
            Snapshot::Disk(path) => std::fs::read(&*path).map_err(|e| format!("no se pudo leer el historial: {e}")),
        }
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        if let Snapshot::Disk(path) = self {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Default)]
pub(crate) struct History {
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
}

impl History {
    pub(crate) fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub(crate) fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Saves the state before a new edit (and discards what could be redone).
    pub(crate) fn record(&mut self, before: Vec<u8>) {
        self.undo.push(Snapshot::new(before));
        if self.undo.len() > LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Returns the previous state and keeps `current` for redo.
    pub(crate) fn undo(&mut self, current: Vec<u8>) -> Result<Vec<u8>, String> {
        let mut previous = self.undo.pop().ok_or("no hay nada que deshacer")?;
        let bytes = previous.bytes()?;
        self.redo.push(Snapshot::new(current));
        Ok(bytes)
    }

    pub(crate) fn redo(&mut self, current: Vec<u8>) -> Result<Vec<u8>, String> {
        let mut next = self.redo.pop().ok_or("no hay nada que rehacer")?;
        let bytes = next.bytes()?;
        self.undo.push(Snapshot::new(current));
        Ok(bytes)
    }
}

/// Deletes history folders left over from previous sessions (e.g. if the app closed
/// abruptly). Only old ones, in case another app window is open.
pub(crate) fn clean_old() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
    let own = dir();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path == own || !name.to_string_lossy().starts_with(DIR_PREFIX) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(2 * 24 * 3600));
        if old {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> usize {
        std::fs::read_dir(dir()).map(|d| d.count()).unwrap_or(0)
    }

    #[test]
    fn big_snapshots_go_to_disk_and_are_removed() {
        let big = vec![7u8; MEMORY_LIMIT + 1];
        let before = files();
        let mut history = History::default();
        history.record(big.clone());
        history.record(b"chico".to_vec());
        assert_eq!(files(), before + 1);

        assert_eq!(history.undo(b"actual".to_vec()).unwrap(), b"chico");
        assert_eq!(history.undo(b"chico".to_vec()).unwrap(), big);
        // The file that was read is no longer needed.
        assert_eq!(files(), before);
        assert!(history.undo(vec![]).is_err());
        assert_eq!(history.redo(big.clone()).unwrap(), b"chico");
        assert!(history.can_undo() && history.can_redo());

        // A new edit discards what could be redone.
        history.record(big);
        assert!(!history.can_redo());
        drop(history);
        assert_eq!(files(), before);
    }

    #[test]
    fn keeps_a_limited_number_of_steps() {
        let mut history = History::default();
        for i in 0..LIMIT + 10 {
            history.record(vec![i as u8]);
        }
        let mut steps = 0;
        while history.undo(vec![]).is_ok() {
            steps += 1;
        }
        assert_eq!(steps, LIMIT);
    }
}
