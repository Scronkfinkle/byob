mod backends;
mod db;
mod export;
mod files;
mod folder;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A fresh directory under the system temp dir, removed on drop.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("yah-store-test-{}-{nanos}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A store of either backend, on disk in its own temp dir.
pub(crate) struct Fixture {
    pub(crate) store: Box<dyn crate::ChatStore>,
    kind: crate::StorageKind,
    dir: TempDir,
}

impl Fixture {
    fn open(kind: crate::StorageKind, dir: TempDir) -> Self {
        let settings = crate::Settings { storage: kind, ..Default::default() };
        let store = crate::open_backend(&settings, &crate::Paths::at(dir.path())).unwrap();
        assert_eq!(store.kind(), kind);
        Self { store, kind, dir }
    }

    /// Close and open again from the same files.
    pub(crate) fn reopen(self) -> Self {
        let Self { store, kind, dir } = self;
        drop(store);
        Self::open(kind, dir)
    }
}

impl std::ops::Deref for Fixture {
    type Target = dyn crate::ChatStore;
    fn deref(&self) -> &Self::Target {
        &*self.store
    }
}

pub(crate) fn sqlite() -> Fixture {
    Fixture::open(crate::StorageKind::Sqlite, TempDir::new())
}

pub(crate) fn folder() -> Fixture {
    Fixture::open(crate::StorageKind::Folder, TempDir::new())
}

/// An export with backend-specific message and compaction ids zeroed, for comparing across stores.
pub(crate) fn normalized(mut e: crate::ChatExport) -> crate::ChatExport {
    for m in &mut e.messages {
        m.id = crate::MessageId(0);
        for s in &mut m.steps {
            s.message_id = crate::MessageId(0);
        }
    }
    for c in &mut e.compactions {
        c.compaction.id = 0;
        c.compaction.through = crate::MessageId(0);
    }
    e
}
