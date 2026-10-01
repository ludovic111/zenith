//! The file system as the scanner sees it (Effect's `FileSystem` on Node), behind a trait so
//! tests can observe and bend it the way the TS tests do (count opens and reads, alias stats,
//! serve synthetic directory listings).

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// What `fs.stat` reports, the way the scanner reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    pub is_file: bool,
    pub is_dir: bool,
    pub size: u64,
    /// Whole milliseconds (Effect stats with `bigint: true`, so truncated, not rounded).
    pub mtime_ms: Option<i64>,
    pub birthtime_ms: Option<i64>,
    pub dev: u64,
    pub ino: u64,
}

impl FileStat {
    pub fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            is_file: metadata.is_file(),
            is_dir: metadata.is_dir(),
            size: metadata.len(),
            mtime_ms: millis(metadata.modified()),
            birthtime_ms: Some(millis(metadata.created()).unwrap_or(0)),
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

fn millis(time: std::io::Result<std::time::SystemTime>) -> Option<i64> {
    let time = time.ok()?;
    Some(match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => (elapsed.as_nanos() / 1_000_000) as i64,
        Err(before) => -((before.duration().as_nanos() / 1_000_000) as i64),
    })
}

/// An open file (`FileSystem.File`).
pub trait ScanFile: Send {
    /// `readAlloc(size)`: up to `size` bytes, `None` at the end of the file.
    fn read_alloc(&mut self, size: usize) -> std::io::Result<Option<Vec<u8>>>;
    /// `file.stat`.
    fn stat(&self) -> std::io::Result<FileStat>;
}

/// The operations the scanner performs.
pub trait ScanFileSystem: Send + Sync {
    /// `readDirectory`: entry names, in the order Node gives them (sorted by bytes).
    fn read_directory(&self, directory: &Path) -> std::io::Result<Vec<String>>;
    /// `stat` (follows symlinks).
    fn stat(&self, path: &Path) -> std::io::Result<FileStat>;
    fn real_path(&self, path: &Path) -> std::io::Result<PathBuf>;
    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>>;
    fn open(&self, path: &Path) -> std::io::Result<Box<dyn ScanFile>>;
}

/// The real file system.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealFileSystem;

struct RealFile(File);

impl ScanFile for RealFile {
    fn read_alloc(&mut self, size: usize) -> std::io::Result<Option<Vec<u8>>> {
        let mut buffer = vec![0u8; size];
        let read = self.0.read(&mut buffer)?;
        if read == 0 {
            return Ok(None);
        }
        buffer.truncate(read);
        Ok(Some(buffer))
    }

    fn stat(&self) -> std::io::Result<FileStat> {
        Ok(FileStat::from_metadata(&self.0.metadata()?))
    }
}

impl ScanFileSystem for RealFileSystem {
    fn read_directory(&self, directory: &Path) -> std::io::Result<Vec<String>> {
        use std::os::unix::ffi::OsStrExt;
        let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(directory)?
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .collect();
        // libuv's scandir sorts with strcmp.
        names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        Ok(names.into_iter().map(|name| name.to_string_lossy().into_owned()).collect())
    }

    fn stat(&self, path: &Path) -> std::io::Result<FileStat> {
        Ok(FileStat::from_metadata(&std::fs::metadata(path)?))
    }

    fn real_path(&self, path: &Path) -> std::io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn open(&self, path: &Path) -> std::io::Result<Box<dyn ScanFile>> {
        Ok(Box::new(RealFile(File::open(path)?)))
    }
}
