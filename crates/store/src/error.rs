use std::path::PathBuf;

use rusqlite::ErrorCode;

/// Everything that can go wrong in this crate. Nothing here panics; callers decide whether
/// an error is fatal (e.g. a corrupt database at startup) or just worth a notice.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The file is not a SQLite database, or its pages are damaged.
    #[error("database {path} is corrupt or not a byob database: {detail}")]
    Corrupt { path: PathBuf, detail: String },
    /// A newer byob wrote this database; we refuse to touch it rather than lose data.
    #[error("database schema version {found} is newer than this byob supports ({supported}); upgrade byob")]
    TooNew { found: i64, supported: i64 },
    /// A row we were asked to change does not exist.
    #[error("{0} not found")]
    NotFound(String),
    /// Something we were asked to create already exists (e.g. a chat id).
    #[error("{0} already exists")]
    Exists(String),
    /// A stored value could not be decoded (bad JSON, unknown enum text, …).
    #[error("bad stored value in {column}: {detail}")]
    BadValue { column: &'static str, detail: String },
    /// Any other SQLite failure (I/O, locked past the busy timeout, constraint, …).
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// A value from a user or client that can't be used (an empty name, …).
    #[error("{0}")]
    Invalid(String),
    /// Neither `BYOB_HOME` nor a platform home directory is available.
    #[error("no home directory found; set BYOB_HOME")]
    NoHome,
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

impl StoreError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io { path: path.into(), source }
    }

    /// Turn "file is not a database"/"malformed" SQLite errors into [`StoreError::Corrupt`].
    pub(crate) fn classify(e: rusqlite::Error, path: &std::path::Path) -> Self {
        match e.sqlite_error_code() {
            Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt) => Self::Corrupt { path: path.to_owned(), detail: e.to_string() },
            _ => Self::Sqlite(e),
        }
    }

    /// True for errors that mean the database file itself is unusable.
    pub fn is_corrupt(&self) -> bool {
        match self {
            Self::Corrupt { .. } => true,
            Self::Sqlite(e) => matches!(e.sqlite_error_code(), Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt)),
            _ => false,
        }
    }
}
