//! Credential names identify authenticated principals without retaining secrets.
use serde::Deserialize;
use std::str::FromStr;

/// A credential name cannot be empty or contain control characters.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The name does not meet the configuration identity contract.
    #[error("credential name must be nonempty, at most 128 bytes, and contain no whitespace or control characters")]
    InvalidName,
}

/// Human-readable identity configured for one bearer credential.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct CredentialName(String);

impl CredentialName {
    /// The credential's configured display name, never its secret token.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for CredentialName {
    type Err = Error;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        if name.is_empty()
            || name.len() > 128
            || name.chars().any(|character| {
                character.is_whitespace() || character.is_control()
            })
        {
            return Err(Error::InvalidName);
        }
        Ok(Self(name.to_owned()))
    }
}

impl TryFrom<String> for CredentialName {
    type Error = Error;
    fn try_from(name: String) -> Result<Self, Self::Error> {
        name.parse()
    }
}
