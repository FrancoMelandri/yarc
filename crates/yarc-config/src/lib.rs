//! Parses `yarc.yaml` into a validated project model.

mod dotenv;
mod interpolate;
mod model;
mod validate;

pub use dotenv::{parse_dotenv, DotenvError};
pub use interpolate::{interpolate, InterpolationError};
pub use model::*;

use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum ConfigError {
    #[error("invalid yaml: {0}")]
    Yaml(String),
    #[error("service '{0}': image must not be empty")]
    EmptyImage(String),
    #[error("service '{0}': user must not be empty")]
    EmptyUser(String),
    #[error("service '{service}': invalid port '{value}': {reason}")]
    InvalidPort { service: String, value: String, reason: String },
    #[error("service '{service}': invalid volume '{value}': {reason}")]
    InvalidVolume { service: String, value: String, reason: String },
    #[error("service '{service}': invalid duration '{value}'")]
    InvalidDuration { service: String, value: String },
    #[error("service '{service}' needs unknown service '{target}'")]
    UnknownNeed { service: String, target: String },
    #[error("service '{0}' cannot need itself")]
    SelfNeed(String),
    #[error("service '{service}' needs '{target}' to be healthy, but '{target}' has no health check")]
    NeedsHealthWithoutCheck { service: String, target: String },
    #[error("dependency cycle involving service '{0}'")]
    Cycle(String),
    #[error("project defines no services")]
    NoServices,
}

/// Parse and validate a `yarc.yaml` document.
pub fn parse(input: &str) -> Result<Project, ConfigError> {
    let raw: model::RawProject =
        serde_yaml_ng::from_str(input).map_err(|e| ConfigError::Yaml(e.to_string()))?;
    let project = raw.into_project()?;
    validate::validate(&project)?;
    Ok(project)
}

#[cfg(test)]
mod tests;
