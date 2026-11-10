//! Running the engine tests on either chat store backend.
//!
//! Tests build engines as usual (in memory, or at a `Paths`) and run on SQLite. Wrapped in
//! [`on_folder`] (see [`folder_too!`]), every engine the test starts on its thread uses the
//! folder backend instead: at `<data>/chats-folder` when it has paths, else a temp folder
//! standing in for "in memory".

use std::cell::RefCell;
use std::path::PathBuf;

use byob_store::{ChatStore, Paths, Settings, StorageKind};

use crate::EngineConfig;

thread_local! {
    /// Set while a test runs on the folder backend: the folder for engines without paths.
    static FOLDER: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    /// Engines this thread started on the folder backend.
    static OPENED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Called when an engine opened its store (tests only).
pub(crate) fn opened(kind: StorageKind) {
    if kind == StorageKind::Folder {
        OPENED.with(|n| n.set(n.get() + 1));
    }
}

/// The backend this test runs on.
pub(crate) fn backend() -> StorageKind {
    if FOLDER.with(|f| f.borrow().is_some()) { StorageKind::Folder } else { StorageKind::Sqlite }
}

/// Called by `EngineHandle::spawn_with` (tests only). Leaves a config that already picked
/// a backend alone.
pub(crate) fn applied(mut config: EngineConfig) -> EngineConfig {
    if config.storage != StorageKind::Sqlite || config.chats_folder.is_some() {
        return config;
    }
    FOLDER.with(|f| {
        if let Some(dir) = &*f.borrow() {
            config.storage = StorageKind::Folder;
            if config.paths.is_none() {
                config.chats_folder = Some(dir.clone());
            }
        }
    });
    config
}

/// Open the store an engine at `paths` uses in this test (to inspect or seed it).
pub(crate) fn open(paths: &Paths) -> Box<dyn ChatStore> {
    paths.ensure().unwrap();
    byob_store::open_backend(&Settings { storage: backend(), ..Settings::default() }, paths).unwrap()
}

/// Run `test` with every engine it starts on the folder backend.
pub(crate) fn on_folder(test: fn()) {
    let dir = tempfile::tempdir().unwrap();
    FOLDER.with(|f| *f.borrow_mut() = Some(dir.path().join("chats")));
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            FOLDER.with(|f| *f.borrow_mut() = None);
        }
    }
    let _reset = Reset;
    OPENED.with(|n| n.set(0));
    test();
    assert!(OPENED.with(std::cell::Cell::get) > 0, "the test started no engine on the folder backend");
}

/// `folder_too!(a, b)`: also run tests `a` and `b` (defined in this module) on the folder
/// backend, as `on_folder::a` and `on_folder::b`.
macro_rules! folder_too {
    ($($name:ident),* $(,)?) => {
        mod on_folder {
            $( #[test] fn $name() { crate::test_store::on_folder(super::$name) } )*
        }
    };
}
pub(crate) use folder_too;
