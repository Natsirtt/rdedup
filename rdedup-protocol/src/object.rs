//! Portable repository-relative paths at the HTTP boundary.
use serde::{Deserialize, Serialize, Serializer};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

/// Rejection of paths that could escape or alias the repository namespace.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Root directories are represented separately from object paths.
    #[error("repository path is empty")]
    Empty,
    /// Protocol paths must remain bounded independently of filesystem limits.
    #[error("repository path exceeds the protocol length limit")]
    TooLong,
    /// A path contains an unsafe or nonportable segment.
    #[error("repository path contains an unsafe segment")]
    UnsafeSegment,
    /// HTTP repository paths use UTF-8.
    #[error("repository path is not UTF-8")]
    NonUtf8,
}

/// A nonempty, portable relative path without traversal or Windows aliases.
///
/// This validates path syntax. The server separately admits only repository
/// objects, excluding lock files and incomplete temporary entries.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct RepositoryPath(String);

impl RepositoryPath {
    /// Convert a native relative path into canonical HTTP path segments.
    ///
    /// # Errors
    /// Rejects absolute paths, traversal, non-UTF-8 paths, and unsafe segments.
    pub fn from_path(path: &Path) -> Result<Self, Error> {
        let segments = path
            .components()
            .map(|component| match component {
                Component::Normal(segment) => {
                    segment.to_str().ok_or(Error::NonUtf8)
                }
                _ => Err(Error::UnsafeSegment),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::try_from(segments.join("/"))
    }

    /// Canonical relative representation with slash-separated segments.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Relative filesystem representation on the current platform.
    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(&self.0)
    }
}

impl TryFrom<String> for RepositoryPath {
    type Error = Error;
    fn try_from(path: String) -> Result<Self, Self::Error> {
        if path.is_empty() {
            return Err(Error::Empty);
        }
        if path.len() > 4096 {
            return Err(Error::TooLong);
        }
        for segment in path.split('/') {
            if segment.is_empty()
                || segment.len() > 255
                || segment == "."
                || segment == ".."
                || segment.ends_with(['.', ' '])
                || segment.chars().any(|character| {
                    character.is_control()
                        || matches!(
                            character,
                            '\u{005c}'
                                | ':'
                                | '*'
                                | '?'
                                | '"'
                                | '<'
                                | '>'
                                | '|'
                        )
                })
            {
                return Err(Error::UnsafeSegment);
            }
            let stem = segment
                .split('.')
                .next()
                .ok_or(Error::UnsafeSegment)?
                .to_ascii_uppercase();
            // Windows reserves these aliases even when followed by an extension.
            // https://learn.microsoft.com/windows/win32/fileio/naming-a-file
            let numbered_device = stem
                .strip_prefix("COM")
                .or_else(|| stem.strip_prefix("LPT"))
                .is_some_and(|port| {
                    matches!(
                        port,
                        "1" | "2"
                            | "3"
                            | "4"
                            | "5"
                            | "6"
                            | "7"
                            | "8"
                            | "9"
                            | "\u{b9}"
                            | "\u{b2}"
                            | "\u{b3}"
                    )
                });
            if matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            ) || numbered_device
            {
                return Err(Error::UnsafeSegment);
            }
        }
        Ok(Self(path))
    }
}
impl FromStr for RepositoryPath {
    type Err = Error;
    fn from_str(path: &str) -> Result<Self, Self::Err> {
        Self::try_from(path.to_owned())
    }
}
impl fmt::Display for RepositoryPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}
impl Serialize for RepositoryPath {
    fn serialize<S: Serializer>(
        &self,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

/// Depth of one directory listing.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ListScope {
    /// Immediate entries only.
    #[default]
    Immediate,
    /// Stored files beneath the directory tree.
    Recursive,
}

/// Directory listing parameters; an absent path denotes the repository root.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryQuery {
    /// Relative directory, or the repository root when absent.
    pub path: Option<RepositoryPath>,
    /// Listing depth, defaulting to immediate entries.
    #[serde(default)]
    pub scope: ListScope,
}

/// Exclusive relocation of an arbitrary admitted repository object.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rename {
    /// Existing repository entry.
    pub source: RepositoryPath,
    /// Destination entry under the same repository protection.
    pub destination: RepositoryPath,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn traversal_and_platform_aliases_are_rejected_at_deserialization() {
        for invalid in [
            "",
            "/absolute",
            "../name",
            "a/../b",
            "a//b",
            "a/./b",
            "a:b",
            "a/b.",
            "a/b ",
            "CON",
            "a/LPT1.txt",
            "a/COM\u{b9}.txt",
            "a/LPT\u{b2}.txt",
            "a\u{005c}b",
        ] {
            let encoded = serde_json::to_string(invalid).unwrap();
            assert!(
                serde_json::from_str::<RepositoryPath>(&encoded).is_err(),
                "accepted {invalid}"
            );
        }
    }
    #[test]
    fn valid_publication_names_remain_unambiguous() {
        let path: RepositoryPath =
            "generation/name/nush-57-symbols.yml".parse().unwrap();
        assert_eq!(
            serde_json::to_string(&path).unwrap(),
            "\"generation/name/nush-57-symbols.yml\""
        );
        assert_eq!(
            RepositoryPath::from_path(&path.to_path_buf()).unwrap(),
            path
        );
    }
}
