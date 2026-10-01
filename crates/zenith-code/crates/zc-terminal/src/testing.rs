//! A scriptable PTY for tests (the `FakePtyAdapter` / `FakePtyProcess` of `Manager.test.ts`):
//! records spawns, writes, resizes and signals, and lets the test emit output and exits.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::pty::{
    DataCallback, ExitCallback, PtyAdapter, PtyEventHub, PtyExitEvent, PtyIoError, PtyProcess, PtySignal, PtySpawnError, PtySpawnInput, Unsubscribe,
};

/// A fake process. Pids start at 9000, in spawn order.
pub struct FakePtyProcess {
    pid: u32,
    hub: PtyEventHub,
    pub writes: Mutex<Vec<String>>,
    pub resize_calls: Mutex<Vec<(u16, u16)>>,
    pub kill_signals: Mutex<Vec<PtySignal>>,
    killed: AtomicBool,
    /// Makes the next writes fail with this message.
    pub write_failure: Mutex<Option<String>>,
    /// Makes the next resizes fail with this message.
    pub resize_failure: Mutex<Option<String>>,
    exit_on_subscribe: Option<PtyExitEvent>,
}

impl FakePtyProcess {
    pub fn emit_data(&self, data: &str) {
        self.hub.emit_data(data.to_owned());
    }

    pub fn emit_exit(&self, exit_code: i32, signal: Option<i32>) {
        self.hub.emit_exit(PtyExitEvent {
            exit_code: Some(exit_code),
            signal,
        });
    }

    pub fn killed(&self) -> bool {
        self.killed.load(Ordering::SeqCst)
    }

    pub fn writes(&self) -> Vec<String> {
        self.writes.lock().unwrap().clone()
    }

    pub fn resize_calls(&self) -> Vec<(u16, u16)> {
        self.resize_calls.lock().unwrap().clone()
    }

    pub fn kill_signals(&self) -> Vec<PtySignal> {
        self.kill_signals.lock().unwrap().clone()
    }
}

impl PtyProcess for FakePtyProcess {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn write(&self, data: &str) -> Result<(), PtyIoError> {
        if let Some(message) = self.write_failure.lock().unwrap().clone() {
            return Err(PtyIoError::new(message));
        }
        self.writes.lock().unwrap().push(data.to_owned());
        Ok(())
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<(), PtyIoError> {
        if let Some(message) = self.resize_failure.lock().unwrap().clone() {
            return Err(PtyIoError::new(message));
        }
        self.resize_calls.lock().unwrap().push((cols, rows));
        Ok(())
    }

    fn kill(&self, signal: PtySignal) -> Result<(), PtyIoError> {
        self.killed.store(true, Ordering::SeqCst);
        self.kill_signals.lock().unwrap().push(signal);
        Ok(())
    }

    fn on_data(&self, callback: DataCallback) -> Unsubscribe {
        self.hub.on_data(callback)
    }

    fn on_exit(&self, callback: ExitCallback) -> Unsubscribe {
        if let Some(event) = self.exit_on_subscribe {
            self.hub.emit_exit(event);
        }
        self.hub.on_exit(callback)
    }
}

/// A fake adapter.
pub struct FakePtyAdapter {
    pub spawn_inputs: Mutex<Vec<PtySpawnInput>>,
    pub processes: Mutex<Vec<Arc<FakePtyProcess>>>,
    /// Each queued message makes one spawn fail.
    pub spawn_failures: Mutex<VecDeque<String>>,
    /// Every process exits as soon as the manager subscribes.
    pub exit_on_subscribe: Mutex<Option<PtyExitEvent>>,
    /// Spawns yield to the runtime first (the TS "async" fake).
    pub asynchronous: bool,
    next_pid: AtomicU32,
}

impl Default for FakePtyAdapter {
    fn default() -> Self {
        Self::new(false)
    }
}

impl FakePtyAdapter {
    pub fn new(asynchronous: bool) -> Self {
        Self {
            spawn_inputs: Mutex::new(Vec::new()),
            processes: Mutex::new(Vec::new()),
            spawn_failures: Mutex::new(VecDeque::new()),
            exit_on_subscribe: Mutex::new(None),
            asynchronous,
            next_pid: AtomicU32::new(9000),
        }
    }

    pub fn spawn_inputs(&self) -> Vec<PtySpawnInput> {
        self.spawn_inputs.lock().unwrap().clone()
    }

    pub fn process(&self, index: usize) -> Arc<FakePtyProcess> {
        self.processes.lock().unwrap()[index].clone()
    }

    pub fn process_count(&self) -> usize {
        self.processes.lock().unwrap().len()
    }
}

#[async_trait]
impl PtyAdapter for FakePtyAdapter {
    async fn spawn(&self, input: PtySpawnInput) -> Result<Arc<dyn PtyProcess>, PtySpawnError> {
        if self.asynchronous {
            tokio::task::yield_now().await;
        }
        self.spawn_inputs.lock().unwrap().push(input.clone());
        if let Some(failure) = self.spawn_failures.lock().unwrap().pop_front() {
            return Err(PtySpawnError::new("fake", Some(input.shell), failure));
        }
        let process = Arc::new(FakePtyProcess {
            pid: self.next_pid.fetch_add(1, Ordering::SeqCst),
            hub: PtyEventHub::new(),
            writes: Mutex::new(Vec::new()),
            resize_calls: Mutex::new(Vec::new()),
            kill_signals: Mutex::new(Vec::new()),
            killed: AtomicBool::new(false),
            write_failure: Mutex::new(None),
            resize_failure: Mutex::new(None),
            exit_on_subscribe: *self.exit_on_subscribe.lock().unwrap(),
        });
        self.processes.lock().unwrap().push(process.clone());
        Ok(process)
    }
}
