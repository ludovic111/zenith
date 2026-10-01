//! Fake forge CLIs for the golden tests: shell scripts in a temp `bin` directory that answer
//! recorded outputs keyed by the working directory's name, the argument line and (when the call
//! pipes a body, `--input -` or `--data @-`) the stdin, and log every key they were asked for. Both the TS
//! oracle (first on `PATH`) and the Rust side ([`FakeClis::runner`]) run them.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use zc_core::process::{ProcessRunError, ProcessRunInput, ProcessRunOutput, ProcessRunner, SystemProcessRunner};
use zc_core::vcs_process::VcsProcess;

pub struct FakeClis {
    _dir: tempfile::TempDir,
    pub path: PathBuf,
    count: Mutex<usize>,
    names: Vec<String>,
}

impl FakeClis {
    /// Fake executables named `names` (the others resolve normally).
    pub fn new(names: &[&str]) -> Arc<Self> {
        let dir = tempfile::Builder::new().prefix("zc-pullrequest-bin-").tempdir().unwrap();
        let path = std::fs::canonicalize(dir.path()).unwrap();
        for name in names {
            let script = format!(
                r#"#!/bin/sh
d="$(dirname "$0")"
w="$(basename "$PWD")"
body=""
case " $* " in *" --input - "*|*" --data @- "*) body="$(cat)";; esac
key="$w|$*|$body"
mkdir -p "$d/{name}.log.d"
printf '%s' "$key" > "$(mktemp "$d/{name}.log.d/call.XXXXXX")"
hit="$(K="$key" awk -F '\t' '$2 == ENVIRON["K"] {{ print $1; exit }}' "$d/{name}.index" 2>/dev/null)"
if [ -n "$hit" ]; then
  cat "$d/$hit.out"
  cat "$d/$hit.err" >&2
  exit "$(cat "$d/$hit.code")"
fi
echo "unknown command: $key" >&2
exit 1
"#
            );
            let file = path.join(name);
            std::fs::write(&file, script).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Arc::new(Self {
            _dir: dir,
            path,
            count: Mutex::new(0),
            names: names.iter().map(|name| (*name).to_owned()).collect(),
        })
    }

    /// `name` run in a directory called `cwd`, with `args` (joined by spaces) and the piped
    /// `stdin`, prints `stdout`/`stderr` and exits with `code`.
    pub fn respond(&self, name: &str, cwd: &str, args: &str, stdin: Option<&str>, stdout: impl AsRef<[u8]>, stderr: &str, code: i32) {
        let key = format!("{cwd}|{args}|{}", stdin.unwrap_or_default());
        assert!(!key.contains('\t') && !key.contains('\n'), "a fake CLI key must be one line: {key}");
        let mut count = self.count.lock().unwrap();
        let file = format!("{name}-{count}");
        *count += 1;
        std::fs::write(self.path.join(format!("{file}.out")), stdout).unwrap();
        std::fs::write(self.path.join(format!("{file}.err")), stderr).unwrap();
        std::fs::write(self.path.join(format!("{file}.code")), code.to_string()).unwrap();
        use std::io::Write;
        let mut index = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path.join(format!("{name}.index")))
            .unwrap();
        writeln!(index, "{file}\t{key}").unwrap();
    }

    /// Takes the keys `name` was called with since the last call, sorted (one file per call,
    /// since concurrent calls would interleave appends to one log).
    pub fn take_log(&self, name: &str) -> Vec<String> {
        let dir = self.path.join(format!("{name}.log.d"));
        let mut lines: Vec<String> = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
                    .collect()
            })
            .unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        lines.sort();
        lines
    }

    /// A runner resolving the fake CLIs from this directory and everything else normally.
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
        if self.clis.names.contains(&input.command) {
            input.command = self.clis.path.join(&input.command).to_string_lossy().into_owned();
        }
        SystemProcessRunner.run(input).await
    }
}
