//! The database handle: one writer thread that owns the read-write connection (the TS client's
//! single connection behind a 1-permit semaphore), fed by a tokio channel, plus optional
//! read-only WAL connections for snapshot queries that should not wait behind writes.

use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::thread;

use tokio::sync::{mpsc, oneshot};

use crate::conn::Conn;
use crate::error::{DbError, Result};
use crate::migrations::{self, MigrationOutcome};

type Job = Box<dyn FnOnce(&Conn) + Send + 'static>;

/// How to open the database.
#[derive(Debug, Clone)]
pub struct DbOptions {
    /// Run the migrations at open (the server always does; tests can opt out).
    pub migrate: bool,
    /// Number of read-only connections for [`Db::read`]. 0 sends reads to the writer.
    /// Ignored for in-memory databases.
    pub readers: usize,
}

impl Default for DbOptions {
    fn default() -> Self {
        Self { migrate: true, readers: 1 }
    }
}

/// A cloneable handle to the database. Dropping the last clone stops the threads once their
/// queued work is done.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

struct Inner {
    writer: mpsc::UnboundedSender<Job>,
    writer_thread: thread::ThreadId,
    readers: Option<std_mpsc::Sender<Job>>,
    path: Option<PathBuf>,
    migrations: MigrationOutcome,
}

impl Db {
    /// Opens `path` the way the TS server does (`makeSqlitePersistenceLive`): creates the
    /// directory, applies the pragmas, runs the migrations, then starts the writer thread.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, DbOptions::default())
    }

    pub fn open_with(path: impl AsRef<Path>, options: DbOptions) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let conn = Conn::open(&path)?;
        let outcome = if options.migrate {
            migrations::run(&conn)?
        } else {
            MigrationOutcome::default()
        };
        let readers = if options.readers > 0 {
            Some(spawn_readers(&path, options.readers)?)
        } else {
            None
        };
        Ok(Self::start(conn, readers, Some(path), outcome))
    }

    /// A private in-memory database with every migration applied (`SqlitePersistenceMemory`).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Conn::open_in_memory()?;
        let outcome = migrations::run(&conn)?;
        Ok(Self::start(conn, None, None, outcome))
    }

    /// Wraps an already opened (and migrated, if wanted) connection.
    pub fn from_conn(conn: Conn) -> Self {
        Self::start(conn, None, None, MigrationOutcome::default())
    }

    fn start(conn: Conn, readers: Option<std_mpsc::Sender<Job>>, path: Option<PathBuf>, migrations: MigrationOutcome) -> Self {
        let (writer, mut jobs) = mpsc::unbounded_channel::<Job>();
        let handle = thread::Builder::new()
            .name("zc-db-writer".into())
            .spawn(move || {
                while let Some(job) = jobs.blocking_recv() {
                    job(&conn);
                }
            })
            .expect("spawn the database writer thread");
        Self {
            inner: Arc::new(Inner {
                writer,
                writer_thread: handle.thread().id(),
                readers,
                path,
                migrations,
            }),
        }
    }

    /// The file this handle writes, or `None` for an in-memory database.
    pub fn path(&self) -> Option<&Path> {
        self.inner.path.as_deref()
    }

    /// What the migrator did when this handle was opened.
    pub fn migrations(&self) -> &MigrationOutcome {
        &self.inner.migrations
    }

    /// Runs `f` on the writer connection, after every call queued before it. Calls do not
    /// interleave: this is the serialization the TS semaphore gives. The work runs to completion
    /// even if the returned future is dropped.
    pub async fn call<R, F>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Conn) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (reply, response) = oneshot::channel();
        self.inner.writer.send(job(f, reply)).map_err(|_| DbError::Closed)?;
        response.await.map_err(|_| DbError::Closed)?
    }

    /// [`Db::call`] for synchronous code (the CLI, tests). Must not be called from the writer
    /// thread itself, nor from inside an async task (it blocks the thread).
    pub fn call_blocking<R, F>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Conn) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        assert_ne!(
            thread::current().id(),
            self.inner.writer_thread,
            "Db::call_blocking from the writer thread would deadlock; use the &Conn you have"
        );
        let (reply, response) = oneshot::channel();
        self.inner.writer.send(job(f, reply)).map_err(|_| DbError::Closed)?;
        response.blocking_recv().map_err(|_| DbError::Closed)?
    }

    /// Runs `f` inside a transaction on the writer (`sql.withTransaction`). Repositories called
    /// with the `&Conn` it receives join the transaction; nested `conn.transaction` calls
    /// become savepoints.
    pub async fn transaction<R, F>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Conn) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        self.call(move |conn| conn.transaction(f)).await
    }

    /// Runs a read on a read-only connection when there is one (it sees the last committed
    /// state, never the writer's open transaction), otherwise on the writer.
    pub async fn read<R, F>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Conn) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let Some(readers) = &self.inner.readers else {
            return self.call(f).await;
        };
        let (reply, response) = oneshot::channel();
        readers.send(job(f, reply)).map_err(|_| DbError::Closed)?;
        response.await.map_err(|_| DbError::Closed)?
    }
}

fn job<R, F>(f: F, reply: oneshot::Sender<Result<R>>) -> Job
where
    F: FnOnce(&Conn) -> Result<R> + Send + 'static,
    R: Send + 'static,
{
    Box::new(move |conn: &Conn| {
        let result = match panic::catch_unwind(AssertUnwindSafe(|| f(conn))) {
            Ok(result) => result,
            Err(payload) => Err(DbError::Panicked(panic_message(&payload))),
        };
        // The caller may have gone away; the work is done either way.
        let _ = reply.send(result);
    })
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "panic".to_string()
    }
}

fn spawn_readers(path: &Path, count: usize) -> Result<std_mpsc::Sender<Job>> {
    let (sender, receiver) = std_mpsc::channel::<Job>();
    let receiver = Arc::new(Mutex::new(receiver));
    for index in 0..count {
        let conn = Conn::open_read_only(path)?;
        let receiver = Arc::clone(&receiver);
        thread::Builder::new()
            .name(format!("zc-db-reader-{index}"))
            .spawn(move || loop {
                let next = {
                    let Ok(guard) = receiver.lock() else { return };
                    guard.recv()
                };
                match next {
                    Ok(job) => job(&conn),
                    Err(_) => return,
                }
            })
            .map_err(|error| DbError::Sql {
                operation: "open:spawnReader".into(),
                detail: Some(error.to_string()),
                kind: crate::error::SqlErrorKind::Unknown,
                correlation: None,
                cause: None,
            })?;
    }
    Ok(sender)
}
