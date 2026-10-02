//! Real PTYs: the manager on `PortablePtyAdapter`, running `/bin/sh`.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::{FutureExt, StreamExt};
use zc_terminal::contracts::{
    TerminalCloseInput, TerminalEvent, TerminalEventKind, TerminalOpenInput, TerminalResizeInput, TerminalRestartInput, TerminalSessionStatus,
    TerminalWriteInput, DEFAULT_TERMINAL_ID,
};
use zc_terminal::{ListenerStream, PortablePtyAdapter, TerminalManager, TerminalManagerOptions};

struct Shell {
    _dir: tempfile::TempDir,
    cwd: PathBuf,
    manager: TerminalManager,
    events: Mutex<ListenerStream<TerminalEvent>>,
    seen: Mutex<Vec<TerminalEvent>>,
}

fn base_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into());
    env.insert("HOME".into(), std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()));
    env.insert("PS1".into(), "$ ".into());
    env.insert("T3CODE_SECRET".into(), "server-only".into());
    env.insert("LANG".into(), "en_US.UTF-8".into());
    env.insert("LC_ALL".into(), "en_US.UTF-8".into());
    env
}

async fn shell_with(logs_dir: Option<PathBuf>, configure: impl FnOnce(&mut TerminalManagerOptions)) -> Shell {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let logs_dir = logs_dir.unwrap_or_else(|| dir.path().join("logs/terminals"));
    let mut options = TerminalManagerOptions::new(&logs_dir, Arc::new(PortablePtyAdapter));
    options.shell_resolver = Some(Arc::new(|| "/bin/sh".into()));
    options.env = Some(base_env());
    options.process_kill_grace = Duration::from_millis(300);
    options.subprocess_poll_interval = Duration::from_secs(3600);
    configure(&mut options);
    let manager = TerminalManager::new(options).await.unwrap();
    let events = Mutex::new(manager.subscribe());
    Shell {
        _dir: dir,
        cwd,

        manager,
        events,
        seen: Mutex::new(Vec::new()),
    }
}

async fn shell() -> Shell {
    shell_with(None, |_| {}).await
}

impl Shell {
    fn open_input(&self, terminal_id: &str) -> TerminalOpenInput {
        TerminalOpenInput {
            cols: Some(100),
            rows: Some(24),
            ..TerminalOpenInput::new("thread-1", terminal_id, self.cwd.to_string_lossy())
        }
    }

    async fn write(&self, terminal_id: &str, data: &str) {
        self.manager
            .write(TerminalWriteInput {
                thread_id: "thread-1".into(),
                terminal_id: terminal_id.into(),
                data: data.into(),
            })
            .await
            .unwrap();
    }

    fn collect(&self) -> Vec<TerminalEvent> {
        let mut seen = self.seen.lock().unwrap();
        let mut events = self.events.lock().unwrap();
        while let Some(Some(event)) = events.next().now_or_never() {
            seen.push(event);
        }
        seen.clone()
    }

    fn output(&self, terminal_id: &str) -> String {
        self.collect()
            .into_iter()
            .filter(|e| e.terminal_id == terminal_id)
            .filter_map(|e| match e.kind {
                TerminalEventKind::Output { data } => Some(data),
                _ => None,
            })
            .collect()
    }

    async fn wait_for(&self, what: &str, mut condition: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition(self) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; output so far: {:?}",
                self.output(DEFAULT_TERMINAL_ID)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_output(&self, terminal_id: &str, needle: &str) {
        self.wait_for(needle, |s| s.output(terminal_id).contains(needle)).await;
    }

    fn exits(&self) -> Vec<(Option<i32>, Option<i32>)> {
        self.collect()
            .into_iter()
            .filter_map(|e| match e.kind {
                TerminalEventKind::Exited { exit_code, exit_signal } => Some((exit_code, exit_signal)),
                _ => None,
            })
            .collect()
    }
}

fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_commands_in_a_real_shell() {
    let s = shell().await;
    let snapshot = s.manager.open(s.open_input(DEFAULT_TERMINAL_ID)).await.unwrap();
    assert_eq!(snapshot.status, TerminalSessionStatus::Running);
    assert!(snapshot.pid.is_some_and(|pid| pid > 0));
    s.write(DEFAULT_TERMINAL_ID, "echo hello-$((40+2))\n").await;
    s.wait_output(DEFAULT_TERMINAL_ID, "hello-42").await;
    s.write(DEFAULT_TERMINAL_ID, "pwd\n").await;
    s.wait_output(DEFAULT_TERMINAL_ID, &format!("{}\r\n", s.cwd.display())).await;
    // The terminal environment: TERM / COLORTERM set, server variables filtered.
    s.write(DEFAULT_TERMINAL_ID, "echo \"env:$TERM:$COLORTERM:${T3CODE_SECRET:-none}\"\n").await;
    s.wait_output(DEFAULT_TERMINAL_ID, "env:xterm-256color:truecolor:none").await;
    // Multibyte output survives the UTF-8 decoder whatever the read boundaries.
    s.write(DEFAULT_TERMINAL_ID, "printf 'u:\\303\\251\\345\\220\\215\\360\\237\\232\\200:end\\n'\n")
        .await;
    s.wait_output(DEFAULT_TERMINAL_ID, "u:é名🚀:end").await;
    s.manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resizes_the_pty() {
    let s = shell().await;
    s.manager.open(s.open_input(DEFAULT_TERMINAL_ID)).await.unwrap();
    s.manager
        .resize(TerminalResizeInput {
            thread_id: "thread-1".into(),
            terminal_id: DEFAULT_TERMINAL_ID.into(),
            cols: 132,
            rows: 40,
        })
        .await
        .unwrap();
    s.write(DEFAULT_TERMINAL_ID, "echo size:$(stty size)\n").await;
    s.wait_output(DEFAULT_TERMINAL_ID, "size:40 132").await;
    s.manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reports_exit_codes_and_signals() {
    let s = shell().await;
    s.manager.open(s.open_input(DEFAULT_TERMINAL_ID)).await.unwrap();
    s.write(DEFAULT_TERMINAL_ID, "echo bye; exit 3\n").await;
    s.wait_for("exit", |s| !s.exits().is_empty()).await;
    // node-pty's numbers: the exit status, and signal 0 for a normal exit.
    assert_eq!(s.exits(), [(Some(3), Some(0))]);
    // The output comes before the exit.
    let events = s.collect();
    let exit_index = events.iter().position(|e| e.kind.type_name() == "exited").unwrap();
    // The PTY may split "bye" across reads: look at the output joined up to the exit.
    let before_exit: String = events[..exit_index]
        .iter()
        .filter_map(|e| match &e.kind {
            TerminalEventKind::Output { data } => Some(data.as_str()),
            _ => None,
        })
        .collect();
    assert!(before_exit.contains("bye\r\n"), "{before_exit:?}");
    // Writing to the exited terminal is ignored; reopening starts a fresh shell.
    s.write(DEFAULT_TERMINAL_ID, "ignored\n").await;
    let reopened = s.manager.open(s.open_input(DEFAULT_TERMINAL_ID)).await.unwrap();
    assert_eq!(reopened.status, TerminalSessionStatus::Running);
    assert_eq!(reopened.history, "");

    // Killed by a signal: exit code 0 and the signal number.
    s.write(DEFAULT_TERMINAL_ID, "kill -9 $$\n").await;
    s.wait_for("signal exit", |s| s.exits().len() == 2).await;
    assert_eq!(s.exits()[1], (Some(0), Some(9)));
    s.manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_kills_the_shell_with_escalation() {
    let s = shell().await;
    let snapshot = s.manager.open(s.open_input(DEFAULT_TERMINAL_ID)).await.unwrap();
    let pid = snapshot.pid.unwrap();
    s.write(DEFAULT_TERMINAL_ID, "echo ready\n").await;
    s.wait_output(DEFAULT_TERMINAL_ID, "ready\r\n").await;
    // An interactive shell ignores SIGTERM: this one also traps it, so only SIGKILL works.
    s.write(DEFAULT_TERMINAL_ID, "trap '' TERM HUP; echo trapped\n").await;
    s.wait_output(DEFAULT_TERMINAL_ID, "trapped\r\n").await;
    assert!(pid_alive(pid));
    s.manager
        .close(TerminalCloseInput {
            thread_id: "thread-1".into(),
            terminal_id: Some(DEFAULT_TERMINAL_ID.into()),
            delete_history: None,
        })
        .await
        .unwrap();
    assert!(s.collect().iter().any(|e| e.kind.type_name() == "closed"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while pid_alive(pid) {
        assert!(Instant::now() < deadline, "the shell survived SIGKILL escalation");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    s.manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_replaces_the_process_and_clears_history() {
    let s = shell().await;
    let first = s.manager.open(s.open_input(DEFAULT_TERMINAL_ID)).await.unwrap();
    s.write(DEFAULT_TERMINAL_ID, "echo before\n").await;
    s.wait_output(DEFAULT_TERMINAL_ID, "before\r\n").await;
    let restarted = s
        .manager
        .restart(TerminalRestartInput {
            thread_id: "thread-1".into(),
            terminal_id: DEFAULT_TERMINAL_ID.into(),
            cwd: s.cwd.to_string_lossy().into(),
            worktree_path: None,
            cols: 80,
            rows: 20,
            env: Some(BTreeMap::from([("MARKER".to_owned(), "restarted-env".to_owned())])),
            provider_instance_id: None,
        })
        .await
        .unwrap();
    assert_eq!(restarted.history, "");
    assert_eq!(restarted.status, TerminalSessionStatus::Running);
    assert_ne!(restarted.pid, first.pid);
    s.write(DEFAULT_TERMINAL_ID, "echo marker:$MARKER:$(stty size)\n").await;
    s.wait_output(DEFAULT_TERMINAL_ID, "marker:restarted-env:20 80").await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while pid_alive(first.pid.unwrap()) {
        assert!(Instant::now() < deadline, "the first shell is still alive");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    s.manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restores_history_from_file_in_a_new_manager() {
    let logs = tempfile::tempdir().unwrap();
    let logs_dir = logs.path().join("terminals");
    {
        let s = shell_with(Some(logs_dir.clone()), |_| {}).await;
        s.manager.open(s.open_input("term-2")).await.unwrap();
        s.write("term-2", "printf 'persisted-%s\\n' line\n").await;
        s.wait_output("term-2", "persisted-line\r\n").await;
        s.manager.shutdown().await;
        let stored = std::fs::read_to_string(s.manager.history_store().history_path("thread-1", "term-2")).unwrap();
        assert!(stored.contains("persisted-line"), "{stored:?}");
    }
    // A new server process: the PTY is gone, the history is not.
    let s = shell_with(Some(logs_dir), |_| {}).await;
    let snapshot = s.manager.open(s.open_input("term-2")).await.unwrap();
    assert!(snapshot.history.contains("persisted-line"), "{:?}", snapshot.history);
    assert_eq!(snapshot.status, TerminalSessionStatus::Running);
    s.manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detects_subprocesses_with_ps_and_closes_idle_shells() {
    let s = shell_with(None, |o| o.subprocess_poll_interval = Duration::from_millis(100)).await;
    s.manager.open(s.open_input(DEFAULT_TERMINAL_ID)).await.unwrap();
    s.manager.open(s.open_input("idle")).await.unwrap();
    s.write(DEFAULT_TERMINAL_ID, "sleep 30\n").await;
    s.wait_for("busy activity", |s| {
        s.collect().iter().any(|e| {
            e.terminal_id == DEFAULT_TERMINAL_ID && matches!(&e.kind, TerminalEventKind::Activity { has_running_subprocess: true, label } if label == "sleep")
        })
    })
    .await;
    // The busy terminal stays, the idle one closes.
    s.manager.close_idle("thread-1", None).await;
    let closed: Vec<String> = s
        .collect()
        .into_iter()
        .filter(|e| e.kind.type_name() == "closed")
        .map(|e| e.terminal_id)
        .collect();
    assert_eq!(closed, ["idle"]);
    // Ctrl-C ends the command: the terminal goes back to idle.
    s.write(DEFAULT_TERMINAL_ID, "\u{3}").await;
    s.wait_for("idle activity", |s| {
        s.collect()
            .iter()
            .any(|e| matches!(&e.kind, TerminalEventKind::Activity { has_running_subprocess: false, label } if label == "Terminal 1"))
    })
    .await;
    s.manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn falls_back_when_the_requested_shell_does_not_exist() {
    let s = shell_with(None, |o| o.shell_resolver = Some(Arc::new(|| "/definitely/missing-shell".into()))).await;
    let snapshot = s.manager.open(s.open_input(DEFAULT_TERMINAL_ID)).await.unwrap();
    assert_eq!(snapshot.status, TerminalSessionStatus::Running);
    s.write(DEFAULT_TERMINAL_ID, "echo fallback-$((1+1))\n").await;
    s.wait_output(DEFAULT_TERMINAL_ID, "fallback-2").await;
    s.manager.shutdown().await;
}
