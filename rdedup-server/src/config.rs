use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read server config {path}: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not parse server config {path}: {source}")]
    ParseFile {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error(
        "invalid value for {setting} from {environment_variable}: {value}"
    )]
    InvalidEnvironmentValue {
        setting: &'static str,
        environment_variable: &'static str,
        value: String,
    },
    #[error("required setting {setting} is missing; set it in the config file or {environment_variable}")]
    Missing {
        setting: &'static str,
        environment_variable: &'static str,
    },
    #[error("invalid token list: {0}")]
    InvalidTokens(String),
    #[error("server settings are invalid: {0}")]
    Invalid(String),
}

/// Resolves one setting from a config file, an environment variable, or a default.
struct Setting<T> {
    name: &'static str,
    environment_variable: &'static str,
    default: Option<T>,
}

impl<T> Setting<T>
where
    T: FromStr,
{
    fn resolve(self, file_value: Option<T>) -> Result<T, ConfigError> {
        if let Some(value) = file_value {
            return Ok(value);
        }

        if let Ok(value) = env::var(self.environment_variable) {
            return value.parse().map_err(|_| {
                ConfigError::InvalidEnvironmentValue {
                    setting: self.name,
                    environment_variable: self.environment_variable,
                    value,
                }
            });
        }

        self.default.ok_or(ConfigError::Missing {
            setting: self.name,
            environment_variable: self.environment_variable,
        })
    }
}

#[derive(Clone, Debug, Default)]
struct ClientTokens(HashMap<String, String>);

impl FromStr for ClientTokens {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut tokens = HashMap::new();
        for entry in value.split(',').filter(|entry| !entry.is_empty()) {
            let (name, token) = entry.split_once('=').ok_or_else(|| {
                ConfigError::InvalidTokens(
                    "expected comma-separated name=token entries".to_owned(),
                )
            })?;
            if name.trim().is_empty() || token.len() < 32 {
                return Err(ConfigError::InvalidTokens(
                    "token names must be nonempty and tokens must contain at least 32 bytes".to_owned(),
                ));
            }
            if tokens.insert(name.to_owned(), token.to_owned()).is_some() {
                return Err(ConfigError::InvalidTokens(format!(
                    "token name {name} is configured more than once"
                )));
            }
        }
        Ok(ClientTokens(tokens))
    }
}

#[derive(Default, Deserialize)]
struct FileConfig {
    repository_path: Option<PathBuf>,
    bind_address: Option<String>,
    lease_ttl_seconds: Option<u64>,
    lease_renewal_grace_seconds: Option<u64>,
    lease_request_ttl_seconds: Option<u64>,
    auth: Option<FileAuth>,
}

#[derive(Default, Deserialize)]
struct FileAuth {
    require_read_auth: Option<bool>,
    tokens: Option<HashMap<String, String>>,
}

#[derive(Clone)]
pub struct ServerConfig {
    pub repository_path: PathBuf,
    pub bind_address: SocketAddr,
    pub lease_ttl: Duration,
    pub lease_renewal_grace: Duration,
    pub lease_request_ttl: Duration,
    pub require_read_auth: bool,
    pub tokens: HashMap<String, String>,
}

impl ServerConfig {
    pub fn load(config_path: Option<PathBuf>) -> Result<Self, ConfigError> {
        let file = match config_path {
            Some(path) => {
                let contents =
                    std::fs::read_to_string(&path).map_err(|source| {
                        ConfigError::ReadFile {
                            path: path.clone(),
                            source,
                        }
                    })?;
                Some(toml::from_str::<FileConfig>(&contents).map_err(
                    |source| ConfigError::ParseFile {
                        path,
                        source: Box::new(source),
                    },
                )?)
            }
            None => None,
        };
        let file = file.unwrap_or_default();
        let auth = file.auth.unwrap_or_default();

        let repository_path = (Setting {
            name: "repository path",
            environment_variable: "RDEDUP_SERVER_REPOSITORY_PATH",
            default: None,
        })
        .resolve(file.repository_path)?;
        let file_bind_address = file
            .bind_address
            .map(|value| value.parse())
            .transpose()
            .map_err(|_| {
                ConfigError::Invalid(
                    "invalid bind address in config file".to_owned(),
                )
            })?;
        let bind_address = (Setting {
            name: "bind address",
            environment_variable: "RDEDUP_SERVER_BIND_ADDRESS",
            default: Some(
                "127.0.0.1:8080".parse().expect("static address is valid"),
            ),
        })
        .resolve(file_bind_address)?;
        let lease_ttl_seconds = (Setting {
            name: "lease TTL",
            environment_variable: "RDEDUP_SERVER_LEASE_TTL_SECONDS",
            default: Some(300_u64),
        })
        .resolve(file.lease_ttl_seconds)?;
        let lease_renewal_grace_seconds = (Setting {
            name: "lease renewal grace period",
            environment_variable: "RDEDUP_SERVER_LEASE_RENEWAL_GRACE_SECONDS",
            default: Some(21_600_u64),
        })
        .resolve(file.lease_renewal_grace_seconds)?;
        let lease_request_ttl_seconds = (Setting {
            name: "lease request TTL",
            environment_variable: "RDEDUP_SERVER_LEASE_REQUEST_TTL_SECONDS",
            default: Some(86_400_u64),
        })
        .resolve(file.lease_request_ttl_seconds)?;
        let require_read_auth = (Setting {
            name: "read authentication requirement",
            environment_variable: "RDEDUP_SERVER_REQUIRE_READ_AUTH",
            default: Some(false),
        })
        .resolve(auth.require_read_auth)?;
        let tokens = match auth.tokens {
            Some(tokens) => tokens,
            None => match env::var("RDEDUP_SERVER_TOKENS") {
                Ok(value) => value.parse::<ClientTokens>()?.0,
                Err(_) => HashMap::new(),
            },
        };
        for (name, token) in &tokens {
            if name.trim().is_empty() || token.len() < 32 {
                return Err(ConfigError::InvalidTokens(
                    "token names must be nonempty and tokens must contain at least 32 bytes".to_owned(),
                ));
            }
        }

        if lease_ttl_seconds == 0
            || lease_renewal_grace_seconds < lease_ttl_seconds
            || lease_request_ttl_seconds == 0
        {
            return Err(ConfigError::Invalid(
                "lease TTL and request TTL must be nonzero; renewal grace must be at least the lease TTL".to_owned(),
            ));
        }

        Ok(ServerConfig {
            repository_path,
            bind_address,
            lease_ttl: Duration::from_secs(lease_ttl_seconds),
            lease_renewal_grace: Duration::from_secs(
                lease_renewal_grace_seconds,
            ),
            lease_request_ttl: Duration::from_secs(lease_request_ttl_seconds),
            require_read_auth,
            tokens,
        })
    }
}
