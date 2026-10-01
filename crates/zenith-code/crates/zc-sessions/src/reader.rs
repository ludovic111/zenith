//! Finds the logs and keeps them read, cheaply.
//!
//! - **Unchanged files are not read again**: each file is remembered with its size,
//!   modification time and inode.
//! - **Growing files are read from where the last read stopped**: logs are append-only
//!   JSONL, so the running state of a file ([`ClaudeLog`], [`CodexLog`]) is kept with the
//!   offset after its last complete line, and only the new bytes are read. The 64 bytes
//!   before that offset are kept too and checked first: a file that was truncated, replaced
//!   or rewritten (with other bytes there) is read again from the start. An unterminated last line is read, but not
//!   committed until its newline arrives.
//! - **One read at a time, cached for 20 s** (`ReaderConfig::ttl`): concurrent callers wait
//!   for the read in progress and share its result. Files are read on up to 8 threads.
//!
//! Projects and liveness change without the files changing, so they are applied to the
//! cached list on every call ([`SessionReader::list`]), never stored with it.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::model::{project_of, Agent, ProjectRef, ProjectRoot, Session, SLOT_MS};
use crate::parse::{codex_titles, ClaudeLog, CodexLog};

/// Where the logs are and how long a read stays fresh.
#[derive(Clone, Debug)]
pub struct ReaderConfig {
    /// `~/.claude` (`CLAUDE_CONFIG_DIR`, else `CLAUDE_HOME`, else the home folder's).
    pub claude_home: PathBuf,
    /// `~/.codex` (`CODEX_HOME`, else the home folder's).
    pub codex_home: PathBuf,
    /// How long a read is reused. 20 s, like the dashboard's.
    pub ttl: Duration,
}

impl ReaderConfig {
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        Self {
            claude_home: var("CLAUDE_CONFIG_DIR").or_else(|| var("CLAUDE_HOME")).unwrap_or_else(|| home.join(".claude")),
            codex_home: var("CODEX_HOME").unwrap_or_else(|| home.join(".codex")),
            ttl: Duration::from_secs(20),
        }
    }
}

/// A log format, fed one line at a time.
pub(crate) trait LineLog: Clone + Default + Send {
    fn feed(&mut self, line: &str);
}

impl LineLog for ClaudeLog {
    fn feed(&mut self, line: &str) {
        ClaudeLog::feed(self, line)
    }
}

impl LineLog for CodexLog {
    fn feed(&mut self, line: &str) {
        CodexLog::feed(self, line)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileKey {
    len: u64,
    modified: Option<SystemTime>,
    inode: u64,
}

impl FileKey {
    fn of(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(meta);
        #[cfg(not(unix))]
        let inode = 0;
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            inode,
        }
    }
}

/// How many bytes before the offset are checked before resuming a read.
const TAIL: usize = 64;

/// A log file read up to `offset`.
#[derive(Clone, Debug)]
pub(crate) struct Tracked<L> {
    key: FileKey,
    /// Just after the last complete line.
    offset: u64,
    /// The bytes just before `offset` (up to [`TAIL`]).
    tail: Vec<u8>,
    /// Every complete line, fed.
    log: L,
    /// `log` plus the unterminated last line, when there is one.
    pending: Option<L>,
}

impl<L: LineLog> Tracked<L> {
    pub(crate) fn current(&self) -> &L {
        self.pending.as_ref().unwrap_or(&self.log)
    }

    /// Whether the bytes before `offset` are still the ones read last time.
    fn still_prefix(&self, file: &mut File, key: &FileKey) -> bool {
        if key.inode != self.key.inode || key.len < self.offset {
            return false;
        }
        let n = self.tail.len();
        if n == 0 {
            return self.offset == 0;
        }
        let mut seen = vec![0; n];
        file.seek(SeekFrom::Start(self.offset - n as u64)).is_ok() && file.read_exact(&mut seen).is_ok() && seen == self.tail
    }
}

/// Reads what changed in `path` since `prev`.
pub(crate) fn track<L: LineLog>(path: &Path, prev: Option<Tracked<L>>) -> io::Result<Tracked<L>> {
    let key = FileKey::of(&fs::metadata(path)?);
    if let Some(p) = prev.as_ref().filter(|p| p.key == key) {
        return Ok(p.clone());
    }
    let mut file = File::open(path)?;
    let (mut log, mut offset, mut tail) = match prev.filter(|p| p.still_prefix(&mut file, &key)) {
        Some(p) => (p.log, p.offset, p.tail),
        None => (L::default(), 0, Vec::new()),
    };
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::with_capacity(key.len.saturating_sub(offset) as usize);
    file.read_to_end(&mut buf)?;
    let done = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    for line in buf[..done].split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        log.feed(&String::from_utf8_lossy(line));
    }
    if done >= TAIL {
        tail = buf[done - TAIL..done].to_vec();
    } else {
        tail.extend_from_slice(&buf[..done]);
        tail.drain(..tail.len().saturating_sub(TAIL));
    }
    offset += done as u64;
    let rest = &buf[done..];
    let pending = (!rest.is_empty()).then(|| {
        let mut l = log.clone();
        l.feed(&String::from_utf8_lossy(rest));
        l
    });
    Ok(Tracked {
        key,
        offset,
        tail,
        log,
        pending,
    })
}

/// A folder's entries, sorted by name.
fn names(dir: &Path) -> Vec<String> {
    let mut list: Vec<String> = fs::read_dir(dir)
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    list.sort();
    list
}

/// A Claude transcript: its path, session id and how many subagents it ran.
struct ClaudeFile {
    path: PathBuf,
    id: String,
    subagents: usize,
}

/// Every transcript in `<claude>/projects/<folder>/`, with `<id>/subagents/` counted.
fn claude_files(home: &Path) -> Vec<ClaudeFile> {
    let base = home.join("projects");
    let mut files = Vec::new();
    for d in names(&base) {
        let dir = base.join(&d);
        let entries = names(&dir);
        for f in entries.iter().filter(|e| e.ends_with(".jsonl")) {
            let id = f[..f.len() - ".jsonl".len()].to_owned();
            let subagents = if entries.contains(&id) {
                names(&dir.join(&id).join("subagents")).iter().filter(|n| n.ends_with(".jsonl")).count()
            } else {
                0
            };
            files.push(ClaudeFile {
                path: dir.join(f),
                id,
                subagents,
            });
        }
    }
    files
}

/// Every `.jsonl` under `dir`, depth first, sorted by name.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for name in names(dir) {
        let p = dir.join(&name);
        if p.is_dir() {
            walk(&p, out);
        } else if name.ends_with(".jsonl") {
            out.push(p);
        }
    }
}

/// `f` over `items` on up to 8 threads, results in input order.
fn par_map<T: Send, R: Send>(items: Vec<T>, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    let len = items.len();
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8).min(len.max(1));
    let queue = Mutex::new(items.into_iter().enumerate());
    let out = Mutex::new(Vec::with_capacity(len));
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let next = queue.lock().unwrap_or_else(|e| e.into_inner()).next();
                let Some((i, item)) = next else { break };
                let r = f(item);
                out.lock().unwrap_or_else(|e| e.into_inner()).push((i, r));
            });
        }
    });
    let mut out = out.into_inner().unwrap_or_else(|e| e.into_inner());
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, r)| r).collect()
}

/// What is remembered between reads.
#[derive(Default)]
struct State {
    claude: HashMap<PathBuf, Tracked<ClaudeLog>>,
    codex: HashMap<PathBuf, Tracked<CodexLog>>,
    list: Option<(Instant, Arc<Vec<Session>>)>,
}

/// Reads every file again (as little as possible) and rebuilds the list.
fn refresh(config: &ReaderConfig, state: &mut State) -> Vec<Session> {
    let mut all = Vec::new();

    let mut memo = std::mem::take(&mut state.claude);
    let jobs: Vec<_> = claude_files(&config.claude_home)
        .into_iter()
        .map(|f| {
            let prev = memo.remove(&f.path);
            (f, prev)
        })
        .collect();
    for (f, tracked) in par_map(jobs, |(f, prev)| {
        let t = track::<ClaudeLog>(&f.path, prev).ok();
        (f, t)
    }) {
        let Some(t) = tracked else { continue };
        all.extend(t.current().session(&f.id, f.subagents));
        state.claude.insert(f.path, t);
    }

    let titles = codex_titles(&fs::read_to_string(config.codex_home.join("session_index.jsonl")).unwrap_or_default());
    let mut memo = std::mem::take(&mut state.codex);
    let mut files = Vec::new();
    walk(&config.codex_home.join("sessions"), &mut files);
    let jobs: Vec<_> = files
        .into_iter()
        .map(|p| {
            let prev = memo.remove(&p);
            (p, prev)
        })
        .collect();
    for (path, tracked) in par_map(jobs, |(p, prev)| {
        let t = track::<CodexLog>(&p, prev).ok();
        (p, t)
    }) {
        let Some(t) = tracked else { continue };
        let log = t.current();
        all.extend(log.session(log.id().and_then(|id| titles.get(id)).map(String::as_str)));
        state.codex.insert(path, t);
    }

    merge(all)
}

/// Codex can spread one thread over several files: they are merged. Most recent first.
pub(crate) fn merge(all: impl IntoIterator<Item = Session>) -> Vec<Session> {
    let mut merged: Vec<Session> = Vec::new();
    let mut index: HashMap<(Agent, String), usize> = HashMap::new();
    for s in all {
        let Some(&i) = index.get(&(s.agent, s.id.clone())) else {
            index.insert((s.agent, s.id.clone()), merged.len());
            merged.push(s);
            continue;
        };
        let prev = &mut merged[i];
        prev.start = prev.start.min(s.start);
        prev.end = prev.end.max(s.end);
        prev.turns += s.turns;
        prev.tokens = match (prev.tokens, s.tokens) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        };
        prev.model = prev.model.take().or(s.model);
        let mut seen: HashSet<i64> = prev.slots.iter().copied().collect();
        prev.slots.extend(s.slots.into_iter().filter(|x| seen.insert(*x)));
        prev.slots.sort_unstable();
        prev.active_ms = prev.slots.len() as i64 * SLOT_MS;
    }
    merged.sort_by_key(|s| std::cmp::Reverse(s.end));
    merged
}

/// Every Claude Code and Codex session of this Mac. Create one per server and share it.
pub struct SessionReader {
    config: ReaderConfig,
    state: tokio::sync::Mutex<State>,
}

impl SessionReader {
    pub fn new(config: ReaderConfig) -> Self {
        Self {
            config,
            state: Default::default(),
        }
    }

    pub fn config(&self) -> &ReaderConfig {
        &self.config
    }

    /// The sessions, most recent first, without `project` or `live` (see [`Self::list`]).
    /// Read again when the last read is older than the TTL.
    pub async fn sessions(&self) -> Arc<Vec<Session>> {
        let mut guard = self.state.lock().await;
        if let Some((at, list)) = &guard.list {
            if at.elapsed() < self.config.ttl {
                return list.clone();
            }
        }
        let mut state = std::mem::take(&mut *guard);
        let config = self.config.clone();
        let read = tokio::task::spawn_blocking(move || {
            let list = refresh(&config, &mut state);
            (state, list)
        })
        .await;
        let (state, list) = match read {
            Ok(done) => done,
            Err(e) => {
                tracing::warn!("reading agent sessions failed: {e}");
                (State::default(), Vec::new())
            }
        };
        *guard = state;
        let list = Arc::new(list);
        guard.list = Some((Instant::now(), list.clone()));
        list
    }

    /// The sessions with their project (from `projects`) and whether they are live at `now_ms`.
    pub async fn list(&self, projects: &[ProjectRoot], now_ms: i64) -> Vec<Session> {
        self.sessions()
            .await
            .iter()
            .map(|s| {
                let mut s = s.clone();
                s.project = project_of(&s.cwd, projects).map(|p| ProjectRef {
                    id: p.id.clone(),
                    title: p.title.clone(),
                });
                s.live = s.is_live_at(now_ms);
                s
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::parse::tests::{claude, CLAUDE, CODEX};

    fn append(path: &Path, text: &str) {
        let mut f = fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn reads_only_what_was_appended() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.jsonl");
        let (head, rest) = CLAUDE.split_at(CLAUDE.find("{\"type\":\"pr-link\"").unwrap());
        // The first read stops in the middle of a line.
        let cut = rest.find('\n').unwrap() / 2;
        append(&path, head);
        append(&path, &rest[..cut]);
        let t1 = track::<ClaudeLog>(&path, None).unwrap();
        assert_eq!(t1.offset, head.len() as u64);
        assert!(t1.pending.is_some(), "the unterminated line is read but not committed");

        append(&path, &rest[cut..]);
        let t2 = track::<ClaudeLog>(&path, Some(t1)).unwrap();
        assert_eq!(t2.offset, CLAUDE.len() as u64);
        assert!(t2.pending.is_none());
        assert_eq!(t2.current().session("s1", 0), claude(CLAUDE).session("s1", 0));

        // Unchanged: same state, nothing read.
        let t3 = track::<ClaudeLog>(&path, Some(t2.clone())).unwrap();
        assert_eq!(t3.offset, t2.offset);

        // Rewritten (the bytes before the offset differ): read from the start.
        let other = CLAUDE.replace("/p/acme/web", "/p/beta/web").replace("not json \"cwd\":\"", "not json at all");
        fs::write(&path, &other).unwrap();
        let t4 = track::<ClaudeLog>(&path, Some(t3)).unwrap();
        assert_eq!(t4.current().session("s1", 0).unwrap().cwd, "/p/beta/web");

        // Truncated: read from the start.
        fs::write(&path, &CLAUDE[..head.len()]).unwrap();
        let t5 = track::<ClaudeLog>(&path, Some(t4)).unwrap();
        assert!(t5.current().session("s1", 0).unwrap().prs.is_empty());
    }

    fn fixture() -> (tempfile::TempDir, ReaderConfig) {
        let dir = tempfile::tempdir().unwrap();
        let claude_home = dir.path().join("claude");
        let codex_home = dir.path().join("codex");
        let project = claude_home.join("projects/-p-acme");
        fs::create_dir_all(project.join("s1/subagents")).unwrap();
        fs::write(project.join("s1.jsonl"), CLAUDE).unwrap();
        fs::write(project.join("s1/subagents/agent-a.jsonl"), "").unwrap();
        fs::write(project.join("s1/subagents/agent-b.jsonl"), "").unwrap();
        fs::write(project.join("s1/subagents/notes.txt"), "").unwrap();
        fs::write(project.join("empty.jsonl"), "").unwrap();
        let day = codex_home.join("sessions/2026/10/01");
        fs::create_dir_all(&day).unwrap();
        fs::write(day.join("rollout-a.jsonl"), CODEX).unwrap();
        // The same thread, resumed in a second file an hour later.
        let resumed = CODEX.replace("T08:", "T10:").replace("T09:", "T11:");
        fs::write(day.join("rollout-b.jsonl"), resumed).unwrap();
        fs::write(codex_home.join("session_index.jsonl"), "{\"id\":\"c1\",\"thread_name\":\"Desk tidy\"}\n").unwrap();
        let config = ReaderConfig {
            claude_home,
            codex_home,
            ttl: Duration::from_secs(60),
        };
        (dir, config)
    }

    #[tokio::test]
    async fn lists_every_session_of_both_agents() {
        let (_dir, config) = fixture();
        let reader = SessionReader::new(config);
        let projects = [
            ProjectRoot {
                id: "p-acme".into(),
                title: "Acme".into(),
                workspace_root: "/p/acme".into(),
            },
            ProjectRoot {
                id: "p-zen".into(),
                title: "Zen".into(),
                workspace_root: "/p/zen".into(),
            },
        ];
        let now = crate::parse::ms("2026-10-01T10:33:00Z").unwrap();
        let list = reader.list(&projects, now).await;
        assert_eq!(list.len(), 2, "{list:#?}");
        // Most recent first: the Codex thread ends at 11:20.
        let (codex, claude) = (&list[0], &list[1]);
        assert_eq!((codex.agent, codex.id.as_str(), codex.title.as_str()), (Agent::Codex, "c1", "Desk tidy"));
        assert_eq!(codex.project.as_ref().map(|p| p.title.as_str()), Some("Zen"));
        assert_eq!((codex.turns, codex.tokens), (2, Some(2 * 18566)));
        assert_eq!(codex.start, crate::parse::ms("2026-10-01T08:50:48.376Z").unwrap());
        assert_eq!((claude.agent, claude.subagents), (Agent::Claude, 2));
        assert_eq!(claude.project.as_ref().map(|p| p.id.as_str()), Some("p-acme"));
        assert!(claude.live, "wrote at 10:32, read at 10:33");
        assert!(!codex.live || codex.end > now);

        // Cached: a new file shows up only after the TTL.
        let day = reader.config().codex_home.join("sessions/2026/10/01");
        fs::write(day.join("rollout-c.jsonl"), CODEX.replace("\"c1\"", "\"c2\"")).unwrap();
        assert_eq!(reader.sessions().await.len(), 2);
        reader.state.lock().await.list = None;
        assert_eq!(reader.sessions().await.len(), 3);
    }

    #[tokio::test]
    async fn missing_folders_mean_no_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let reader = SessionReader::new(ReaderConfig {
            claude_home: dir.path().join("nope"),
            codex_home: dir.path().join("nope"),
            ttl: Duration::ZERO,
        });
        assert!(reader.sessions().await.is_empty());
    }
}
