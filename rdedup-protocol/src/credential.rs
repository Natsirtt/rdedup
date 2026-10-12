//! Bearer secrets shared by server configuration and authenticated clients.
use http::header::HeaderValue;
use serde::Deserialize;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use subtle::ConstantTimeEq;

/// Rejection of a bearer secret with an invalid encoding or length.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A credential encodes exactly 32 random bytes as hexadecimal text.
    #[error(
        "bearer token must encode 32 random bytes as 64 hexadecimal characters"
    )]
    TokenFormat,
}

/// A 256-bit bearer secret with redacted diagnostics and constant-time equality.
///
/// Encoding validation cannot establish that the secret was randomly generated.
#[derive(Clone, Deserialize)]
#[serde(try_from = "String")]
pub struct BearerToken([u8; 32]);

impl BearerToken {
    /// A sensitive Authorization header suitable for an authenticated request.
    pub fn authorization_header(&self) -> HeaderValue {
        let mut header =
            HeaderValue::from_str(&format!("Bearer {}", hex::encode(self.0)))
                .expect(
                "Bearer plus hexadecimal digits always forms an ASCII header",
            );
        header.set_sensitive(true);
        header
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerToken([redacted])")
    }
}
impl PartialEq for BearerToken {
    fn eq(&self, other: &Self) -> bool {
        bool::from(self.0.ct_eq(&other.0))
    }
}
impl Eq for BearerToken {}
impl Hash for BearerToken {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}
impl FromStr for BearerToken {
    type Err = Error;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut bytes = [0; 32];
        hex::decode_to_slice(text, &mut bytes)
            .map_err(|_| Error::TokenFormat)?;
        Ok(Self(bytes))
    }
}
impl TryFrom<String> for BearerToken {
    type Error = Error;
    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validated_secret_has_one_identity_and_redacted_diagnostics() {
        let token: BearerToken = "aB".repeat(32).parse().unwrap();
        assert_eq!(token, "ab".repeat(32).parse().unwrap());
        assert_ne!(token, "cd".repeat(32).parse().unwrap());
        assert_eq!(format!("{token:?}"), "BearerToken([redacted])");
        let header = token.authorization_header();
        assert!(header.is_sensitive());
        assert_eq!(
            header.to_str().unwrap(),
            format!("Bearer {}", "ab".repeat(32))
        );
        assert!(!format!("{header:?}").contains(&"ab".repeat(32)));
    }

    #[test]
    fn malformed_secrets_cannot_be_constructed_or_deserialized() {
        for text in ["ab".repeat(31), "ab".repeat(33), "zz".repeat(32)] {
            assert!(text.parse::<BearerToken>().is_err());
            let encoded = serde_json::to_string(&text).unwrap();
            assert!(serde_json::from_str::<BearerToken>(&encoded).is_err());
        }
    }
}
