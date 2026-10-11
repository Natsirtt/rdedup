//! Directory iteration retains its originating operation without collecting a tree.
use super::LocalProtection;
use std::fs::{self, ReadDir};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use walkdir::{IntoIter, WalkDir};

/// The filesystem entry kinds that participate in the repository layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// An ordinary file; symbolic links are not followed or included.
    File,
    /// An ordinary directory.
    Directory,
}

/// One native repository-relative entry produced by protected traversal.
#[derive(Debug)]
pub struct DirectoryEntry {
    path: PathBuf,
    kind: EntryKind,
}

impl DirectoryEntry {
    /// Native path relative to the protected repository root.
    pub fn as_path(&self) -> &Path {
        &self.path
    }

    /// Whether this entry is an ordinary file or directory.
    pub fn kind(&self) -> EntryKind {
        self.kind
    }
}

enum Entries {
    Immediate(Box<ReadDir>),
    Recursive(IntoIter),
}

/// Incremental directory traversal whose lifetime retains repository protection.
///
/// Immediate traversal includes files and directories. Recursive traversal
/// includes files only, matching the backend's recursive listing contract.
/// Symbolic links and special files are excluded in both modes.
pub struct StoredEntries {
    entries: Entries,
    protection: Arc<LocalProtection>,
}

impl StoredEntries {
    pub(super) fn immediate(
        protection: Arc<LocalProtection>,
        path: &Path,
    ) -> io::Result<Self> {
        let entries = Entries::Immediate(Box::new(fs::read_dir(
            directory_path(&protection, path)?,
        )?));
        Ok(Self {
            entries,
            protection,
        })
    }

    pub(super) fn recursive(
        protection: Arc<LocalProtection>,
        path: &Path,
    ) -> io::Result<Self> {
        let entries = Entries::Recursive(
            WalkDir::new(directory_path(&protection, path)?).into_iter(),
        );
        Ok(Self {
            entries,
            protection,
        })
    }

    fn relative(
        &self,
        path: PathBuf,
        kind: EntryKind,
    ) -> io::Result<DirectoryEntry> {
        let path = path
            .strip_prefix(&self.protection.path)
            .map_err(io::Error::other)?
            .to_owned();
        Ok(DirectoryEntry { path, kind })
    }
}

impl Iterator for StoredEntries {
    type Item = io::Result<DirectoryEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match &mut self.entries {
                Entries::Immediate(entries) => {
                    let entry = match entries.next()? {
                        Ok(entry) => entry,
                        Err(error) => return Some(Err(error)),
                    };
                    let kind = match entry.file_type() {
                        Ok(kind) if kind.is_file() => EntryKind::File,
                        Ok(kind) if kind.is_dir() => EntryKind::Directory,
                        Ok(_) => continue,
                        Err(error) => return Some(Err(error)),
                    };
                    return Some(self.relative(entry.path(), kind));
                }
                Entries::Recursive(entries) => {
                    let entry = match entries.next()? {
                        Ok(entry) => entry,
                        Err(error) => return Some(Err(error.into())),
                    };
                    if entry.file_type().is_file() {
                        return Some(
                            self.relative(entry.into_path(), EntryKind::File),
                        );
                    }
                }
            }
        }
    }
}

fn directory_path(
    protection: &LocalProtection,
    path: &Path,
) -> io::Result<PathBuf> {
    let path = protection.path.join(path);
    if !fs::symlink_metadata(&path)?.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "listing requires an ordinary directory",
        ));
    }
    Ok(path)
}
