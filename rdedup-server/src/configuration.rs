//! Resolve and validate server settings before accepting connections.
use crate::authentication::Credentials;
use crate::lease::{LeasePolicy, RequestCapacity, RequestPolicy};
use crate::setting::{Environment, FileSettings, ProcessEnvironment, Setting};
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Invalid server startup configuration.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The selected file could not be opened.
    #[error("cannot read configuration file {path}")]
    Read {
        /// Selected configuration file.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// Only the location is retained because parser diagnostics may contain tokens.
    #[error("invalid TOML in configuration file {path}")]
    Parse {
        /// Selected configuration file.
        path: PathBuf,
    },
    /// A setting is missing, malformed, or unknown.
    #[error(transparent)]
    Setting(#[from] crate::setting::Error),
    /// Lease timing is invalid.
    #[error(transparent)]
    Lease(#[from] crate::lease::PolicyError),
    /// Acquisition bookkeeping limits are invalid.
    #[error(transparent)]
    Requests(#[from] crate::lease::RequestPolicyError),
}

/// Whether object reads require an authenticated credential.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ReadPolicy {
    Public,
    Authenticated,
}

/// Fully resolved settings; business code never reads environment variables.
#[derive(Debug)]
pub struct Configuration {
    pub(crate) repository: PathBuf,
    pub(crate) listen: SocketAddr,
    pub(crate) credentials: Credentials,
    pub(crate) reads: ReadPolicy,
    pub(crate) leases: LeasePolicy,
    pub(crate) requests: RequestPolicy,
}

impl Configuration {
    /// Read an explicitly selected TOML file, then resolve absent values from
    /// environment variables and defaults. An absent file argument uses only
    /// environment/defaults; a selected but missing file is an error.
    ///
    /// # Errors
    /// Returns file, setting, credential, or startup policy validation failures.
    pub fn load(path: Option<&Path>) -> Result<Self, Error> {
        let values = match path {
            Some(path) => {
                let text = std::fs::read_to_string(path).map_err(|source| {
                    Error::Read {
                        path: path.to_owned(),
                        source,
                    }
                })?;
                toml::from_str(&text).map_err(|_| Error::Parse {
                    path: path.to_owned(),
                })?
            }
            None => BTreeMap::new(),
        };
        Self::resolve(
            FileSettings::new(values),
            &ProcessEnvironment,
            Instant::now(),
        )
    }

    fn resolve(
        mut file: FileSettings,
        environment: &impl Environment,
        now: Instant,
    ) -> Result<Self, Error> {
        let repository =
            Setting::new("repository", "RDEDUP_SERVER_REPOSITORY", None)
                .resolve(&mut file, environment)?;
        let listen = Setting::new(
            "listen",
            "RDEDUP_SERVER_LISTEN",
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080)),
        )
        .resolve(&mut file, environment)?;
        let credentials =
            Setting::new("credentials", "RDEDUP_SERVER_CREDENTIALS", None)
                .resolve(&mut file, environment)?;
        let require_reads = Setting::new(
            "require_read_credentials",
            "RDEDUP_SERVER_REQUIRE_READ_CREDENTIALS",
            Some(false),
        )
        .resolve(&mut file, environment)?;
        let lifetime = Setting::new(
            "lease_lifetime_secs",
            "RDEDUP_SERVER_LEASE_LIFETIME_SECS",
            Some(Duration::from_secs(300)),
        )
        .resolve_duration(&mut file, environment)?;
        let grace = Setting::new(
            "exclusive_grace_secs",
            "RDEDUP_SERVER_EXCLUSIVE_GRACE_SECS",
            Some(Duration::from_secs(900)),
        )
        .resolve_duration(&mut file, environment)?;
        let capacity = Setting::new(
            "request_capacity",
            "RDEDUP_SERVER_REQUEST_CAPACITY",
            NonZeroUsize::new(4096),
        )
        .resolve(&mut file, environment)?;
        let idle = Setting::new(
            "request_idle_secs",
            "RDEDUP_SERVER_REQUEST_IDLE_SECS",
            Some(Duration::from_secs(60)),
        )
        .resolve_duration(&mut file, environment)?;
        let retention = Setting::new(
            "request_retention_secs",
            "RDEDUP_SERVER_REQUEST_RETENTION_SECS",
            Some(Duration::from_secs(600)),
        )
        .resolve_duration(&mut file, environment)?;
        file.finish()?;
        Ok(Self {
            repository,
            listen,
            credentials,
            reads: if require_reads {
                ReadPolicy::Authenticated
            } else {
                ReadPolicy::Public
            },
            leases: LeasePolicy::new(lifetime, grace, now)?,
            requests: RequestPolicy::new(
                RequestCapacity::new(capacity),
                idle,
                retention,
                now,
            )?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEnvironment(BTreeMap<String, String>);

    impl Environment for TestEnvironment {
        fn read(
            &self,
            key: &str,
        ) -> Result<Option<String>, std::env::VarError> {
            Ok(self.0.get(key).cloned())
        }
    }

    fn file(extra: &str) -> FileSettings {
        let text = format!(
            "repository = 'from-file'\n{extra}\n\
             [[credentials]]\nname = 'agent'\ntoken = '{}'\n",
            "ab".repeat(32)
        );
        FileSettings::new(toml::from_str(&text).unwrap())
    }

    #[test]
    fn startup_resolves_each_setting_independently_and_redacts_credentials() {
        let environment = TestEnvironment(BTreeMap::from([
            (
                "RDEDUP_SERVER_REPOSITORY".to_owned(),
                "from-environment".to_owned(),
            ),
            ("RDEDUP_SERVER_CREDENTIALS".to_owned(), "invalid".to_owned()),
            (
                "RDEDUP_SERVER_LISTEN".to_owned(),
                "127.0.0.1:8123".to_owned(),
            ),
        ]));
        let configuration =
            Configuration::resolve(file(""), &environment, Instant::now())
                .unwrap();
        assert_eq!(configuration.repository, PathBuf::from("from-file"));
        assert_eq!(configuration.listen, "127.0.0.1:8123".parse().unwrap());
        assert!(matches!(configuration.reads, ReadPolicy::Public));
        assert!(!format!("{configuration:?}").contains(&"ab".repeat(32)));
    }

    #[test]
    fn unknown_settings_and_invalid_lease_policy_fail_startup() {
        let environment = TestEnvironment(BTreeMap::new());
        assert!(matches!(
            Configuration::resolve(
                file("lease_lifetme_secs = 30"),
                &environment,
                Instant::now()
            ),
            Err(Error::Setting(crate::setting::Error::Unknown(_)))
        ));
        assert!(matches!(
            Configuration::resolve(
                file("lease_lifetime_secs = 0"),
                &environment,
                Instant::now()
            ),
            Err(Error::Lease(_))
        ));
        assert!(matches!(
            Configuration::resolve(
                file("request_capacity = 0"),
                &environment,
                Instant::now()
            ),
            Err(Error::Setting(_))
        ));
    }

    #[test]
    fn a_selected_missing_file_is_an_error() {
        let directory = tempfile::TempDir::new().unwrap();
        assert!(matches!(
            Configuration::load(Some(&directory.path().join("missing.toml"))),
            Err(Error::Read { .. })
        ));
    }
}
