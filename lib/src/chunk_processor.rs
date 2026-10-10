//! Chunk storage jobs acknowledge only complete, reusable chunks.
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;

use sgdata::SGData;

use super::aio;
use super::{DataType, Repo};
use crate::compression::ArcCompression;
use crate::encryption::ArcEncrypter;
use crate::hashing::ArcHasher;
use crate::{Digest, Generation};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("chunk metadata failed at {path}")]
    Metadata {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("chunk promotion failed at {path}")]
    Promotion {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("chunk compression failed")]
    Compression(#[source] io::Error),
    #[error("chunk encryption failed")]
    Encryption(#[source] io::Error),
    #[error("chunk upload failed at {path}")]
    Upload {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl From<Error> for io::Error {
    fn from(error: Error) -> Self {
        let kind = match &error {
            Error::Metadata { source, .. }
            | Error::Promotion { source, .. }
            | Error::Upload { source, .. }
            | Error::Compression(source)
            | Error::Encryption(source) => source.kind(),
        };
        io::Error::new(kind, error)
    }
}

/// Position within one ordered stream of data or index chunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ChunkSequence(u64);

impl ChunkSequence {
    pub(crate) fn new(index: u64) -> Self {
        Self(index)
    }
    pub(crate) fn as_index(self) -> u64 {
        self.0
    }
}

/// One storage job, with ownership of its contents and completion destination.
pub(crate) struct Message {
    pub sequence: ChunkSequence,
    pub contents: SGData,
    pub data_type: DataType,
    pub response: mpsc::Sender<Completion>,
}

/// Completion reports storage failure instead of acknowledging a missing chunk.
pub(crate) struct Completion {
    pub sequence: ChunkSequence,
    pub result: Result<Digest, Error>,
}

pub(crate) struct ChunkProcessor {
    repo: Repo,
    receiver: crossbeam_channel::Receiver<Message>,
    aio: aio::AsyncIO,
    encrypter: ArcEncrypter,
    compressor: ArcCompression,
    hasher: ArcHasher,
    generations: Vec<Generation>,
}

impl ChunkProcessor {
    pub fn new(
        repo: Repo,
        receiver: crossbeam_channel::Receiver<Message>,
        aio: aio::AsyncIO,
        encrypter: ArcEncrypter,
        compressor: ArcCompression,
        hasher: ArcHasher,
        generations: Vec<Generation>,
    ) -> Self {
        assert!(!generations.is_empty());
        Self {
            repo,
            receiver,
            aio,
            encrypter,
            compressor,
            hasher,
            generations,
        }
    }

    pub fn run(&self) {
        while let Ok(message) = self.receiver.recv() {
            let result = self.store(message.contents, message.data_type);
            if message
                .response
                .send(Completion {
                    sequence: message.sequence,
                    result,
                })
                .is_err()
            {
                // The index consumer has stopped; closing this worker's input
                // lets the bounded producer unwind without a separate flag.
                return;
            }
        }
    }

    fn store(
        &self,
        contents: SGData,
        data_type: DataType,
    ) -> Result<Digest, Error> {
        let digest = Digest(self.hasher.calculate_digest(&contents));
        // Construction requires at least one generation.
        let current = self
            .generations
            .last()
            .expect("a chunk processor requires a generation")
            .to_string();
        let destination = self
            .repo
            .chunk_rel_path_by_digest(digest.as_digest_ref(), &current);
        for generation in self.generations.iter().rev() {
            let path = self.repo.chunk_rel_path_by_digest(
                digest.as_digest_ref(),
                &generation.to_string(),
            );
            match self.aio.read_metadata(path.clone()).wait() {
                Ok(metadata) => {
                    if !metadata.is_file {
                        return Err(Error::Metadata {
                            path,
                            source: io::Error::new(
                                io::ErrorKind::InvalidData,
                                "chunk is not a file",
                            ),
                        });
                    }
                    if path != destination {
                        let promotion =
                            crate::backends::ChunkPromotion::from_paths(
                                path,
                                destination.clone(),
                            )
                            .map_err(|error| {
                                Error::Promotion {
                                    path: destination.clone(),
                                    source: io::Error::new(
                                        io::ErrorKind::InvalidInput,
                                        error,
                                    ),
                                }
                            })?;
                        self.aio.promote_chunk(promotion).wait().map_err(
                            |source| Error::Promotion {
                                path: destination.clone(),
                                source,
                            },
                        )?;
                    }
                    return Ok(digest);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => return Err(Error::Metadata { path, source }),
            }
        }
        let contents = if data_type.should_compress() {
            self.compressor
                .compress(contents)
                .map_err(Error::Compression)?
        } else {
            contents
        };
        let contents = if data_type.should_encrypt() {
            self.encrypter
                .encrypt(contents, &digest.0)
                .map_err(Error::Encryption)?
        } else {
            contents
        };
        self.aio
            .write_idempotent(destination.clone(), contents)
            .wait()
            .map_err(|source| Error::Upload {
                path: destination,
                source,
            })?;
        Ok(digest)
    }
}
