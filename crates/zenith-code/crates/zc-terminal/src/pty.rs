//! The PTY port (`terminal/PtyAdapter.ts`) and its real implementation on `portable-pty`
//! (the Rust counterpart of `NodePtyAdapter.ts`).
//!
//! The manager only sees [`PtyAdapter`] / [`PtyProcess`], so tests drive it with a fake PTY
//! exactly like `Manager.test.ts` does. Callbacks mirror node-pty's `onData` / `onExit`:
//!
//! - data that arrives before the first `on_data` subscriber is kept and handed to it, so
//!   nothing is lost between spawn and subscription;
//! - `on_exit` after the exit replays it at once (`NodePtyProcess` retains the exit);
//! - the exit is delivered after the output read so far (the reader is given a moment to
//!   drain the PTY once the child is reaped);
//! - exit events carry node-pty's numbers: `exit_code` from `WEXITSTATUS` (0 when killed) and
//!   `signal` from `WTERMSIG` (0 for a normal exit).
//!
//! Callbacks run on the adapter's reader / waiter threads; they must not call back into the
//! process's `on_data` / `on_exit`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use zc_core::Defect;

use crate::decoder::Utf8StreamDecoder;

/// `PtyExitEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyExitEvent {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

/// The signals the manager sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtySignal {
    Term,
    Kill,
}

impl PtySignal {
    pub fn name(self) -> &'static str {
        match self {
            Self::Term => "SIGTERM",
            Self::Kill => "SIGKILL",
        }
    }
}

/// `PtySpawnInput`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtySpawnInput {
    pub shell: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub cols: u16,
    pub rows: u16,
    pub env: BTreeMap<String, String>,
}

/// `PtySpawnError`: `{adapter, shell?, attemptedShells?, cause?}`.
#[derive(Debug, Clone, PartialEq)]
pub struct PtySpawnError {
    pub adapter: String,
    pub shell: Option<String>,
    pub attempted_shells: Option<Vec<String>>,
    /// The chain of causes, outermost first (each one's message).
    pub causes: Vec<String>,
}

impl PtySpawnError {
    pub fn new(adapter: impl Into<String>, shell: Option<String>, cause: impl Into<String>) -> Self {
        Self {
            adapter: adapter.into(),
            shell,
            attempted_shells: None,
            causes: vec![cause.into()],
        }
    }

    /// The TS `message` getter.
    pub fn message(&self) -> String {
        let shell = self.shell.as_ref().map(|shell| format!(" '{shell}'")).unwrap_or_default();
        let attempted = match &self.attempted_shells {
            Some(shells) if !shells.is_empty() => format!(" Tried shells: {}.", shells.join(", ")),
            _ => String::new(),
        };
        format!("Failed to spawn PTY process{shell} with {}.{attempted}", self.adapter)
    }

    /// Every message of the error and its causes (`isRetryableShellSpawnError` reads them).
    pub fn messages(&self) -> Vec<String> {
        std::iter::once(self.message()).chain(self.causes.iter().cloned()).collect()
    }
}

impl std::fmt::Display for PtySpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for PtySpawnError {}

/// A write / resize / kill failure, kept as the error's `cause` defect.
#[derive(Debug, Clone, PartialEq)]
pub struct PtyIoError(pub Defect);

impl PtyIoError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(Defect::error("Error", message))
    }
}

impl From<std::io::Error> for PtyIoError {
    fn from(error: std::io::Error) -> Self {
        Self(Defect::from(&error))
    }
}

impl std::fmt::Display for PtyIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.message())
    }
}

pub type DataCallback = Arc<dyn Fn(String) + Send + Sync>;
pub type ExitCallback = Arc<dyn Fn(PtyExitEvent) + Send + Sync>;

/// Removes a callback (the function `onData` / `onExit` return in TS).
pub struct Unsubscribe(Option<Box<dyn FnOnce() + Send + Sync>>);

impl Unsubscribe {
    pub fn new(f: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self(Some(Box::new(f)))
    }

    pub fn noop() -> Self {
        Self(None)
    }

    pub fn call(mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

impl std::fmt::Debug for Unsubscribe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Unsubscribe")
    }
}

/// `PtyProcess`.
pub trait PtyProcess: Send + Sync {
    fn pid(&self) -> u32;
    fn write(&self, data: &str) -> Result<(), PtyIoError>;
    fn resize(&self, cols: u16, rows: u16) -> Result<(), PtyIoError>;
    fn kill(&self, signal: PtySignal) -> Result<(), PtyIoError>;
    fn on_data(&self, callback: DataCallback) -> Unsubscribe;
    fn on_exit(&self, callback: ExitCallback) -> Unsubscribe;
}

/// `PtyAdapter`.
#[async_trait]
pub trait PtyAdapter: Send + Sync {
    async fn spawn(&self, input: PtySpawnInput) -> Result<Arc<dyn PtyProcess>, PtySpawnError>;
}

// ---------------------------------------------------------------------------------------------
// Callback plumbing shared by real and fake processes
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct ListenerSet {
    next_id: u64,
    data: Vec<(u64, DataCallback)>,
    exit: Vec<(u64, ExitCallback)>,
    /// Output read before anyone listened.
    early_data: Vec<String>,
    exit_event: Option<PtyExitEvent>,
}

/// node-pty-like event fan-out: early data is kept for the first subscriber, a late `on_exit`
/// gets the exit replayed, deliveries are serialized (output order, then the exit).
#[derive(Default)]
pub struct PtyEventHub {
    /// Held while delivering, so callbacks see events in order. Never held by `unsubscribe`.
    delivery: Mutex<()>,
    listeners: Arc<Mutex<ListenerSet>>,
}

impl PtyEventHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn emit_data(&self, data: String) {
        let _delivery = self.delivery.lock().unwrap();
        let callbacks: Vec<DataCallback> = {
            let mut listeners = self.listeners.lock().unwrap();
            if listeners.exit_event.is_some() {
                return;
            }
            if listeners.data.is_empty() {
                listeners.early_data.push(data);
                return;
            }
            listeners.data.iter().map(|(_, cb)| cb.clone()).collect()
        };
        for callback in callbacks {
            callback(data.clone());
        }
    }

    /// Records the exit and tells every exit listener; later exits are ignored.
    pub fn emit_exit(&self, event: PtyExitEvent) {
        let _delivery = self.delivery.lock().unwrap();
        let callbacks: Vec<ExitCallback> = {
            let mut listeners = self.listeners.lock().unwrap();
            if listeners.exit_event.is_some() {
                return;
            }
            listeners.exit_event = Some(event);
            listeners.exit.drain(..).map(|(_, cb)| cb).collect()
        };
        for callback in callbacks {
            callback(event);
        }
    }

    pub fn has_exited(&self) -> bool {
        self.listeners.lock().unwrap().exit_event.is_some()
    }

    pub fn on_data(&self, callback: DataCallback) -> Unsubscribe {
        let _delivery = self.delivery.lock().unwrap();
        let (id, early) = {
            let mut listeners = self.listeners.lock().unwrap();
            let id = listeners.next_id;
            listeners.next_id += 1;
            listeners.data.push((id, callback.clone()));
            (id, std::mem::take(&mut listeners.early_data))
        };
        for data in early {
            callback(data);
        }
        let listeners = Arc::downgrade(&self.listeners);
        Unsubscribe::new(move || {
            if let Some(listeners) = listeners.upgrade() {
                listeners.lock().unwrap().data.retain(|(other, _)| *other != id);
            }
        })
    }

    pub fn on_exit(&self, callback: ExitCallback) -> Unsubscribe {
        let _delivery = self.delivery.lock().unwrap();
        let replay = {
            let mut listeners = self.listeners.lock().unwrap();
            match listeners.exit_event {
                Some(event) => Some(event),
                None => {
                    let id = listeners.next_id;
                    listeners.next_id += 1;
                    listeners.exit.push((id, callback.clone()));
                    let weak = Arc::downgrade(&self.listeners);
                    return Unsubscribe::new(move || {
                        if let Some(listeners) = weak.upgrade() {
                            listeners.lock().unwrap().exit.retain(|(other, _)| *other != id);
                        }
                    });
                }
            }
        };
        if let Some(event) = replay {
            callback(event);
        }
        Unsubscribe::noop()
    }
}

// ---------------------------------------------------------------------------------------------
// portable-pty
// ---------------------------------------------------------------------------------------------

const ADAPTER_NAME: &str = "portable-pty";
const READ_BUFFER_BYTES: usize = 64 * 1024;
/// How long the exit waits for the reader to drain the PTY after the child is reaped.
const EXIT_DRAIN_GRACE: Duration = Duration::from_millis(500);
#[cfg(unix)]
const READ_POLL_MS: i32 = 100;

/// The real adapter: `portable-pty` on a native PTY (`TERM=xterm-256color`, like node-pty's
/// `name`).
#[derive(Debug, Default, Clone, Copy)]
pub struct PortablePtyAdapter;

#[async_trait]
impl PtyAdapter for PortablePtyAdapter {
    async fn spawn(&self, input: PtySpawnInput) -> Result<Arc<dyn PtyProcess>, PtySpawnError> {
        let shell = input.shell.clone();
        match tokio::task::spawn_blocking(move || spawn_portable(input)).await {
            Ok(result) => result.map(|process| process as Arc<dyn PtyProcess>),
            Err(join) => Err(PtySpawnError::new(ADAPTER_NAME, Some(shell), format!("spawn task failed: {join}"))),
        }
    }
}

struct ReaderState {
    done: Mutex<bool>,
    signal: Condvar,
}

/// A process on a portable-pty PTY.
pub struct PortablePtyProcess {
    pid: u32,
    hub: Arc<PtyEventHub>,
    master: Mutex<Box<dyn portable_pty::MasterPty + Send>>,
    writer: Mutex<Option<std::sync::mpsc::Sender<Vec<u8>>>>,
    exited: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    #[cfg(not(unix))]
    killer: Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
}

impl std::fmt::Debug for PortablePtyProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortablePtyProcess").field("pid", &self.pid).finish_non_exhaustive()
    }
}

static THREAD_IDS: AtomicU64 = AtomicU64::new(1);

/// Missing executables read as `ENOENT`, so the manager's shell fallback (which looks for
/// `enoent` / `not found` / `no such file`) moves on to the next shell, as with node-pty.
fn spawn_error_message(text: String) -> String {
    let lower = text.to_lowercase();
    if lower.contains("doesn't exist") || lower.contains("does not exist") || lower.contains("no viable candidates") || lower.contains("no such file") {
        format!("ENOENT: {text}")
    } else {
        text
    }
}

fn spawn_portable(input: PtySpawnInput) -> Result<Arc<PortablePtyProcess>, PtySpawnError> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let fail = |message: String| PtySpawnError::new(ADAPTER_NAME, Some(input.shell.clone()), message);
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: input.rows,
            cols: input.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| fail(format!("openpty failed: {e:#}")))?;

    let mut command = CommandBuilder::new(&input.shell);
    command.args(&input.args);
    command.cwd(&input.cwd);
    command.env_clear();
    for (key, value) in &input.env {
        command.env(key, value);
    }
    if cfg!(unix) || !input.env.contains_key("TERM") {
        command.env("TERM", "xterm-256color");
    }

    let child = pair.slave.spawn_command(command).map_err(|e| fail(spawn_error_message(format!("{e:#}"))))?;
    // The child has its own copy of the slave; ours would keep the PTY open after it exits.
    drop(pair.slave);
    let pid = child.process_id().unwrap_or(0);
    let master = pair.master;
    let writer = master.take_writer().map_err(|e| fail(format!("could not open the PTY for writing: {e:#}")))?;

    let hub = Arc::new(PtyEventHub::new());
    let exited = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let reader_state = Arc::new(ReaderState {
        done: Mutex::new(false),
        signal: Condvar::new(),
    });
    let thread_id = THREAD_IDS.fetch_add(1, Ordering::Relaxed);

    // Reader.
    #[cfg(unix)]
    let reader = {
        let fd = master.as_raw_fd().ok_or_else(|| fail("the PTY has no file descriptor".into()))?;
        // SAFETY: duplicating a descriptor we own; the copy is closed by the reader.
        let dup = unsafe { libc::dup(fd) };
        if dup < 0 {
            return Err(fail(format!("could not duplicate the PTY descriptor: {}", std::io::Error::last_os_error())));
        }
        PtyReader::Fd(dup)
    };
    #[cfg(not(unix))]
    let reader = PtyReader::Blocking(
        master
            .try_clone_reader()
            .map_err(|e| fail(format!("could not open the PTY for reading: {e:#}")))?,
    );
    {
        let hub = hub.clone();
        let stop = stop.clone();
        let reader_state = reader_state.clone();
        std::thread::Builder::new()
            .name(format!("pty-read-{thread_id}"))
            .spawn(move || {
                reader.run(&hub, &stop);
                *reader_state.done.lock().unwrap() = true;
                reader_state.signal.notify_all();
            })
            .map_err(|e| fail(format!("could not start the PTY reader: {e}")))?;
    }

    // Writer: node-pty queues writes, so a full PTY never blocks the caller.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::Builder::new()
        .name(format!("pty-write-{thread_id}"))
        .spawn(move || {
            let mut writer = writer;
            while let Ok(bytes) = rx.recv() {
                if writer.write_all(&bytes).and_then(|()| writer.flush()).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| fail(format!("could not start the PTY writer: {e}")))?;

    #[cfg(not(unix))]
    let killer = child.clone_killer();

    // Waiter.
    {
        let hub = hub.clone();
        let exited = exited.clone();
        let mut child = child;
        std::thread::Builder::new()
            .name(format!("pty-wait-{thread_id}"))
            .spawn(move || {
                let event = wait_exit(&mut child);
                exited.store(true, Ordering::SeqCst);
                // Let the reader hand over the output still in the PTY first.
                let done = reader_state.done.lock().unwrap();
                let _ = reader_state.signal.wait_timeout_while(done, EXIT_DRAIN_GRACE, |done| !*done);
                hub.emit_exit(event);
            })
            .map_err(|e| fail(format!("could not start the PTY waiter: {e}")))?;
    }

    Ok(Arc::new(PortablePtyProcess {
        pid,
        hub,
        master: Mutex::new(master),
        writer: Mutex::new(Some(tx)),
        exited,
        stop,
        #[cfg(not(unix))]
        killer: Mutex::new(killer),
    }))
}

/// node-pty's exit numbers: `exitCode` = `WEXITSTATUS` (0 when signalled), `signal` =
/// `WTERMSIG` (0 when not).
fn wait_exit(child: &mut Box<dyn portable_pty::Child + Send + Sync>) -> PtyExitEvent {
    let child_ref: &mut dyn portable_pty::Child = &mut **child;
    if let Some(std_child) = child_ref.downcast_mut::<std::process::Child>() {
        return match std_child.wait() {
            Ok(status) => {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    PtyExitEvent {
                        exit_code: Some(status.code().unwrap_or(0)),
                        signal: Some(status.signal().unwrap_or(0)),
                    }
                }
                #[cfg(not(unix))]
                {
                    PtyExitEvent {
                        exit_code: Some(status.code().unwrap_or(0)),
                        signal: None,
                    }
                }
            }
            Err(_) => PtyExitEvent { exit_code: None, signal: None },
        };
    }
    match child.wait() {
        Ok(status) => PtyExitEvent {
            exit_code: Some(status.exit_code() as i32),
            signal: None,
        },
        Err(_) => PtyExitEvent { exit_code: None, signal: None },
    }
}

enum PtyReader {
    #[cfg(unix)]
    Fd(libc::c_int),
    #[cfg(not(unix))]
    Blocking(Box<dyn std::io::Read + Send>),
}

impl PtyReader {
    fn run(self, hub: &PtyEventHub, stop: &AtomicBool) {
        let mut decoder = Utf8StreamDecoder::new();
        let mut buffer = vec![0u8; READ_BUFFER_BYTES];
        match self {
            #[cfg(unix)]
            PtyReader::Fd(fd) => {
                loop {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let mut poll = libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: one valid pollfd.
                    let ready = unsafe { libc::poll(&mut poll, 1, READ_POLL_MS) };
                    if ready < 0 {
                        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                            continue;
                        }
                        break;
                    }
                    if ready == 0 {
                        continue;
                    }
                    // SAFETY: reading into our own buffer.
                    let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
                    if read < 0 {
                        let error = std::io::Error::last_os_error();
                        if matches!(error.kind(), std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock) {
                            continue;
                        }
                        // EIO: every slave descriptor is closed.
                        break;
                    }
                    if read == 0 {
                        break;
                    }
                    let text = decoder.write(&buffer[..read as usize]);
                    if !text.is_empty() {
                        hub.emit_data(text);
                    }
                }
                // SAFETY: closing the descriptor we duplicated.
                unsafe { libc::close(fd) };
            }
            #[cfg(not(unix))]
            PtyReader::Blocking(mut reader) => loop {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        let text = decoder.write(&buffer[..read]);
                        if !text.is_empty() {
                            hub.emit_data(text);
                        }
                    }
                }
            },
        }
        let rest = decoder.end();
        if !rest.is_empty() {
            hub.emit_data(rest);
        }
    }
}

impl PtyProcess for PortablePtyProcess {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn write(&self, data: &str) -> Result<(), PtyIoError> {
        let writer = self.writer.lock().unwrap();
        match writer.as_ref() {
            Some(tx) if tx.send(data.as_bytes().to_vec()).is_ok() => Ok(()),
            _ => Err(PtyIoError::new("The PTY input is closed")),
        }
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<(), PtyIoError> {
        self.master
            .lock()
            .unwrap()
            .resize(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyIoError::new(format!("{e:#}")))
    }

    /// Signals the shell (not its process group, like node-pty). A no-op once the child has
    /// been reaped, so a recycled pid is never signalled.
    fn kill(&self, signal: PtySignal) -> Result<(), PtyIoError> {
        if self.exited.load(Ordering::SeqCst) {
            return Ok(());
        }
        #[cfg(unix)]
        {
            let number = match signal {
                PtySignal::Term => libc::SIGTERM,
                PtySignal::Kill => libc::SIGKILL,
            };
            // SAFETY: plain kill(2).
            if unsafe { libc::kill(self.pid as libc::pid_t, number) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return Ok(());
                }
                return Err(error.into());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = signal;
            self.killer.lock().unwrap().kill().map_err(PtyIoError::from)
        }
    }

    fn on_data(&self, callback: DataCallback) -> Unsubscribe {
        self.hub.on_data(callback)
    }

    fn on_exit(&self, callback: ExitCallback) -> Unsubscribe {
        self.hub.on_exit(callback)
    }
}

impl Drop for PortablePtyProcess {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Dropping the sender ends the writer thread.
        self.writer.lock().unwrap().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_keeps_early_data_and_replays_exit() {
        let hub = PtyEventHub::new();
        hub.emit_data("early".into());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let unsubscribe = {
            let seen = seen.clone();
            hub.on_data(Arc::new(move |data| seen.lock().unwrap().push(data)))
        };
        hub.emit_data("late".into());
        unsubscribe.call();
        hub.emit_data("ignored".into());
        assert_eq!(*seen.lock().unwrap(), vec!["early", "late"]);

        let removed = Arc::new(Mutex::new(Vec::new()));
        {
            let removed = removed.clone();
            hub.on_exit(Arc::new(move |e| removed.lock().unwrap().push(e))).call();
        }
        hub.emit_exit(PtyExitEvent {
            exit_code: Some(7),
            signal: Some(2),
        });
        hub.emit_exit(PtyExitEvent {
            exit_code: Some(9),
            signal: None,
        });
        let late = Arc::new(Mutex::new(Vec::new()));
        {
            let late = late.clone();
            hub.on_exit(Arc::new(move |e| late.lock().unwrap().push(e)));
        }
        assert!(removed.lock().unwrap().is_empty());
        assert_eq!(
            *late.lock().unwrap(),
            vec![PtyExitEvent {
                exit_code: Some(7),
                signal: Some(2)
            }]
        );
    }

    #[test]
    fn spawn_error_messages() {
        let error = PtySpawnError {
            adapter: "terminal-manager".into(),
            shell: None,
            attempted_shells: Some(vec!["/bin/zsh -o nopromptsp".into(), "bash".into()]),
            causes: vec![],
        };
        assert_eq!(
            error.message(),
            "Failed to spawn PTY process with terminal-manager. Tried shells: /bin/zsh -o nopromptsp, bash."
        );
        assert_eq!(
            PtySpawnError::new("portable-pty", Some("/bin/x".into()), "boom").message(),
            "Failed to spawn PTY process '/bin/x' with portable-pty."
        );
    }
}
