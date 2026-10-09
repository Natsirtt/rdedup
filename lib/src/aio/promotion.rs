//! Additive publication of an existing chunk in a later generation.
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::{Generation, DIGEST_SIZE};

/// A rejected promotion request.
#[derive(Debug, Error)]
pub enum Error {
    /// A path does not identify a repository chunk.
    #[error("invalid chunk path: {path}")]
    InvalidPath { path: PathBuf },
    /// The paths identify different chunks or nesting layouts.
    #[error("promotion must preserve the chunk digest and nesting")]
    DifferentChunk,
    /// A promotion must advance the generation.
    #[error("promotion destination must be a newer generation")]
    NotForward,
}

/// The same content-addressed chunk in two ordered generations.
///
/// Promotion makes the destination available without removing the source.
/// Shared readers can therefore continue using either generation. Exclusive GC
/// removes the older entry when reclaiming that generation.
#[derive(Clone, Debug)]
pub struct ChunkPromotion {
    source: PathBuf,
    destination: PathBuf,
}

impl ChunkPromotion {
    /// Validate paths describing one chunk moving forward through generations.
    ///
    /// # Errors
    /// Rejects malformed paths, different digests/nesting, and a destination
    /// which is not newer than the source.
    pub fn new(source: PathBuf, destination: PathBuf) -> Result<Self, Error> {
        let source_generation = generation(&source)?;
        let destination_generation = generation(&destination)?;
        if source
            .components()
            .skip(1)
            .ne(destination.components().skip(1))
        {
            return Err(Error::DifferentChunk);
        }
        if source_generation >= destination_generation {
            return Err(Error::NotForward);
        }
        Ok(Self {
            source,
            destination,
        })
    }

    /// Repository-relative location of the already stored chunk.
    pub fn source_path(&self) -> &Path {
        &self.source
    }

    /// Repository-relative location to populate without replacing existing data.
    pub fn destination_path(&self) -> &Path {
        &self.destination
    }
}

fn generation(path: &Path) -> Result<Generation, Error> {
    let invalid = || Error::InvalidPath {
        path: path.to_path_buf(),
    };
    let parts = path
        .components()
        .map(|component| match component {
            Component::Normal(part) => part.to_str().ok_or_else(invalid),
            _ => Err(invalid()),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if parts.len() < 3 || parts.len() > DIGEST_SIZE + 2 || parts[1] != "chunk" {
        return Err(invalid());
    }
    let digest = parts[parts.len() - 1];
    if digest.len() != DIGEST_SIZE * 2
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid());
    }
    for (level, directory) in parts[2..parts.len() - 1].iter().enumerate() {
        if *directory != &digest[level * 2..level * 2 + 2] {
            return Err(invalid());
        }
    }
    Generation::try_from(parts[0]).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::{ChunkPromotion, Error};
    use std::path::PathBuf;

    fn path(generation: u64, digest: &str) -> PathBuf {
        PathBuf::from(format!(
            "{generation:016x}-0000000000000000/chunk/{}/{}",
            &digest[..2],
            digest
        ))
    }

    #[test]
    fn promotion_rejects_different_content_addresses() {
        let source = path(0, &"ab".repeat(32));
        let destination = path(1, &"cd".repeat(32));
        assert!(matches!(
            ChunkPromotion::new(source, destination),
            Err(Error::DifferentChunk)
        ));
    }

    #[test]
    fn promotion_rejects_names_and_parent_paths() {
        let destination = path(1, &"ab".repeat(32));
        for source in [
            "../chunk/value",
            "0000000000000000-0000000000000000/name/archive.yml",
        ] {
            assert!(matches!(
                ChunkPromotion::new(source.into(), destination.clone()),
                Err(Error::InvalidPath { .. })
            ));
        }
    }

    #[test]
    fn promotion_requires_a_newer_generation() {
        let digest = "ab".repeat(32);
        assert!(matches!(
            ChunkPromotion::new(path(1, &digest), path(0, &digest)),
            Err(Error::NotForward)
        ));
        assert!(matches!(
            ChunkPromotion::new(path(1, &digest), path(1, &digest)),
            Err(Error::NotForward)
        ));
        assert!(ChunkPromotion::new(path(0, &digest), path(1, &digest)).is_ok());
    }
}
