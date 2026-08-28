//! Bundled stock-SQLite implementation of br's synchronous storage facade.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

pub use fsqlite_error::FrankenError;
pub use fsqlite_types::SqliteValue;
use fsqlite_types::value::SmallText;
use rusqlite::ffi::{self, ErrorCode};
use rusqlite::types::{ToSql, ToSqlOutput, ValueRef};

/// A database row produced by a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    values: Vec<SqliteValue>,
}

impl Row {
    /// Returns all values in this row.
    #[must_use]
    pub fn values(&self) -> &[SqliteValue] {
        &self.values
    }

    /// Returns the value at `index`, if present.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&SqliteValue> {
        self.values.get(index)
    }
}

struct SqliteParam<'value>(&'value SqliteValue);

impl ToSql for SqliteParam<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        let value = match self.0 {
            SqliteValue::Null => ValueRef::Null,
            SqliteValue::Integer(value) => ValueRef::Integer(*value),
            SqliteValue::Float(value) => ValueRef::Real(*value),
            SqliteValue::Text(value) => ValueRef::Text(value.as_bytes_direct()),
            SqliteValue::Blob(value) => ValueRef::Blob(value.as_ref()),
        };
        Ok(ToSqlOutput::Borrowed(value))
    }
}

fn row_value(value: ValueRef<'_>) -> SqliteValue {
    match value {
        ValueRef::Null => SqliteValue::Null,
        ValueRef::Integer(value) => SqliteValue::Integer(value),
        ValueRef::Real(value) => SqliteValue::Float(value),
        ValueRef::Text(value) => SqliteValue::Text(SmallText::from_bytes(value)),
        ValueRef::Blob(value) => SqliteValue::Blob(value.into()),
    }
}

fn query_statement(
    statement: &mut rusqlite::Statement<'_>,
    params: &[SqliteValue],
    path: &Path,
) -> Result<Vec<Row>, FrankenError> {
    let column_count = statement.column_count();
    let params = params.iter().map(SqliteParam).collect::<Vec<_>>();
    let mut rows = statement
        .query(rusqlite::params_from_iter(params.iter()))
        .map_err(|error| map_error(error, path))?;
    let mut output = Vec::new();
    while let Some(row) = rows.next().map_err(|error| map_error(error, path))? {
        let mut values = Vec::with_capacity(column_count);
        for index in 0..column_count {
            values.push(row_value(
                row.get_ref(index).map_err(|error| map_error(error, path))?,
            ));
        }
        output.push(Row { values });
    }
    Ok(output)
}

fn exactly_one_row(rows: Vec<Row>) -> Result<Row, FrankenError> {
    match rows.len() {
        0 => Err(FrankenError::QueryReturnedNoRows),
        1 => Ok(rows.into_iter().next().expect("one row exists")),
        _ => Err(FrankenError::QueryReturnedMultipleRows),
    }
}

fn error_detail(message: Option<String>, fallback: impl std::fmt::Display) -> String {
    message.unwrap_or_else(|| fallback.to_string())
}

fn message_error(message: &str) -> Option<FrankenError> {
    if let Some(name) = message.strip_prefix("no such table: ") {
        return Some(FrankenError::NoSuchTable {
            name: name.to_string(),
        });
    }
    if let Some(name) = message.strip_prefix("no such column: ") {
        return Some(FrankenError::NoSuchColumn {
            name: name.to_string(),
        });
    }
    if let Some(name) = message.strip_prefix("no such index: ") {
        return Some(FrankenError::NoSuchIndex {
            name: name.to_string(),
        });
    }
    if let Some(name) = message
        .strip_prefix("table ")
        .and_then(|detail| detail.strip_suffix(" already exists"))
    {
        return Some(FrankenError::TableExists {
            name: name.to_string(),
        });
    }
    if let Some(name) = message
        .strip_prefix("index ")
        .and_then(|detail| detail.strip_suffix(" already exists"))
    {
        return Some(FrankenError::IndexExists {
            name: name.to_string(),
        });
    }
    if let Some(columns) = message.strip_prefix("UNIQUE constraint failed: ") {
        return Some(FrankenError::UniqueViolation {
            columns: columns.to_string(),
        });
    }
    if let Some(column) = message.strip_prefix("NOT NULL constraint failed: ") {
        return Some(FrankenError::NotNullViolation {
            column: column.to_string(),
        });
    }
    if let Some(name) = message.strip_prefix("CHECK constraint failed: ") {
        return Some(FrankenError::CheckViolation {
            name: name.to_string(),
        });
    }
    if message == "FOREIGN KEY constraint failed" {
        return Some(FrankenError::ForeignKeyViolation);
    }
    if let Some(name) = message.strip_prefix("ambiguous column name: ") {
        return Some(FrankenError::AmbiguousColumn {
            name: name.to_string(),
        });
    }
    None
}

fn sqlite_failure(error: ffi::Error, message: Option<String>, path: &Path) -> FrankenError {
    let detail = error_detail(message, error);
    if let Some(mapped) = message_error(&detail) {
        return mapped;
    }
    match error.extended_code {
        ffi::SQLITE_BUSY_RECOVERY => return FrankenError::BusyRecovery,
        ffi::SQLITE_BUSY_SNAPSHOT => {
            return FrankenError::BusySnapshot {
                conflicting_pages: "stock SQLite snapshot conflict".to_string(),
            };
        }
        ffi::SQLITE_CONSTRAINT_PRIMARYKEY => return FrankenError::PrimaryKeyViolation,
        ffi::SQLITE_CONSTRAINT_UNIQUE => {
            return FrankenError::UniqueViolation { columns: detail };
        }
        ffi::SQLITE_CONSTRAINT_NOTNULL => {
            return FrankenError::NotNullViolation { column: detail };
        }
        ffi::SQLITE_CONSTRAINT_FOREIGNKEY => return FrankenError::ForeignKeyViolation,
        ffi::SQLITE_CONSTRAINT_CHECK => return FrankenError::CheckViolation { name: detail },
        ffi::SQLITE_CONSTRAINT_DATATYPE => return FrankenError::DatatypeMismatch,
        _ => {}
    }
    match error.code {
        ErrorCode::DatabaseBusy => FrankenError::Busy,
        ErrorCode::DatabaseLocked => FrankenError::DatabaseLocked {
            path: path.to_path_buf(),
        },
        ErrorCode::DatabaseCorrupt => FrankenError::DatabaseCorrupt { detail },
        ErrorCode::NotADatabase => FrankenError::NotADatabase {
            path: path.to_path_buf(),
        },
        ErrorCode::CannotOpen => FrankenError::CannotOpen {
            path: path.to_path_buf(),
        },
        ErrorCode::SchemaChanged => FrankenError::SchemaChanged,
        ErrorCode::DiskFull => FrankenError::DatabaseFull,
        ErrorCode::TooBig => FrankenError::TooBig,
        ErrorCode::OutOfMemory => FrankenError::OutOfMemory,
        ErrorCode::TypeMismatch => FrankenError::DatatypeMismatch,
        ErrorCode::OperationAborted | ErrorCode::OperationInterrupted => FrankenError::Abort,
        ErrorCode::AuthorizationForStatementDenied | ErrorCode::PermissionDenied => {
            FrankenError::AuthDenied
        }
        ErrorCode::FileLockingProtocolFailed => FrankenError::LockFailed { detail },
        _ => FrankenError::Internal(detail),
    }
}

fn map_error(error: rusqlite::Error, path: &Path) -> FrankenError {
    match error {
        rusqlite::Error::SqliteFailure(error, message) => sqlite_failure(error, message, path),
        rusqlite::Error::QueryReturnedNoRows => FrankenError::QueryReturnedNoRows,
        rusqlite::Error::QueryReturnedMoreThanOneRow => FrankenError::QueryReturnedMultipleRows,
        rusqlite::Error::InvalidColumnIndex(index) => FrankenError::NoSuchColumn {
            name: format!("column index {index}"),
        },
        rusqlite::Error::InvalidColumnName(name) => FrankenError::NoSuchColumn { name },
        rusqlite::Error::InvalidColumnType(index, name, actual) => FrankenError::TypeMismatch {
            expected: format!("column {index} ({name})"),
            actual: format!("{actual:?}"),
        },
        rusqlite::Error::InvalidPath(path) => FrankenError::CannotOpen { path },
        other => {
            let detail = other.to_string();
            message_error(&detail).unwrap_or(FrankenError::Internal(detail))
        }
    }
}

/// The spelling of `path` handed to the SQLite engine.
///
/// SQLite's Win32 VFS passes the exact bytes it is given to `CreateFileW`, so
/// a database (or `VACUUM INTO` target) whose absolute path nears `MAX_PATH`
/// fails with `ERROR_PATH_NOT_FOUND` (GitHub #462). Long Windows paths are
/// therefore rewritten to the extended-length (`\\?\`) form via
/// [`crate::util::windows_extended_length_path`]; `:memory:`, shorter paths,
/// and every path on non-Windows targets are returned unchanged.
#[must_use]
pub fn engine_path(path: &Path) -> PathBuf {
    if path == Path::new(":memory:") {
        return path.to_path_buf();
    }
    crate::util::windows_extended_length_path(path)
}

fn immutable_read_only_uri(path: &Path) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let engine = engine_path(path);
    let absolute_path = if engine.is_absolute() {
        engine
    } else {
        std::env::current_dir().map_or_else(|_| engine.clone(), |cwd| cwd.join(&engine))
    };
    let path = absolute_path.to_string_lossy();
    // An extended-length spelling must reach the VFS byte-for-byte, so its
    // backslashes are percent-encoded instead of being rewritten as `/`.
    #[cfg(windows)]
    let verbatim = path.starts_with(r"\\?\");
    let mut uri = String::with_capacity(path.len() + 32);
    uri.push_str("file:");
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'_' | b'-' | b'~' => {
                uri.push(char::from(byte));
            }
            #[cfg(windows)]
            b'\\' if !verbatim => uri.push('/'),
            #[cfg(windows)]
            b':' => uri.push(':'),
            _ => {
                uri.push('%');
                uri.push(char::from(HEX[usize::from(byte >> 4)]));
                uri.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
        }
    }
    uri.push_str("?mode=ro&immutable=1");
    uri
}

/// Synchronous connection backed by bundled stock SQLite.
pub struct Connection {
    inner: Option<rusqlite::Connection>,
    path: PathBuf,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Open (or create) a database at `path`.
    pub fn open(path: impl Into<String>) -> Result<Self, FrankenError> {
        let path = PathBuf::from(path.into());
        let inner = if path == Path::new(":memory:") {
            rusqlite::Connection::open_in_memory()
        } else {
            rusqlite::Connection::open(engine_path(&path))
        }
        .map_err(|error| map_error(error, &path))?;
        Ok(Self {
            inner: Some(inner),
            path,
        })
    }

    fn from_inner(inner: rusqlite::Connection, path: PathBuf) -> Self {
        Self {
            inner: Some(inner),
            path,
        }
    }

    fn inner(&self) -> Result<&rusqlite::Connection, FrankenError> {
        self.inner
            .as_ref()
            .ok_or_else(|| FrankenError::Internal("connection is closed".to_string()))
    }

    /// Execute a single SQL statement, returning the affected row count.
    pub fn execute(&self, sql: &str) -> Result<usize, FrankenError> {
        match self.inner()?.execute(sql, []) {
            Ok(changed) => Ok(changed),
            Err(rusqlite::Error::ExecuteReturnedResults) => {
                let mut statement = self
                    .inner()?
                    .prepare(sql)
                    .map_err(|error| map_error(error, &self.path))?;
                query_statement(&mut statement, &[], &self.path).map(|_| 0)
            }
            Err(error) => Err(map_error(error, &self.path)),
        }
    }

    /// Execute a single SQL statement with positional parameters.
    pub fn execute_with_params(
        &self,
        sql: &str,
        params: &[SqliteValue],
    ) -> Result<usize, FrankenError> {
        let params = params.iter().map(SqliteParam).collect::<Vec<_>>();
        self.inner()?
            .execute(sql, rusqlite::params_from_iter(params.iter()))
            .map_err(|error| map_error(error, &self.path))
    }

    /// Query, returning all rows.
    pub fn query(&self, sql: &str) -> Result<Vec<Row>, FrankenError> {
        let mut statement = self
            .inner()?
            .prepare(sql)
            .map_err(|error| map_error(error, &self.path))?;
        query_statement(&mut statement, &[], &self.path)
    }

    /// Query with positional parameters, returning all rows.
    pub fn query_with_params(
        &self,
        sql: &str,
        params: &[SqliteValue],
    ) -> Result<Vec<Row>, FrankenError> {
        let mut statement = self
            .inner()?
            .prepare(sql)
            .map_err(|error| map_error(error, &self.path))?;
        query_statement(&mut statement, params, &self.path)
    }

    /// Query, returning exactly one row.
    pub fn query_row(&self, sql: &str) -> Result<Row, FrankenError> {
        exactly_one_row(self.query(sql)?)
    }

    /// Query with positional parameters, returning exactly one row.
    pub fn query_row_with_params(
        &self,
        sql: &str,
        params: &[SqliteValue],
    ) -> Result<Row, FrankenError> {
        exactly_one_row(self.query_with_params(sql, params)?)
    }

    /// Prepare a statement for repeated execution.
    pub fn prepare(&self, sql: &str) -> Result<PreparedStatement<'_>, FrankenError> {
        let inner = self
            .inner()?
            .prepare(sql)
            .map_err(|error| map_error(error, &self.path))?;
        Ok(PreparedStatement {
            inner: RefCell::new(inner),
            sql: sql.to_string(),
            path: self.path.clone(),
        })
    }

    /// Last-inserted rowid on this connection.
    #[must_use]
    pub fn last_insert_rowid(&self) -> i64 {
        self.inner
            .as_ref()
            .map_or(0, rusqlite::Connection::last_insert_rowid)
    }

    /// Close the connection.
    pub fn close(mut self) -> Result<(), FrankenError> {
        self.close_in_place()
    }

    /// Close in place, retaining the handle on error so callers can retry.
    pub fn close_in_place(&mut self) -> Result<(), FrankenError> {
        let Some(inner) = self.inner.take() else {
            return Ok(());
        };
        match inner.close() {
            Ok(()) => Ok(()),
            Err((inner, error)) => {
                self.inner = Some(inner);
                Err(map_error(error, &self.path))
            }
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.as_ref() {
            let _ = inner.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
        }
    }
}

/// Prepared statement with the facade's historical shared-reference methods.
pub struct PreparedStatement<'connection> {
    inner: RefCell<rusqlite::Statement<'connection>>,
    sql: String,
    path: PathBuf,
}

impl PreparedStatement<'_> {
    /// Render the statement for diagnostics.
    #[must_use]
    pub fn explain(&self) -> String {
        self.sql.clone()
    }

    /// Query, returning all rows.
    pub fn query(&self) -> Result<Vec<Row>, FrankenError> {
        query_statement(&mut self.inner.borrow_mut(), &[], &self.path)
    }

    /// Query with positional parameters, returning all rows.
    pub fn query_with_params(&self, params: &[SqliteValue]) -> Result<Vec<Row>, FrankenError> {
        query_statement(&mut self.inner.borrow_mut(), params, &self.path)
    }

    /// Query, returning exactly one row.
    pub fn query_row(&self) -> Result<Row, FrankenError> {
        exactly_one_row(self.query()?)
    }

    /// Query with positional parameters, returning exactly one row.
    pub fn query_row_with_params(&self, params: &[SqliteValue]) -> Result<Row, FrankenError> {
        exactly_one_row(self.query_with_params(params)?)
    }

    /// Execute, returning the affected row count.
    pub fn execute(&self) -> Result<usize, FrankenError> {
        self.inner
            .borrow_mut()
            .execute([])
            .map_err(|error| map_error(error, &self.path))
    }

    /// Execute with positional parameters, returning the affected row count.
    pub fn execute_with_params(&self, params: &[SqliteValue]) -> Result<usize, FrankenError> {
        let params = params.iter().map(SqliteParam).collect::<Vec<_>>();
        self.inner
            .borrow_mut()
            .execute(rusqlite::params_from_iter(params.iter()))
            .map_err(|error| map_error(error, &self.path))
    }
}

/// rusqlite-style open flags, retained behind the historical compatibility API.
pub mod compat {
    use std::path::{Path, PathBuf};

    use super::{Connection, FrankenError, engine_path, immutable_read_only_uri, map_error};

    pub use rusqlite::OpenFlags;

    /// Open a database with explicit flags.
    pub fn open_with_flags(path: &str, flags: OpenFlags) -> Result<Connection, FrankenError> {
        let path = PathBuf::from(path);
        let inner = rusqlite::Connection::open_with_flags(engine_path(&path), flags)
            .map_err(|error| map_error(error, &path))?;
        Ok(Connection::from_inner(inner, path))
    }

    /// Open an immutable read-only snapshot without creating SQLite sidecars.
    ///
    /// Callers must first prove that no committed WAL frames are needed for
    /// the visible database state. SQLite intentionally ignores WAL content
    /// for immutable URIs.
    pub fn open_read_only_immutable(path: &Path) -> Result<Connection, FrankenError> {
        let uri = immutable_read_only_uri(path);
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
        let inner = rusqlite::Connection::open_with_flags(&uri, flags)
            .map_err(|error| map_error(error, path))?;
        Ok(Connection::from_inner(inner, path.to_path_buf()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_execute_query_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("bridge.db");
        let conn =
            Connection::open(db.to_string_lossy().into_owned()).expect("open bridge database");
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .expect("create table");
        let inserted = conn
            .execute_with_params(
                "INSERT INTO t (v) VALUES (?1)",
                &[SqliteValue::from("hello")],
            )
            .expect("insert row");
        assert_eq!(inserted, 1);
        let rows = conn.query("SELECT v FROM t").expect("query rows");
        assert_eq!(rows.len(), 1);
        let row = conn
            .query_row_with_params("SELECT v FROM t WHERE id = ?1", &[SqliteValue::from(1_i64)])
            .expect("query row");
        assert_eq!(row.get(0).and_then(SqliteValue::as_text), Some("hello"));
        conn.close().expect("close");
    }

    #[test]
    fn prepared_statement_roundtrip() {
        let conn = Connection::open(":memory:").expect("open in-memory database");
        conn.execute("CREATE TABLE t (k TEXT)").expect("create");
        conn.execute_with_params("INSERT INTO t (k) VALUES (?1)", &[SqliteValue::from("a")])
            .expect("insert");
        let stmt = conn
            .prepare("SELECT count(*) FROM t WHERE k = ?1")
            .expect("prepare");
        let row = stmt
            .query_row_with_params(&[SqliteValue::from("a")])
            .expect("query");
        assert_eq!(row.get(0).and_then(SqliteValue::as_integer), Some(1));
    }

    #[test]
    fn query_row_rejects_zero_and_multiple_rows() {
        let conn = Connection::open(":memory:").expect("open in-memory database");
        conn.execute("CREATE TABLE t (k INTEGER)").expect("create");
        assert!(matches!(
            conn.query_row("SELECT k FROM t"),
            Err(FrankenError::QueryReturnedNoRows)
        ));
        conn.execute("INSERT INTO t VALUES (1)")
            .expect("insert one");
        conn.execute("INSERT INTO t VALUES (2)")
            .expect("insert two");
        assert!(matches!(
            conn.query_row("SELECT k FROM t"),
            Err(FrankenError::QueryReturnedMultipleRows)
        ));
    }

    #[test]
    fn read_only_flags_refuse_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("readonly.db");
        let conn = Connection::open(db.to_string_lossy().into_owned()).expect("create database");
        conn.execute("CREATE TABLE t (k INTEGER)").expect("create");
        conn.close().expect("close writer");

        let conn = compat::open_with_flags(
            db.to_string_lossy().as_ref(),
            compat::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open read-only");
        assert!(conn.execute("INSERT INTO t VALUES (1)").is_err());
    }

    #[test]
    fn immutable_read_only_open_creates_no_sidecars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("immutable readonly.db");
        let conn = Connection::open(db.to_string_lossy().into_owned()).expect("create database");
        conn.execute("CREATE TABLE t (k INTEGER)").expect("create");
        conn.execute("INSERT INTO t VALUES (7)").expect("insert");
        conn.close().expect("close writer");

        let directory_names = || {
            let mut names = std::fs::read_dir(dir.path())
                .expect("read temp directory")
                .map(|entry| entry.expect("directory entry").file_name())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        let before = directory_names();
        let conn = compat::open_read_only_immutable(&db).expect("open immutable read-only");
        assert_eq!(
            conn.query_row("SELECT k FROM t")
                .expect("query")
                .get(0)
                .and_then(SqliteValue::as_integer),
            Some(7)
        );
        assert!(conn.execute("INSERT INTO t VALUES (8)").is_err());
        conn.close().expect("close reader");
        assert_eq!(directory_names(), before);
    }

    /// Off Windows the engine spelling is the operator's own spelling.
    #[cfg(not(windows))]
    #[test]
    fn engine_path_is_identity_off_windows() {
        let long = format!("/tmp/{}/beads.db", "segment/".repeat(60));
        assert_eq!(engine_path(Path::new(&long)), Path::new(&long));
        assert_eq!(
            engine_path(Path::new("relative.db")),
            Path::new("relative.db")
        );
        assert_eq!(engine_path(Path::new(":memory:")), Path::new(":memory:"));
    }

    /// #462: only paths that would overflow `MAX_PATH` (with room for
    /// SQLite's `-journal` sidecar suffix) are upgraded to the extended-length
    /// form; short paths, relative spellings, and `:memory:` keep their bytes.
    #[cfg(windows)]
    #[test]
    fn engine_path_upgrades_only_long_windows_paths() {
        assert_eq!(engine_path(Path::new(":memory:")), Path::new(":memory:"));
        assert_eq!(
            engine_path(Path::new(r"C:\repo\.beads\beads.db")),
            Path::new(r"C:\repo\.beads\beads.db")
        );

        let long_dir = format!(r"C:\{}", r"segment\".repeat(40));
        let long_db = format!(r"{long_dir}.beads\beads.db");
        assert!(long_db.len() > crate::util::WINDOWS_LEGACY_PATH_BUDGET);
        assert_eq!(
            engine_path(Path::new(&long_db)),
            Path::new(&format!(r"\\?\{long_db}"))
        );

        let long_unc = format!(r"\\server\share\{}beads.db", r"segment\".repeat(40));
        assert_eq!(
            engine_path(Path::new(&long_unc)),
            Path::new(&format!(r"\\?\UNC\{}", &long_unc[2..]))
        );

        let already_verbatim = format!(r"\\?\{long_db}");
        assert_eq!(
            engine_path(Path::new(&already_verbatim)),
            Path::new(&already_verbatim)
        );

        let uri = immutable_read_only_uri(Path::new(&long_db));
        assert!(uri.starts_with("file:%5C%5C%3F%5CC:%5C"), "{uri}");
        assert!(uri.ends_with("?mode=ro&immutable=1"), "{uri}");
        assert!(!uri[5..uri.len() - "?mode=ro&immutable=1".len()].contains('?'));
    }

    /// A database whose absolute path is well past `MAX_PATH` opens, writes,
    /// and reopens through the extended-length engine spelling (#462).
    #[cfg(windows)]
    #[test]
    fn long_windows_database_path_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut deep = dir.path().to_path_buf();
        while deep.as_os_str().len() < 300 {
            deep.push("a-long-directory-segment-name");
        }
        std::fs::create_dir_all(&deep).expect("create deep directory");
        let db = deep.join("beads.db");

        let conn = Connection::open(db.to_string_lossy().into_owned()).expect("open long path");
        conn.execute("CREATE TABLE t (k INTEGER)").expect("create");
        conn.execute("INSERT INTO t VALUES (7)").expect("insert");
        conn.close().expect("close");

        let reader = compat::open_read_only_immutable(&db).expect("immutable open");
        assert_eq!(
            reader
                .query_row("SELECT k FROM t")
                .expect("query")
                .get(0)
                .and_then(SqliteValue::as_integer),
            Some(7)
        );
        reader.close().expect("close reader");
    }
}
