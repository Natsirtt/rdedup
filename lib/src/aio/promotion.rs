//! Additive publication of an existing chunk in a later generation.
use std::path::{Path, PathBuf};

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::chunk_path::ChunkPath;

/// A rejected promotion request.
#[derive(Debug, Error)]
pub enum Error {
    /// A path does not identify a repository chunk.
    #[error("invalid chunk path")]
    InvalidPath(#[source] crate::chunk_path::Error),
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
#[derive(Clone, Debug, Serialize)]
pub struct ChunkPromotion {
    source: ChunkPath,
    destination: ChunkPath,
}

impl ChunkPromotion {
    /// Validate paths describing one chunk moving forward through generations.
    ///
    /// # Errors
    /// Rejects malformed paths, different digests/nesting, and a destination
    /// which is not newer than the source.
    pub fn from_paths(
        source: PathBuf,
        destination: PathBuf,
    ) -> Result<Self, Error> {
        let source = ChunkPath::new(source).map_err(Error::InvalidPath)?;
        let destination =
            ChunkPath::new(destination).map_err(Error::InvalidPath)?;
        Self::new(source, destination)
    }

    /// Promote one validated chunk location into a later generation.
    ///
    /// # Errors
    /// Rejects different chunk identities/layouts or a non-forward generation.
    pub fn new(
        source: ChunkPath,
        destination: ChunkPath,
    ) -> Result<Self, Error> {
        if !source.has_same_chunk_layout(&destination) {
            return Err(Error::DifferentChunk);
        }
        if !source.is_before(&destination) {
            return Err(Error::NotForward);
        }
        Ok(Self {
            source,
            destination,
        })
    }

    /// Repository-relative location of the already stored chunk.
    pub fn source_path(&self) -> &Path {
        self.source.as_path()
    }

    /// Repository-relative location to populate without replacing existing data.
    pub fn destination_path(&self) -> &Path {
        self.destination.as_path()
    }
}

impl<'de> Deserialize<'de> for ChunkPromotion {
    fn deserialize<D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Self, D::Error> {
        struct PromotionVisitor;
        impl<'de> Visitor<'de> for PromotionVisitor {
            type Value = ChunkPromotion;
            fn expecting(
                &self,
                formatter: &mut std::fmt::Formatter<'_>,
            ) -> std::fmt::Result {
                formatter.write_str("source and destination paths for a forward chunk promotion")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut source = None;
                let mut destination = None;
                while let Some(field) = map.next_key::<String>()? {
                    match field.as_str() {
                        "source" => {
                            if source.is_some() {
                                return Err(de::Error::duplicate_field(
                                    "source",
                                ));
                            }
                            source = Some(map.next_value()?);
                        }
                        "destination" => {
                            if destination.is_some() {
                                return Err(de::Error::duplicate_field(
                                    "destination",
                                ));
                            }
                            destination = Some(map.next_value()?);
                        }
                        _ => {
                            return Err(de::Error::unknown_field(
                                &field,
                                &["source", "destination"],
                            ))
                        }
                    }
                }
                let source =
                    source.ok_or_else(|| de::Error::missing_field("source"))?;
                let destination = destination
                    .ok_or_else(|| de::Error::missing_field("destination"))?;
                ChunkPromotion::new(source, destination)
                    .map_err(de::Error::custom)
            }
        }
        deserializer.deserialize_map(PromotionVisitor)
    }
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
    fn wire_promotions_preserve_paths_and_reject_invalid_semantics() {
        let promotion = ChunkPromotion::from_paths(
            path(0, &"ab".repeat(32)),
            path(1, &"ab".repeat(32)),
        )
        .unwrap();
        let encoded = serde_json::to_string(&promotion).unwrap();
        assert!(!encoded.contains('\\'));
        let decoded: ChunkPromotion = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.source_path(), promotion.source_path());
        assert_eq!(decoded.destination_path(), promotion.destination_path());
        let source = format!(
            "0000000000000000-0000000000000000/chunk/ab/{}",
            "ab".repeat(32)
        );
        let destination = format!(
            "0000000000000001-0000000000000000/chunk/ab/{}",
            "ab".repeat(32)
        );
        for invalid in [
            format!(r#"{{"source":"{destination}","destination":"{source}"}}"#),
            format!(
                r#"{{"source":"{source}","source":"{source}","destination":"{destination}"}}"#
            ),
            format!(
                r#"{{"source":"{source}","destination":"{destination}","extra":true}}"#
            ),
            format!(
                r#"{{"source":"{}","destination":"{destination}"}}"#,
                source.replace("/chunk/", "//chunk/")
            ),
        ] {
            assert!(
                serde_json::from_str::<ChunkPromotion>(&invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn promotion_rejects_different_content_addresses() {
        let source = path(0, &"ab".repeat(32));
        let destination = path(1, &"cd".repeat(32));
        assert!(matches!(
            ChunkPromotion::from_paths(source, destination),
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
                ChunkPromotion::from_paths(source.into(), destination.clone()),
                Err(Error::InvalidPath(_))
            ));
        }
    }

    #[test]
    fn promotion_requires_a_newer_generation() {
        let digest = "ab".repeat(32);
        assert!(matches!(
            ChunkPromotion::from_paths(path(1, &digest), path(0, &digest)),
            Err(Error::NotForward)
        ));
        assert!(matches!(
            ChunkPromotion::from_paths(path(1, &digest), path(1, &digest)),
            Err(Error::NotForward)
        ));
        assert!(
            ChunkPromotion::from_paths(path(0, &digest), path(1, &digest))
                .is_ok()
        );
    }
}
