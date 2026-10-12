//! Named static bearer credentials; secret values never enter diagnostics.
use crate::lease::{CredentialName, Principal};
use rdedup_protocol::credential::BearerToken;
use serde::Deserialize;
use std::collections::HashSet;
use std::str::FromStr;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("at least one named bearer credential is required")]
    Empty,
    #[error("credential names and tokens must be unique")]
    Duplicate,
    #[error("credentials must be a JSON array of named token entries")]
    EnvironmentFormat,
    #[error("missing or invalid bearer credentials")]
    Unauthorized,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Credential {
    name: CredentialName,
    token: BearerToken,
}

/// A validated, nonempty set of unambiguous credential identities.
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "Vec<Credential>")]
pub(crate) struct Credentials(Vec<Credential>);

impl TryFrom<Vec<Credential>> for Credentials {
    type Error = Error;
    fn try_from(credentials: Vec<Credential>) -> Result<Self, Self::Error> {
        if credentials.is_empty() {
            return Err(Error::Empty);
        }
        let mut names = HashSet::new();
        let mut tokens = HashSet::new();
        for credential in &credentials {
            if !names.insert(&credential.name)
                || !tokens.insert(&credential.token)
            {
                return Err(Error::Duplicate);
            }
        }
        Ok(Self(credentials))
    }
}

impl FromStr for Credentials {
    type Err = Error;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(text).map_err(|_| Error::EnvironmentFormat)
    }
}

impl Credentials {
    pub(crate) fn authenticate(
        &self,
        header: Option<&str>,
    ) -> Result<Principal, Error> {
        let Some(header) = header else {
            return Ok(Principal::Anonymous);
        };
        let (scheme, value) =
            header.split_once(' ').ok_or(Error::Unauthorized)?;
        if !scheme.eq_ignore_ascii_case("Bearer") {
            return Err(Error::Unauthorized);
        }
        let supplied = value
            .parse::<BearerToken>()
            .map_err(|_| Error::Unauthorized)?;
        let mut authenticated = None;
        for credential in &self.0 {
            if credential.token == supplied {
                authenticated =
                    Some(Principal::Credential(credential.name.clone()));
            }
        }
        authenticated.ok_or(Error::Unauthorized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_reject_ambiguous_identity_and_redact_debug_output() {
        let token = "ab".repeat(32);
        let entries = format!(
            r#"[{{"name":"one","token":"{token}"}},{{"name":"two","token":"{token}"}}]"#
        );
        assert!(entries.parse::<Credentials>().is_err());
        let entries = format!(r#"[{{"name":"one","token":"{token}"}}]"#);
        let credentials: Credentials = entries.parse().unwrap();
        assert!(!format!("{credentials:?}").contains(&token));
        assert_eq!(
            credentials
                .authenticate(Some(&format!("Bearer {token}")))
                .unwrap(),
            Principal::Credential("one".parse().unwrap())
        );
        assert!(credentials.authenticate(Some("Bearer incorrect")).is_err());
        assert_eq!(
            credentials.authenticate(None).unwrap(),
            Principal::Anonymous
        );
    }
}
