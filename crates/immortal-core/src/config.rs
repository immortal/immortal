//! Strict service configuration parsing, normalization, validation, and resolution.
//!
//! Version 2 wire input is converted into one typed service model before process
//! setup. Canonical logging separates local `log` routes from one combined
//! external `logger`; deprecated v2 spellings are translated only when their
//! stream behavior is representable without widening or dropping output.
//! Validation owns policy bounds and field relationships, while path resolution
//! owns definition-relative paths. Callers never need to infer precedence or
//! repair partially valid policy across a process boundary.
//!
//! This module is a thin facade over focused children: `model` owns the
//! canonical runtime types, `document` owns the strict `version: 2` wire
//! schema, `logging` owns logging-alias normalization and its compact
//! quantity codec (split further into its own `wire` and `quantity`
//! children), `validate` owns policy bounds, `paths` owns
//! definition-relative path resolution, `environment` owns the
//! direct-command environment-directory loader, and `error` owns the
//! shared failure type. Every child is private; every public item below is
//! re-exported so `immortal_core::config` remains the single reachable path.

use std::{fs::File, io::Read, path::Path};

use serde::{Serialize, de::IgnoredAny};

mod document;
mod environment;
mod error;
mod logging;
mod model;
mod paths;
mod validate;

pub use self::environment::{
    MAX_ENVIRONMENT_DIRECTORY_BYTES, MAX_ENVIRONMENT_DIRECTORY_ENTRIES,
    MAX_ENVIRONMENT_VALUE_BYTES, load_environment_directory,
};
pub use self::error::ConfigError;
pub use self::model::{
    BackoffConfig, CommandHook, ConditionBackoffConfig, DescriptorTrackingConfig, EnvironmentMode,
    FileLogConfig, FileLogRoutes, LoggerRestartConfig, LoggingConfig, PidFiles, ProcessMode,
    ReadinessConfig, ReadinessMode, RestartBurstLimit, RestartConfig, RestartLimits, RestartPolicy,
    ServiceConfig, StartConditionConfig,
};
pub use self::paths::resolve_paths;
pub use self::validate::MAX_SCHEDULE_SECONDS;

/// Maximum accepted size of one service definition.
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;

const MEBIBYTE: u64 = 1024 * 1024;
const DEFAULT_LOG_KEEP: u32 = 7;

/// Read, parse, normalize, and validate a service definition.
///
/// # Errors
///
/// Returns an error when the file cannot be read, exceeds the size limit, is
/// malformed, selects an unsupported schema, or violates service invariants.
pub fn parse_file(path: &Path) -> Result<ServiceConfig, ConfigError> {
    let file = File::open(path)?;
    let declared_size = file.metadata()?.len();
    if declared_size > MAX_CONFIG_BYTES as u64 {
        return Err(ConfigError::TooLarge {
            actual: declared_size,
        });
    }

    let mut bytes = Vec::with_capacity(usize::try_from(declared_size).unwrap_or(0));
    file.take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    parse_bytes_at(&bytes, path)
}

/// Parse one in-memory definition using the source path for stable resolution.
///
/// # Errors
///
/// Returns the same parse, validation, resolution, and filesystem errors as
/// [`parse_file`], without reopening a path already read by a safe scanner.
pub(crate) fn parse_bytes_at(bytes: &[u8], path: &Path) -> Result<ServiceConfig, ConfigError> {
    let mut config = parse_bytes(bytes)?;
    let absolute_source = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let base = absolute_source.parent().ok_or_else(|| {
        ConfigError::Validation(vec![
            "configuration path has no parent directory".to_owned(),
        ])
    })?;
    resolve_paths(&mut config, base)?;
    paths::validate_resolved(&config)?;
    Ok(config)
}

/// Parse, normalize, and validate an in-memory service definition.
///
/// # Errors
///
/// Returns an error for oversized, non-UTF-8, malformed, unsupported, or
/// semantically invalid input.
pub fn parse_bytes(bytes: &[u8]) -> Result<ServiceConfig, ConfigError> {
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge {
            actual: bytes.len() as u64,
        });
    }
    let source = std::str::from_utf8(bytes)
        .map_err(|error| ConfigError::Parse(format!("input is not UTF-8: {error}")))?;
    parse_str(source)
}

/// Parse, normalize, and validate a UTF-8 service definition.
///
/// # Errors
///
/// Returns an error for malformed, unsupported, or semantically invalid input.
pub fn parse_str(source: &str) -> Result<ServiceConfig, ConfigError> {
    if source.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge {
            actual: source.len() as u64,
        });
    }

    let documents: Vec<IgnoredAny> =
        serde_saphyr::from_multiple_with_options(source, document::yaml_options())
            .map_err(|error| ConfigError::Parse(error.to_string()))?;
    if documents.len() != 1 {
        return Err(ConfigError::Parse(
            "configuration must contain exactly one YAML document".to_owned(),
        ));
    }

    let header: document::VersionHeader =
        serde_saphyr::from_str_with_options(source, document::yaml_options())
            .map_err(|error| ConfigError::Parse(error.to_string()))?;
    let config = match header.version {
        None => return Err(ConfigError::MissingVersion),
        Some(2) => document::parse_current(source)?,
        Some(version) => return Err(ConfigError::UnsupportedVersion(version)),
    };
    validate::validate(&config)?;
    Ok(config)
}

/// Validate one already-materialized, path-resolved service definition.
///
/// This applies exactly the policy and resolved-path checks the file parser
/// runs, in the same order, for callers which build a [`ServiceConfig`]
/// programmatically instead of parsing one. Command-line supervision mutates a
/// definition after construction, so it must re-validate here before any field
/// reaches process setup. Call it after [`resolve_paths`], since the resolved
/// path checks inspect the filesystem.
///
/// # Errors
///
/// Returns a validation error listing every policy bound, field relationship,
/// or resolved path the definition violates.
pub fn validate_service(config: &ServiceConfig) -> Result<(), ConfigError> {
    validate::validate(config)?;
    paths::validate_resolved(config)
}

/// Serialize a validated configuration using the single supported schema.
///
/// # Errors
///
/// Returns an error if the normalized model cannot be represented by the YAML
/// serializer.
pub fn emit_config(config: &ServiceConfig) -> Result<String, ConfigError> {
    #[derive(Serialize)]
    struct ConfigOutput<'a> {
        version: u8,
        #[serde(flatten)]
        service: &'a ServiceConfig,
    }

    validate::validate(config)?;
    serde_saphyr::to_string(&ConfigOutput {
        version: 2,
        service: config,
    })
    .map_err(|error| ConfigError::Parse(format!("unable to emit configuration: {error}")))
}

#[cfg(test)]
mod tests;
