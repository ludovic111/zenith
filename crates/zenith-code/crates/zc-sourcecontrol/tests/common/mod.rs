//! Shared fixtures for the zc-sourcecontrol tests.
//!
//! - [`ScriptedRunner`]: a `ProcessRunner` answering from a closure and recording every
//!   invocation (the TS tests' `Layer.mock(VcsProcess)`), so `VcsProcess` still applies its
//!   limits, defaults and stderr classification.
//! - [`FakeClis`]: fake `gh`/`glab`/`az`/`tea`/`fj` executables (`testUtils/fakeCli.ts`): shell
//!   scripts in a temp directory that print recorded outputs per argument line and log their
//!   calls; [`FakeClis::runner`] resolves the forge CLIs there and runs everything else (git)
//!   for real.
//! - Temporary repositories with an isolated git config.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, Once};

use async_trait::async_trait;
use zc_core::defect::Defect;
use zc_core::process::{ProcessInvocation, ProcessRunError, ProcessRunInput, ProcessRunOutput, ProcessRunner, SystemProcessRunner};
use zc_core::vcs_process::VcsProcess;

pub mod http;

static ISOLATE: Once = Once::new();

/// Point git at an empty global config and ignore the system config (once per test binary).
pub fn isolate_git() {
    ISOLATE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("zc-sourcecontrol-test-home-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("gitconfig");
        std::fs::write(
            &config,
            "[protocol \"file\"]\n\tallow = always\n[advice]\n\tdefaultBranchName = false\n[commit]\n\tgpgsign = false\n[init]\n\tdefaultBranch = main\n[user]\n\tname = Test\n\temail = test@example.test\n",
        )
        .unwrap();
        // SAFETY: runs once, before any test of this binary spawns a process (every fixture
        // calls `isolate_git` first, and `call_once` blocks the others).
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", &config);
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
            std::env::remove_var("GIT_DIR");
            std::env::remove_var("GIT_WORK_TREE");
            std::env::remove_var("GH_HOST");
        }
    });
}

/// A temporary directory with its canonical path.
pub struct Tmp {
    _dir: tempfile::TempDir,
    pub path: PathBuf,
}

impl Tmp {
    pub fn new(prefix: &str) -> Self {
        isolate_git();
        let dir = tempfile::Builder::new().prefix(prefix).tempdir().unwrap();
        let path = std::fs::canonicalize(dir.path()).unwrap();
        Self { _dir: dir, path }
    }

    pub fn str(&self) -> &str {
        self.path.to_str().unwrap()
    }

    pub fn join(&self, rel: &str) -> String {
        self.path.join(rel).to_string_lossy().into_owned()
    }
}

/// Run git; panics on failure. Returns trimmed stdout.
pub fn git(cwd: impl AsRef<Path>, args: &[&str]) -> String {
    isolate_git();
    let output = Command::new("git").args(args).current_dir(cwd.as_ref()).output().unwrap();
    assert!(output.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

pub fn write(cwd: impl AsRef<Path>, rel: &str, contents: &str) {
    let path = cwd.as_ref().join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// `git init` + one commit on `main`.
pub fn init_repo_with_commit(cwd: impl AsRef<Path>) {
    let cwd = cwd.as_ref();
    git(cwd, &["init", "-b", "main"]);
    write(cwd, "README.md", "# test\n");
    git(cwd, &["add", "."]);
    git(cwd, &["commit", "-m", "initial commit"]);
}

/// A successful process result.
pub fn ok(stdout: &str) -> Result<ProcessRunOutput, ProcessRunError> {
    Ok(ProcessRunOutput {
        stdout: stdout.into(),
        code: Some(0),
        ..ProcessRunOutput::default()
    })
}

/// A finished process with an exit code and stderr.
pub fn exit(code: i32, stdout: &str, stderr: &str) -> Result<ProcessRunOutput, ProcessRunError> {
    Ok(ProcessRunOutput {
        stdout: stdout.into(),
        stderr: stderr.into(),
        code: Some(code),
        ..ProcessRunOutput::default()
    })
}

/// The spawn failure of a missing executable.
pub fn missing(input: &ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> {
    Err(ProcessRunError::Spawn {
        invocation: ProcessInvocation {
            command: input.command.clone(),
            argument_count: input.args.len(),
            cwd: input.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
            spawn_cwd: None,
        },
        resolved_command: Some(input.command.clone()),
        resolved_argument_count: Some(input.args.len()),
        shell: Some(false),
        cause: Defect::error("Error", "No such file or directory (os error 2)"),
    })
}

type Respond = Box<dyn Fn(&ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> + Send + Sync>;

/// A recording process runner answering from a closure.
pub struct ScriptedRunner {
    pub calls: Mutex<Vec<ProcessRunInput>>,
    respond: Respond,
}

impl ScriptedRunner {
    pub fn new(respond: impl Fn(&ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            respond: Box::new(respond),
        })
    }

    pub fn process(self: &Arc<Self>) -> VcsProcess {
        VcsProcess::new(self.clone())
    }

    /// Every call as `command arg…`.
    pub fn lines(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| format!("{} {}", c.command, c.args.join(" ")).trim().to_owned())
            .collect()
    }

    pub fn calls(&self) -> Vec<ProcessRunInput> {
        self.calls.lock().unwrap().clone()
    }

    pub fn count(&self, predicate: impl Fn(&ProcessRunInput) -> bool) -> usize {
        self.calls.lock().unwrap().iter().filter(|c| predicate(c)).count()
    }
}

#[async_trait]
impl ProcessRunner for ScriptedRunner {
    async fn run(&self, input: ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> {
        self.calls.lock().unwrap().push(input.clone());
        (self.respond)(&input)
    }
}

/// The env value a call set.
pub fn env_of(input: &ProcessRunInput, key: &str) -> Option<String> {
    input.env.as_ref()?.get(key).cloned().flatten()
}

/// Fake forge CLIs in a temp `bin` directory.
pub struct FakeClis {
    pub dir: Tmp,
    scripts: Mutex<BTreeMap<String, Vec<(String, String, i32)>>>,
}

pub const FORGE_CLIS: [&str; 6] = ["gh", "glab", "az", "tea", "fj", "jj"];

impl FakeClis {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            dir: Tmp::new("zc-sourcecontrol-bin-"),
            scripts: Mutex::default(),
        })
    }

    /// `name <args>` prints `stdout` (and `stderr`) and exits with `code`. Calls with other
    /// arguments print `unknown command` and exit 1.
    pub fn respond(&self, name: &str, args: &str, stdout: &str, stderr: &str, code: i32) {
        let mut scripts = self.scripts.lock().unwrap();
        let entries = scripts.entry(name.to_owned()).or_default();
        let index = entries.len();
        std::fs::write(self.dir.path.join(format!("{name}-{index}.out")), stdout).unwrap();
        std::fs::write(self.dir.path.join(format!("{name}-{index}.err")), stderr).unwrap();
        entries.push((args.to_owned(), format!("{name}-{index}"), code));
        let mut script = String::from("#!/bin/sh\nd=\"$(dirname \"$0\")\"\nprintf '%s\\n' \"$*\" >> \"$d/");
        script.push_str(name);
        script.push_str(".log\"\nprintf 'GH_TOKEN=%s GH_HOST=%s\\n' \"$GH_TOKEN\" \"$GH_HOST\" >> \"$d/");
        script.push_str(name);
        script.push_str(".env\"\ncase \"$*\" in\n");
        for (pattern, file, code) in entries.iter() {
            script.push_str(&format!(
                "  '{}') cat \"$d/{file}.out\"; cat \"$d/{file}.err\" >&2; exit {code} ;;\n",
                pattern.replace('\'', "'\\''")
            ));
        }
        script.push_str("  *) echo \"unknown command: $*\" >&2; exit 1 ;;\nesac\n");
        let path = self.dir.path.join(name);
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The argument lines `name` was called with.
    pub fn log(&self, name: &str) -> Vec<String> {
        std::fs::read_to_string(self.dir.path.join(format!("{name}.log")))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    pub fn env_log(&self, name: &str) -> Vec<String> {
        std::fs::read_to_string(self.dir.path.join(format!("{name}.env")))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// A runner spawning the fake forge CLIs from this directory (missing ones fail to spawn),
    /// and every other command for real.
    pub fn runner(self: &Arc<Self>) -> Arc<dyn ProcessRunner> {
        Arc::new(FakeCliRunner { clis: self.clone() })
    }

    pub fn process(self: &Arc<Self>) -> VcsProcess {
        VcsProcess::new(self.runner())
    }
}

struct FakeCliRunner {
    clis: Arc<FakeClis>,
}

#[async_trait]
impl ProcessRunner for FakeCliRunner {
    async fn run(&self, mut input: ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> {
        if FORGE_CLIS.contains(&input.command.as_str()) {
            input.command = self.clis.dir.path.join(&input.command).to_string_lossy().into_owned();
        }
        SystemProcessRunner.run(input).await
    }
}
