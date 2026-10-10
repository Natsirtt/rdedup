//! Repository layout admission, separate from transport path encoding.
use std::path::{Component, Path, PathBuf};

use crate::{chunk_path::ChunkPath, config, generation, name, Generation};

/// A path does not identify an admitted repository object or directory.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Paths must consist entirely of normal UTF-8 relative components.
    #[error("invalid repository-relative path")]
    RelativePath,
    /// The path is outside the repository's published layout.
    #[error("path is not a repository object or directory")]
    Layout,
}

fn parts(path: &Path) -> Result<Vec<&str>, Error> {
    path.components()
        .map(|component| match component {
            Component::Normal(part) => part.to_str().ok_or(Error::RelativePath),
            _ => Err(Error::RelativePath),
        })
        .collect()
}

fn generation_directory(part: &str) -> bool {
    Generation::try_from(part)
        .is_ok_and(|generation| generation.to_string() == part)
}

/// A published configuration, name, or content-addressed chunk location.
///
/// Lock files, staging files, and directories cannot be constructed as objects.
/// Native paths still require the transport's own portable syntax validation.
#[derive(Clone, Debug)]
pub struct ObjectPath(PathBuf);

impl ObjectPath {
    /// Admit a path using the library's repository layout definitions.
    ///
    /// # Errors
    /// Rejects paths outside the published object namespace.
    pub fn new(path: PathBuf) -> Result<Self, Error> {
        let components = parts(&path)?;
        let admitted = match components.as_slice() {
            [configuration] => *configuration == config::CONFIG_YML_FILE,
            [generation, configuration] => {
                generation_directory(generation)
                    && *configuration == generation::CONFIG_YML_FILE
            }
            [generation, namespace, names @ ..]
                if *namespace == name::NAME_SUBDIR =>
            {
                generation_directory(generation)
                    && names.last().is_some_and(|name| name.ends_with(".yml"))
            }
            _ => ChunkPath::new(path.clone()).is_ok(),
        };
        if !admitted {
            return Err(Error::Layout);
        }
        Ok(Self(path))
    }

    /// Native relative path of the admitted published object.
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// A repository root, generation, or directory beneath its names/chunks.
///
/// This admits directory locations without relying on their current existence.
#[derive(Clone, Debug)]
pub struct DirectoryPath(PathBuf);

impl DirectoryPath {
    /// Admit a directory path; an empty path denotes the repository root.
    ///
    /// # Errors
    /// Rejects objects and directories outside the repository layout.
    pub fn new(path: PathBuf) -> Result<Self, Error> {
        let components = parts(&path)?;
        let admitted = match components.as_slice() {
            [] => true,
            [generation] => generation_directory(generation),
            [generation, namespace, ..] => {
                generation_directory(generation)
                    && match *namespace {
                        name::NAME_SUBDIR => true,
                        config::DATA_SUBDIR => {
                            crate::chunk_path::validate_directory(&path).is_ok()
                        }
                        _ => false,
                    }
            }
        };
        if !admitted {
            return Err(Error::Layout);
        }
        Ok(Self(path))
    }

    /// Native relative path, empty for the repository root.
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DIGEST_SIZE;

    #[test]
    fn object_admission_excludes_locks_staging_and_arbitrary_host_paths() {
        let generation = "0000000000000001-0000000000000000";
        for path in [
            config::CONFIG_YML_FILE.to_owned(),
            format!("{generation}/config.yml"),
            format!("{generation}/name/archive.yml"),
            format!("{generation}/name/nested/archive.yml"),
            format!("{generation}/chunk/ab/{}", "ab".repeat(DIGEST_SIZE)),
        ] {
            assert!(ObjectPath::new(path.into()).is_ok());
        }
        for path in [
            config::LOCK_FILE.to_owned(),
            "../config.yml".to_owned(),
            "other-file".to_owned(),
            format!("{generation}/name/archive.random.tmp"),
            format!("{generation}/chunk/{}", "cd".repeat(DIGEST_SIZE - 1)),
            format!("{generation}/chunk"),
        ] {
            assert!(ObjectPath::new(path.into()).is_err());
        }
    }

    #[test]
    fn directory_admission_preserves_generation_and_chunk_prefix_rules() {
        let generation = "000000000000000a-0000000000000000";
        for path in [
            "".to_owned(),
            generation.to_owned(),
            format!("{generation}/name"),
            format!("{generation}/chunk/ab/cd"),
        ] {
            assert!(DirectoryPath::new(path.into()).is_ok());
        }
        for path in [
            "config.yml".to_owned(),
            generation.to_uppercase(),
            format!("{generation}/chunk/AB"),
            format!("{generation}/chunk/abc"),
            format!("{generation}/other"),
        ] {
            assert!(DirectoryPath::new(path.into()).is_err());
        }
    }
}
