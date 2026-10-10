//! Validated locations of content-addressed chunks in the repository layout.
use std::path::{Component, Path, PathBuf};

use crate::{config, Digest, Generation, DIGEST_SIZE};

/// An input does not describe a canonical repository chunk location.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The location has an invalid generation, namespace, or nesting layout.
    #[error("invalid chunk location: {path}")]
    InvalidLocation { path: PathBuf },
    /// The final component cannot represent a complete digest.
    #[error("invalid chunk digest")]
    Digest(#[source] hex::FromHexError),
}

/// A content-addressed chunk location with a validated generation and layout.
///
/// Parsing uses the same nesting encoder that constructs repository paths.
/// Backend operations do not need to know digest widths or directory spelling.
#[derive(Clone, Debug)]
pub struct ChunkPath {
    path: PathBuf,
    generation: Generation,
}

impl ChunkPath {
    /// Validate a canonical native repository-relative chunk path.
    ///
    /// # Errors
    /// Rejects other repository namespaces, invalid digests, traversal, and
    /// directory prefixes that do not match the configured chunk layout rules.
    pub fn new(path: PathBuf) -> Result<Self, Error> {
        let invalid = || Error::InvalidLocation { path: path.clone() };
        let parts = path
            .components()
            .map(|component| match component {
                Component::Normal(part) => part.to_str().ok_or_else(invalid),
                _ => Err(invalid()),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if parts.len() < 3 || parts[1] != config::DATA_SUBDIR {
            return Err(invalid());
        }
        let generation =
            Generation::try_from(parts[0]).map_err(|_| invalid())?;
        let nesting_depth = parts.len() - 3;
        if nesting_depth >= DIGEST_SIZE {
            return Err(invalid());
        }
        let nesting = config::Nesting(
            u8::try_from(nesting_depth).map_err(|_| invalid())?,
        );
        let digest =
            Digest::from_hex(parts[parts.len() - 1]).map_err(Error::Digest)?;
        let canonical = nesting.get_path(
            Path::new(config::DATA_SUBDIR),
            digest.as_digest_ref().0,
            &generation.to_string(),
        );
        if canonical != path {
            return Err(invalid());
        }
        Ok(Self { path, generation })
    }

    /// Canonical native path relative to the repository root.
    pub fn as_path(&self) -> &Path {
        &self.path
    }

    /// Whether two locations preserve the chunk identity and nesting layout.
    pub fn has_same_chunk_layout(&self, other: &Self) -> bool {
        self.path
            .components()
            .skip(1)
            .eq(other.path.components().skip(1))
    }

    /// Whether this chunk location belongs to a strictly older generation.
    pub fn is_before(&self, other: &Self) -> bool {
        self.generation < other.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_locations_round_trip_through_the_repository_nesting_encoder() {
        let digest = [0xab; DIGEST_SIZE];
        let generation = "0000000000000001-0000000000000000";
        for level in [0, 2, 31] {
            let path = config::Nesting(level).get_path(
                Path::new(config::DATA_SUBDIR),
                &digest,
                generation,
            );
            assert_eq!(ChunkPath::new(path.clone()).unwrap().as_path(), path);
        }
    }

    #[test]
    fn a_chunk_location_rejects_truncated_digests_and_mismatched_nesting() {
        let generation = "0000000000000001-0000000000000000";
        for suffix in [
            "name/archive.yml".to_owned(),
            "chunk/ab".to_owned(),
            format!("chunk/cd/{}", "ab".repeat(DIGEST_SIZE)),
            format!("chunk/{}", "AB".repeat(DIGEST_SIZE)),
        ] {
            assert!(
                ChunkPath::new(PathBuf::from(generation).join(suffix)).is_err()
            );
        }
    }
}
