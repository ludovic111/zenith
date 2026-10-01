//! `MediaFile.ts`: opening a canonical media path exactly once, so the response is read from
//! the file that was validated, never from whatever a swapped path points at later.
//!
//! The open is no-follow and non-blocking (a path swapped for a FIFO cannot hang the request),
//! and the descriptor must be the same file (device and inode) as the path before and after
//! the open, with the path still canonical. A signed `media-file-exact` URL also pins the
//! identity it was minted for.

use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};

/// A file's identity, as the decimal strings the signed claims carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIdentity {
    pub device: String,
    pub inode: String,
}

/// The validated descriptor of one request, never a copy of its bytes.
#[derive(Debug)]
pub struct OpenMediaFile {
    pub file: File,
    pub device: u64,
    pub inode: u64,
}

impl OpenMediaFile {
    pub fn identity(&self) -> FileIdentity {
        FileIdentity {
            device: self.device.to_string(),
            inode: self.inode.to_string(),
        }
    }

    /// `statMediaFile`: the metadata of the open descriptor.
    pub fn stat(&self) -> io::Result<Metadata> {
        self.file.metadata()
    }

    /// `readMediaFileHeader`: up to `byte_count` leading bytes, never past the end.
    pub fn read_header(&self, byte_count: usize) -> io::Result<Vec<u8>> {
        let mut buffer = vec![0u8; byte_count];
        let mut filled = 0;
        while filled < byte_count {
            let read = self.file.read_at(&mut buffer[filled..], filled as u64)?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        buffer.truncate(filled);
        Ok(buffer)
    }
}

fn matches_identity(metadata: &Metadata, identity: Option<&FileIdentity>) -> bool {
    identity.is_none_or(|identity| metadata.dev().to_string() == identity.device && metadata.ino().to_string() == identity.inode)
}

/// `openMediaFile`: `Ok(None)` when the path is not (or no longer) the regular file it claims
/// to be. Blocking: call it from `spawn_blocking`.
pub fn open_media_file(file_path: &str, identity: Option<&FileIdentity>) -> io::Result<Option<OpenMediaFile>> {
    let before = std::fs::symlink_metadata(file_path)?;
    if !before.file_type().is_file() || before.ino() == 0 || !matches_identity(&before, identity) {
        return Ok(None);
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(file_path)?;
    // Dropping `file` on any rejection closes the descriptor.
    let info = file.metadata()?;
    if !info.file_type().is_file() || info.dev() != before.dev() || info.ino() != before.ino() || !matches_identity(&info, identity) {
        return Ok(None);
    }
    if std::fs::canonicalize(file_path)?.to_string_lossy() != file_path {
        return Ok(None);
    }
    let after = std::fs::symlink_metadata(file_path)?;
    if !after.file_type().is_file() || info.dev() != after.dev() || info.ino() != after.ino() {
        return Ok(None);
    }
    Ok(Some(OpenMediaFile {
        device: info.dev(),
        inode: info.ino(),
        file,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_regular_canonical_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let file_path = root.join("clip.mp4");
        std::fs::write(&file_path, b"0123456789").unwrap();
        let path = file_path.to_string_lossy().into_owned();

        let opened = open_media_file(&path, None).unwrap().expect("a regular file opens");
        assert_eq!(opened.read_header(4).unwrap(), b"0123");
        assert_eq!(opened.read_header(64).unwrap(), b"0123456789");
        let identity = opened.identity();
        assert!(open_media_file(&path, Some(&identity)).unwrap().is_some());
        let other = FileIdentity {
            device: identity.device.clone(),
            inode: "1".into(),
        };
        assert!(open_media_file(&path, Some(&other)).unwrap().is_none());

        let alias = root.join("alias.mp4");
        std::os::unix::fs::symlink(&file_path, &alias).unwrap();
        assert!(open_media_file(&alias.to_string_lossy(), None).unwrap().is_none());

        let directory = root.join("directory.mp4");
        std::fs::create_dir(&directory).unwrap();
        assert!(open_media_file(&directory.to_string_lossy(), None).unwrap().is_none());
    }
}
