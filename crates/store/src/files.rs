//! Small JSON files: atomic writes and forgiving reads.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{Result, StoreError};

/// A value read from disk, plus anything the user should be told about it (e.g. "settings
/// file was corrupt; using defaults"). Loading never fails outright.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded<T> {
    pub value: T,
    pub warnings: Vec<String>,
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!("{name}{suffix}"))
}

/// Read `path` as JSON. Missing → default. Unreadable or unparsable → default plus a
/// warning; a corrupt file is first copied to `<name>.corrupt` so a later save can't
/// destroy what the user had.
pub(crate) fn load_json<T: DeserializeOwned + Default>(path: &Path) -> Loaded<T> {
    let mut warnings = Vec::new();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Loaded { value: T::default(), warnings },
        Err(e) => {
            warnings.push(format!("couldn't read {}: {e}; using defaults", path.display()));
            return Loaded { value: T::default(), warnings };
        }
    };
    match serde_json::from_str(&text) {
        Ok(value) => Loaded { value, warnings },
        Err(e) => {
            let aside = sibling(path, ".corrupt");
            let kept = match std::fs::copy(path, &aside) {
                Ok(_) => format!("kept a copy at {}", aside.display()),
                Err(e) => format!("couldn't keep a copy: {e}"),
            };
            warnings.push(format!("{} is not valid ({e}); using defaults, {kept}", path.display()));
            Loaded { value: T::default(), warnings }
        }
    }
}

/// Write `value` as pretty JSON via a temp file + rename, so readers (and crashes) only
/// ever see the old or the new file. `mode` sets unix permissions on the new file.
pub(crate) fn save_json<T: Serialize>(path: &Path, value: &T, mode: Option<u32>) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    write_atomic(path, text.as_bytes(), mode)
}

/// Write `bytes` via a temp file + rename (see [`save_json`]).
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| StoreError::io(dir, e))?;
    }
    let tmp = sibling(path, &format!(".tmp-{}", std::process::id()));
    // A stale temp file from a crash may have looser permissions; start fresh.
    let _ = std::fs::remove_file(&tmp);
    let written = write_new(&tmp, bytes, mode).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(StoreError::io(path, e));
    }
    sync_dir(path);
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Best effort: make the rename durable.
pub(crate) fn sync_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty())
        && let Ok(d) = std::fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}
