//! File streams retain their originating operation and publish through a temporary entry.
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::LocalProtection;
use rand::distr::Alphanumeric;
use rand::Rng;

/// Failures preparing, opening, or atomically publishing a stored object.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Temporary object storage could not be prepared.
    #[error("could not stage object at {path}")]
    Prepare {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// A stored object could not be opened.
    #[error("could not open object at {path}")]
    Open {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The staged bytes could not be synchronized before publication.
    #[error("could not synchronize object at {path}")]
    Synchronize {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The final object entry could not be published.
    #[error("could not publish object at {path}")]
    Publish {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl From<Error> for io::Error {
    fn from(error: Error) -> Self {
        let kind = match &error {
            Error::Prepare { source, .. }
            | Error::Open { source, .. }
            | Error::Synchronize { source, .. }
            | Error::Publish { source, .. } => source.kind(),
        };
        io::Error::new(kind, error)
    }
}

/// A file reader retaining repository protection through its last read.
pub struct StoredObject {
    file: File,
    _protection: Arc<LocalProtection>,
}

impl StoredObject {
    pub(super) fn open(
        protection: Arc<LocalProtection>,
        path: &Path,
    ) -> Result<Self, Error> {
        let path = protection.path.join(path);
        let file =
            File::open(&path).map_err(|source| Error::Open { path, source })?;
        Ok(Self {
            file,
            _protection: protection,
        })
    }
}

impl Read for StoredObject {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

pub(super) enum PublicationMode {
    Create,
    Replace,
}

/// Result of publishing a fully staged object.
#[derive(Debug, PartialEq, Eq)]
pub enum Publication {
    /// The complete staged bytes are visible at the destination.
    Published,
    /// An existing file was retained; logical identity is checked by the caller.
    AlreadyExists,
}

struct TemporaryPath(PathBuf);
impl Drop for TemporaryPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// An unpublished file whose cancellation removes its temporary entry.
///
/// The file closes before temporary-path cleanup and repository protection
/// release. Commit consumes the staging handle; it cannot be reused.
pub struct PendingObject {
    file: File,
    staging: StagedEntry,
    destination: PathBuf,
    mode: PublicationMode,
}

// Field drop order keeps protection until temporary cleanup also finishes.
struct StagedEntry {
    temporary: TemporaryPath,
    _protection: Arc<LocalProtection>,
}

impl PendingObject {
    pub(super) fn new(
        protection: Arc<LocalProtection>,
        path: &Path,
        mode: PublicationMode,
    ) -> Result<Self, Error> {
        let destination = protection.path.join(path);
        let prepare = |source| Error::Prepare {
            path: destination.clone(),
            source,
        };
        let parent = destination.parent().ok_or_else(|| {
            prepare(io::Error::new(
                io::ErrorKind::InvalidInput,
                "object path has no parent",
            ))
        })?;
        fs::create_dir_all(parent).map_err(prepare)?;
        let suffix: String = rand::rng()
            .sample_iter(&Alphanumeric)
            .take(20)
            .map(char::from)
            .collect();
        let temporary = destination.with_extension(format!("{suffix}.tmp"));
        // create_new prevents a temporary-name collision from truncating data.
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(prepare)?;
        Ok(Self {
            file,
            staging: StagedEntry {
                temporary: TemporaryPath(temporary),
                _protection: protection,
            },
            destination,
            mode,
        })
    }

    /// Synchronize and atomically publish the completed object.
    ///
    /// # Errors
    /// Returns synchronization or publication failures. An existing destination
    /// is retained for create operations and reported as `AlreadyExists`.
    pub fn commit(self) -> Result<Publication, Error> {
        self.synchronize()?.publish()
    }

    /// Finish file I/O without publishing, allowing a final lease check.
    ///
    /// # Errors
    /// Returns a synchronization failure; cancellation cleans the temporary entry.
    pub fn synchronize(self) -> Result<PreparedObject, Error> {
        let Self {
            file,
            staging,
            destination,
            mode,
        } = self;
        let synchronized = file.sync_data();
        // Close before any fallible return so Windows permits temporary cleanup.
        drop(file);
        synchronized.map_err(|source| Error::Synchronize {
            path: destination.clone(),
            source,
        })?;
        Ok(PreparedObject {
            staging,
            destination,
            mode,
        })
    }
}

/// Synchronized, closed staging entry retaining its repository protection.
/// Dropping it cancels publication and removes the temporary entry.
pub struct PreparedObject {
    staging: StagedEntry,
    destination: PathBuf,
    mode: PublicationMode,
}

impl PreparedObject {
    /// Atomically publish the synchronized entry without another file-data wait.
    ///
    /// # Errors
    /// Returns a publication failure; an existing create destination is retained.
    pub fn publish(self) -> Result<Publication, Error> {
        let Self {
            staging,
            destination,
            mode,
        } = self;
        let result = match mode {
            PublicationMode::Create => {
                match fs::hard_link(&staging.temporary.0, &destination) {
                    Ok(()) => Ok(Publication::Published),
                    Err(error)
                        if error.kind() == io::ErrorKind::AlreadyExists =>
                    {
                        match fs::metadata(&destination) {
                            Ok(metadata) if metadata.is_file() => {
                                Ok(Publication::AlreadyExists)
                            }
                            Ok(_) => Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "object destination is not a file",
                            )),
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                }
            }
            PublicationMode::Replace => {
                fs::rename(&staging.temporary.0, &destination)
                    .map(|()| Publication::Published)
            }
        };
        result.map_err(|source| Error::Publish {
            path: destination,
            source,
        })
    }
}

impl Write for PendingObject {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.file.write(buffer)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
