//! One serialized writer and bounded snapshot readers. A writer lease never
//! returns an unfinished transaction to the next session, including on unwind.
use rusqlite::{Connection, OpenFlags};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::Duration;

pub(super) fn configure_writer(conn: &Connection) -> Result<(), String> {
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")
        .map_err(|e| e.to_string())
}

pub(super) fn enable_wal(conn: &Connection) -> Result<(), String> {
    let mode: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .map_err(|e| format!("Failed to enable session WAL: {e}"))?;
    if mode != "wal" {
        return Err(format!("Session database requires WAL; got {mode}"));
    }
    // Keep SQLite's passive checkpoint policy; readers below release their
    // snapshot at the end of each operation, rather than keeping it in the pool.
    conn.execute_batch("PRAGMA wal_autocheckpoint=1000;")
        .map_err(|e| e.to_string())
}

struct WriterState {
    conn: Connection,
    unhealthy: bool,
    /// The connection intentionally holds one read transaction for its whole
    /// lifetime; leases must not roll it back.
    pinned_read_snapshot: bool,
}

pub(super) struct SessionWriter {
    state: Mutex<WriterState>,
    path: PathBuf,
}

impl SessionWriter {
    pub(super) fn new(conn: Connection, path: PathBuf) -> Self {
        Self {
            state: Mutex::new(WriterState {
                conn,
                unhealthy: false,
                pinned_read_snapshot: false,
            }),
            path,
        }
    }

    /// Opens a query-only connection whose WAL read transaction freezes the
    /// committed database state until the connection is dropped. This gives
    /// export/fork a consistent view without copying the database, whose size
    /// grows with all sessions rather than the one being read.
    pub(super) fn pinned_read_snapshot(path: &Path) -> Result<Self, String> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("Failed to open session database for export snapshot: {e}"))?;
        conn.busy_timeout(Duration::from_secs(10))
            .map_err(|e| format!("Failed to configure export snapshot timeout: {e}"))?;
        conn.execute_batch("PRAGMA query_only=ON; PRAGMA foreign_keys=ON; BEGIN DEFERRED;")
            .map_err(|e| format!("Failed to begin session export snapshot: {e}"))?;
        // A deferred transaction takes its WAL snapshot at the first read.
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
            .map_err(|e| format!("Failed to pin session export snapshot: {e}"))?;
        Ok(Self {
            state: Mutex::new(WriterState {
                conn,
                unhealthy: false,
                pinned_read_snapshot: true,
            }),
            path: PathBuf::new(),
        })
    }

    fn lease<'a>(&self, mut state: MutexGuard<'a, WriterState>) -> Result<WriterLease<'a>, String> {
        if state.unhealthy {
            if self.path.as_os_str().is_empty() {
                return Err(
                    "The session snapshot connection could not be recovered".into(),
                );
            }
            // Close the quarantined handle (and its locks) before reopening.
            // Reopening must not rerun startup recovery/cancel active runs.
            let empty = Connection::open_in_memory().map_err(|e| e.to_string())?;
            drop(std::mem::replace(&mut state.conn, empty));
            let conn = Connection::open(&self.path).map_err(|e| e.to_string())?;
            configure_writer(&conn)?;
            enable_wal(&conn)?;
            state.conn = conn;
            state.unhealthy = false;
        }
        self.state.clear_poison();
        Ok(WriterLease { state })
    }

    pub(super) fn lock(&self) -> Result<WriterLease<'_>, String> {
        self.lease(self.state.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub(super) fn try_lock(&self) -> Result<WriterLease<'_>, String> {
        let state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return Err("Session writer is busy".into()),
        };
        self.lease(state)
    }
}

pub(super) struct WriterLease<'a> {
    state: MutexGuard<'a, WriterState>,
}

impl Deref for WriterLease<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.state.conn
    }
}
impl DerefMut for WriterLease<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.state.conn
    }
}
impl Drop for WriterLease<'_> {
    fn drop(&mut self) {
        if self.state.pinned_read_snapshot {
            // Ending the transaction would silently move the snapshot forward.
            if self.state.conn.is_autocommit() {
                self.state.unhealthy = true;
                eprintln!("[SessionStore] export snapshot transaction ended; connection quarantined");
            }
            return;
        }
        if !self.state.conn.is_autocommit() {
            eprintln!("[SessionStore] rolling back an unfinished writer transaction before releasing the connection");
            if let Err(error) = self.state.conn.execute_batch("ROLLBACK") {
                self.state.unhealthy = true;
                eprintln!("[SessionStore] writer rollback failed; connection quarantined: {error}");
            }
            self.state.unhealthy |= !self.state.conn.is_autocommit();
        }
    }
}

pub(super) struct SessionReaders {
    slots: [Mutex<Option<Connection>>; 2],
}

impl SessionReaders {
    pub(super) fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| Mutex::new(None)),
        }
    }

    pub(super) fn acquire(&self, path: &Path) -> Result<ReaderLease<'_>, String> {
        for slot in &self.slots {
            match slot.try_lock() {
                Ok(guard) => return Self::begin(guard, path),
                Err(TryLockError::Poisoned(error)) => return Self::begin(error.into_inner(), path),
                Err(TryLockError::WouldBlock) => {}
            }
        }
        Self::begin(
            self.slots[0].lock().unwrap_or_else(|e| e.into_inner()),
            path,
        )
    }

    fn begin<'a>(
        mut slot: MutexGuard<'a, Option<Connection>>,
        path: &Path,
    ) -> Result<ReaderLease<'a>, String> {
        if slot.is_none() {
            let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|e| format!("Failed to open session reader: {e}"))?;
            conn.busy_timeout(Duration::from_millis(250))
                .map_err(|e| e.to_string())?;
            conn.execute_batch("PRAGMA query_only=ON; PRAGMA foreign_keys=ON;")
                .map_err(|e| e.to_string())?;
            *slot = Some(conn);
        }
        slot.as_ref()
            .unwrap()
            .execute_batch("BEGIN DEFERRED")
            .map_err(|e| e.to_string())?;
        Ok(ReaderLease { slot })
    }
}

pub(super) struct ReaderLease<'a> {
    slot: MutexGuard<'a, Option<Connection>>,
}
impl Deref for ReaderLease<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.slot.as_ref().unwrap()
    }
}
impl Drop for ReaderLease<'_> {
    fn drop(&mut self) {
        if self
            .slot
            .as_ref()
            .is_some_and(|conn| !conn.is_autocommit() && conn.execute_batch("ROLLBACK").is_err())
        {
            // Never recycle a reader with an old snapshot or a failed rollback.
            *self.slot = None;
        }
    }
}

pub(super) enum ReadLease<'a> {
    File(ReaderLease<'a>),
    Snapshot(WriterLease<'a>),
}
impl Deref for ReadLease<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        match self {
            Self::File(conn) => conn,
            Self::Snapshot(conn) => conn,
        }
    }
}

impl super::SessionStore {
    pub(super) fn read_conn(&self) -> Result<ReadLease<'_>, String> {
        if self.db_path.as_os_str().is_empty() {
            Ok(ReadLease::Snapshot(self.conn.lock()?))
        } else {
            Ok(ReadLease::File(self.readers.acquire(&self.db_path)?))
        }
    }
}
