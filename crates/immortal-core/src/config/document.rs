//! Strict `version: 2` wire schema and its single parsing entry point.
//!
//! [`ConfigDocument`] mirrors exactly the fields accepted by one YAML
//! document; unknown keys are rejected so migrations and typos surface
//! immediately instead of silently no-oping. [`parse_current`] destructures
//! one decoded document into the canonical [`super::model::ServiceConfig`],
//! delegating every logging alias and legacy shape to [`super::logging`].
//! [`yaml_options`] centralizes the bounded parser budget shared by every
//! decode call in the configuration pipeline, so a hostile or malformed
//! document cannot exhaust memory or recursion before validation ever runs.

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Deserializer};

use super::{
    CommandHook, ConfigError, DescriptorTrackingConfig, EnvironmentMode, LoggerRestartConfig,
    MAX_CONFIG_BYTES, PidFiles, ProcessMode, ReadinessConfig, RestartConfig, ServiceConfig,
    StartConditionConfig,
    logging::{FileLogInput, LegacyLoggingConfig, LogInput, normalize_logging},
};

/// The only schema version this decoder accepts.
pub(super) const CURRENT_VERSION: u8 = 2;

/// Minimal probe used to select a schema before the full document is decoded.
#[derive(Deserialize)]
pub(super) struct VersionHeader {
    pub(super) version: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigDocument {
    version: u8,
    #[serde(default = "default_true")]
    enabled: bool,
    command: Vec<String>,
    working_directory: Option<PathBuf>,
    #[serde(default, alias = "env", deserialize_with = "deserialize_environment")]
    environment: BTreeMap<String, String>,
    #[serde(default)]
    environment_mode: EnvironmentMode,
    user: Option<String>,
    #[serde(default)]
    start_delay_seconds: u64,
    #[serde(default)]
    restart: RestartConfig,
    #[serde(default)]
    readiness: ReadinessConfig,
    #[serde(default)]
    requires: Vec<String>,
    start_condition: Option<StartConditionConfig>,
    post_exit: Option<CommandHook>,
    log: Option<LogInput>,
    logger: Option<Vec<String>>,
    log_adapter: Option<PathBuf>,
    logger_restart: Option<LoggerRestartConfig>,
    /// Deprecated Go-compatible alias for `log.stderr`.
    stderr: Option<FileLogInput>,
    /// Deprecated Rust v2 logging shape accepted during migration.
    logging: Option<LegacyLoggingConfig>,
    #[serde(default)]
    pid_files: PidFiles,
    #[serde(default)]
    process_mode: ProcessMode,
    descriptor_tracking: Option<DescriptorTrackingConfig>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum EnvironmentValue {
    Text(String),
    Signed(i64),
    Unsigned(u64),
    Float(f64),
    Boolean(bool),
}

impl EnvironmentValue {
    fn into_string(self) -> String {
        match self {
            Self::Text(value) => value,
            Self::Signed(value) => value.to_string(),
            Self::Unsigned(value) => value.to_string(),
            Self::Float(value) => value.to_string(),
            Self::Boolean(value) => value.to_string(),
        }
    }
}

fn deserialize_environment<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    BTreeMap::<String, EnvironmentValue>::deserialize(deserializer).map(|environment| {
        environment
            .into_iter()
            .map(|(key, value)| (key, value.into_string()))
            .collect()
    })
}

fn default_true() -> bool {
    true
}

/// Decode one already version-selected `version: 2` document into the canonical model.
///
/// # Errors
///
/// Returns an error when the document does not match the strict schema or
/// its logging fields cannot be normalized into one canonical route set.
pub(super) fn parse_current(source: &str) -> Result<ServiceConfig, ConfigError> {
    let document: ConfigDocument = serde_saphyr::from_str_with_options(source, yaml_options())
        .map_err(|error| ConfigError::Parse(error.to_string()))?;
    // The caller selects the version before dispatching here, but this decoder
    // is what binds the strict `version: 2` schema to the canonical model. A
    // real check keeps that binding true in release builds too, rather than
    // relying on an assertion which compiles away.
    if document.version != CURRENT_VERSION {
        return Err(ConfigError::UnsupportedVersion(u64::from(document.version)));
    }
    let ConfigDocument {
        version: _,
        enabled,
        command,
        working_directory,
        environment,
        environment_mode,
        user,
        start_delay_seconds,
        restart,
        readiness,
        requires,
        start_condition,
        post_exit,
        log,
        logger,
        log_adapter,
        logger_restart,
        stderr,
        logging,
        pid_files,
        process_mode,
        descriptor_tracking,
    } = document;
    let logging = normalize_logging(log, logger, log_adapter, logger_restart, stderr, logging)?;
    Ok(ServiceConfig {
        enabled,
        command,
        working_directory,
        environment,
        environment_mode,
        user,
        start_delay_seconds,
        restart,
        readiness,
        requires,
        start_condition,
        post_exit,
        logging,
        pid_files,
        process_mode,
        descriptor_tracking,
    })
}

/// Bounded YAML parser budget shared by every decode call in this pipeline.
pub(super) fn yaml_options() -> serde_saphyr::Options {
    serde_saphyr::options! {
        budget: serde_saphyr::budget! {
            max_events: 100_000,
            max_aliases: 128,
            max_anchors: 128,
            max_depth: 32,
            max_documents: 2,
            max_nodes: 50_000,
            max_total_scalar_bytes: MAX_CONFIG_BYTES,
            max_total_comment_bytes: MAX_CONFIG_BYTES,
            max_merge_keys: 128,
        },
        alias_limits: serde_saphyr::alias_limits! {
            max_total_replayed_events: 100_000,
            max_replay_stack_depth: 32,
            max_alias_expansions_per_anchor: 128,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, error::Error};

    use super::parse_current;
    use crate::config::{ConfigError, MAX_CONFIG_BYTES, emit_config, parse_bytes, parse_str};

    #[test]
    fn env_alias_normalizes_scalars_and_emits_canonical_environment() -> Result<(), Box<dyn Error>>
    {
        let config = parse_str(
            "version: 2\ncommand: [/bin/true]\nenv:\n  DEBUG: 1\n  ENVIRONMENT: production\n  ENABLED: true\n",
        )?;
        assert_eq!(
            config.environment,
            BTreeMap::from([
                ("DEBUG".to_owned(), "1".to_owned()),
                ("ENABLED".to_owned(), "true".to_owned()),
                ("ENVIRONMENT".to_owned(), "production".to_owned()),
            ])
        );

        let emitted = emit_config(&config)?;
        assert!(emitted.lines().any(|line| line == "environment:"));
        assert!(!emitted.lines().any(|line| line == "env:"));
        Ok(())
    }

    #[test]
    fn env_alias_rejects_duplicate_and_non_scalar_values() {
        let duplicate = parse_str(
            "version: 2\ncommand: [/bin/true]\nenv:\n  DEBUG: 1\nenvironment:\n  MODE: test\n",
        );
        assert!(duplicate.is_err());

        let nested = parse_str("version: 2\ncommand: [/bin/true]\nenv:\n  DEBUG: [one, two]\n");
        assert!(nested.is_err());
        let null = parse_str("version: 2\ncommand: [/bin/true]\nenv:\n  DEBUG: null\n");
        assert!(null.is_err());
    }

    #[test]
    fn schema_selection_requires_explicit_version_two() {
        for legacy in [
            "cmd: /bin/true\n",
            "cmd: /bin/true\npid:\n  follow: /run/service.pid\n",
            "command: [/bin/true]\nrestart:\n  policy: never\n",
            "cmd: /bin/true\ncommand: [/bin/false]\n",
        ] {
            assert!(matches!(
                parse_str(legacy),
                Err(ConfigError::MissingVersion)
            ));
        }
        assert!(parse_str("cmd: /bin/true\n").is_err_and(|error| {
            error
                .to_string()
                .contains("INSTALL.md#definition-migration")
        }));
        assert!(matches!(
            parse_str("version: 1\ncmd: /bin/true\n"),
            Err(ConfigError::UnsupportedVersion(1))
        ));
        assert!(matches!(
            parse_str("version: 3\ncommand: [/bin/true]\n"),
            Err(ConfigError::UnsupportedVersion(3))
        ));
        assert!(matches!(
            parse_str("version: 2\ncommand: [/bin/true]\nfuture: value\n"),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn rejects_unsupported_versions_and_multiple_documents() {
        assert!(
            parse_str("version: 2\ncommand: [/bin/true]\n---\nversion: 2\ncommand: [/bin/false]\n")
                .is_err()
        );
    }

    #[test]
    fn rejects_duplicate_keys_excessive_depth_and_aliases() {
        assert!(matches!(
            parse_str("version: 2\ncommand: [/bin/true]\ncommand: [/bin/false]\n"),
            Err(ConfigError::Parse(_))
        ));

        let mut deeply_nested = "version: 2\ncommand: [/bin/true]\nfuture: ".to_owned();
        for _ in 0..40 {
            deeply_nested.push('[');
        }
        deeply_nested.push_str("value");
        for _ in 0..40 {
            deeply_nested.push(']');
        }
        assert!(matches!(
            parse_str(&deeply_nested),
            Err(ConfigError::Parse(_))
        ));

        let mut aliases =
            "version: 2\ncommand: [/bin/true]\nanchor: &value item\nfuture: [".to_owned();
        for position in 0..129 {
            if position != 0 {
                aliases.push_str(", ");
            }
            aliases.push_str("*value");
        }
        aliases.push_str("]\n");
        assert!(matches!(parse_str(&aliases), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn rejects_oversized_and_non_utf8_documents() {
        let oversized = vec![b'a'; MAX_CONFIG_BYTES + 1];
        assert!(matches!(
            parse_bytes(&oversized),
            Err(ConfigError::TooLarge { .. })
        ));
        assert!(matches!(parse_bytes(&[0xff]), Err(ConfigError::Parse(_))));
    }

    /// The version binding is enforced in release builds, not just asserted.
    ///
    /// A `debug_assert` compiled away outside debug builds, so a decoder
    /// reached with the wrong schema would have silently produced a document
    /// built from another version's field meanings.
    #[test]
    fn parse_current_rejects_a_document_which_is_not_version_two() {
        assert!(matches!(
            parse_current("version: 4\ncommand: [/bin/true]\n"),
            Err(ConfigError::UnsupportedVersion(4))
        ));
    }

    /// The supported version still decodes.
    #[test]
    fn parse_current_accepts_version_two() -> Result<(), Box<dyn Error>> {
        let config = parse_current("version: 2\ncommand: [/bin/true]\n")?;
        assert_eq!(config.command, vec!["/bin/true".to_owned()]);
        Ok(())
    }
}
