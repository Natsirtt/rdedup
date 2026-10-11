//! File-first setting resolution; transport and storage see only resolved values.
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::env;
use std::fmt::Display;
use std::str::FromStr;
use std::time::Duration;

/// Failure to resolve one startup setting without exposing its value.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No source supplies a required setting.
    #[error("required setting {name} is absent; configure it in the file or {environment}")]
    Missing {
        /// Configuration key.
        name: &'static str,
        /// Fallback environment variable name.
        environment: &'static str,
    },
    /// Explicit file values must parse rather than silently fall back.
    #[error("setting {name} has an invalid file value")]
    File {
        /// Configuration key.
        name: &'static str,
    },
    /// An environment value is invalid or not Unicode.
    #[error(
        "setting {name} has an invalid environment value in {environment}"
    )]
    Environment {
        /// Configuration key.
        name: &'static str,
        /// Fallback environment variable name.
        environment: &'static str,
    },
    /// A file contains an unrecognized key.
    #[error("unknown configuration setting {0}")]
    Unknown(String),
}

/// A source can be injected without mutating the process environment in tests.
pub(crate) trait Environment {
    fn read(&self, key: &str) -> Result<Option<String>, env::VarError>;
}

pub(crate) struct ProcessEnvironment;
impl Environment for ProcessEnvironment {
    fn read(&self, key: &str) -> Result<Option<String>, env::VarError> {
        match env::var(key) {
            Ok(value) => Ok(Some(value)),
            Err(env::VarError::NotPresent) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// Raw file values are consumed exactly once, making unknown keys an error.
pub(crate) struct FileSettings(BTreeMap<String, toml::Value>);

impl FileSettings {
    pub(crate) fn new(values: BTreeMap<String, toml::Value>) -> Self {
        Self(values)
    }
    pub(crate) fn finish(self) -> Result<(), Error> {
        match self.0.into_keys().next() {
            Some(key) => Err(Error::Unknown(key)),
            None => Ok(()),
        }
    }
}

pub(crate) struct Setting<T> {
    name: &'static str,
    environment: &'static str,
    default: Option<T>,
}

impl<T> Setting<T> {
    pub(crate) fn new(
        name: &'static str,
        environment: &'static str,
        default: Option<T>,
    ) -> Self {
        Self {
            name,
            environment,
            default,
        }
    }
}

impl<T: DeserializeOwned + FromStr> Setting<T>
where
    T::Err: Display,
{
    pub(crate) fn resolve(
        self,
        file: &mut FileSettings,
        environment: &impl Environment,
    ) -> Result<T, Error> {
        if let Some(value) = file.0.remove(self.name) {
            // Deserializer diagnostics can include bearer secrets. The setting
            // name identifies the failing boundary without exposing its value.
            return value
                .try_into()
                .map_err(|_| Error::File { name: self.name });
        }
        let from_environment =
            environment.read(self.environment).map_err(|_| {
                Error::Environment {
                    name: self.name,
                    environment: self.environment,
                }
            })?;
        if let Some(value) = from_environment {
            return value.parse().map_err(|_| Error::Environment {
                name: self.name,
                environment: self.environment,
            });
        }
        self.default.ok_or(Error::Missing {
            name: self.name,
            environment: self.environment,
        })
    }
}

impl Setting<Duration> {
    /// File/environment duration scalars are seconds; resolved values carry units.
    pub(crate) fn resolve_duration(
        self,
        file: &mut FileSettings,
        environment: &impl Environment,
    ) -> Result<Duration, Error> {
        Setting::new(
            self.name,
            self.environment,
            self.default.map(|duration| duration.as_secs()),
        )
        .resolve(file, environment)
        .map(Duration::from_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct TestEnvironment(BTreeMap<String, String>);
    impl Environment for TestEnvironment {
        fn read(&self, key: &str) -> Result<Option<String>, env::VarError> {
            Ok(self.0.get(key).cloned())
        }
    }

    #[test]
    fn a_file_setting_wins_even_when_the_environment_is_invalid() {
        let mut file = FileSettings::new(BTreeMap::from([(
            "port".to_owned(),
            toml::Value::Integer(8123),
        )]));
        let environment = TestEnvironment(BTreeMap::from([(
            "PORT".to_owned(),
            "invalid".to_owned(),
        )]));
        assert_eq!(
            Setting::new("port", "PORT", Some(1234_u16))
                .resolve(&mut file, &environment)
                .unwrap(),
            8123
        );
        file.finish().unwrap();
    }

    #[test]
    fn invalid_explicit_values_do_not_fall_back_to_defaults() {
        let mut file = FileSettings::new(BTreeMap::new());
        let environment = TestEnvironment(BTreeMap::from([(
            "PORT".to_owned(),
            "invalid".to_owned(),
        )]));
        assert!(matches!(
            Setting::new("port", "PORT", Some(1234_u16))
                .resolve(&mut file, &environment),
            Err(Error::Environment { .. })
        ));
        let mut file = FileSettings::new(BTreeMap::from([(
            "port".to_owned(),
            toml::Value::String("invalid".to_owned()),
        )]));
        assert!(matches!(
            Setting::new("port", "PORT", Some(1234_u16))
                .resolve(&mut file, &environment),
            Err(Error::File { .. })
        ));
    }
}
