//! Definition-relative path resolution applied before daemonization.
//!
//! [`resolve_paths`] rewrites every configuration path so its meaning
//! survives a working-directory change: the service executable resolves
//! against the (already resolved) working directory, while every other
//! executable, log, and PID path resolves against the configuration file's
//! own directory. Bare executable names are left untouched so the process
//! executor can still resolve them through `PATH`. [`validate_resolved`]
//! then checks that the resolved working directory actually exists, a
//! property only knowable once resolution has run.

use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use super::{ConfigError, FileLogConfig, FileLogRoutes, ServiceConfig};

/// Resolve every path whose meaning would otherwise change after daemonization.
///
/// Service executable paths containing `/` are relative to the resolved working
/// directory. Hook/logger executables and file/PID paths are relative to the
/// configuration directory. Bare executable names remain available for `PATH`
/// lookup by the process executor.
///
/// # Errors
///
/// Returns an error if an absolute executable path cannot be represented as
/// UTF-8 by the argv-based configuration model.
pub fn resolve_paths(config: &mut ServiceConfig, base: &Path) -> Result<(), ConfigError> {
    let base = normalize_absolute(base, Path::new("."));
    if let Some(directory) = &mut config.working_directory {
        *directory = normalize_absolute(&base, directory);
    }
    let command_base = config.working_directory.as_deref().unwrap_or(&base);
    resolve_argv_executable(&mut config.command, command_base)?;
    if let Some(hook) = &mut config.start_condition {
        resolve_argv_executable(&mut hook.command, &base)?;
    }
    if let Some(hook) = &mut config.post_exit {
        resolve_argv_executable(&mut hook.command, &base)?;
    }
    if let Some(tracking) = &mut config.descriptor_tracking {
        resolve_argv_executable(&mut tracking.stop.command, &base)?;
        resolve_argv_executable(&mut tracking.reload.command, &base)?;
    }
    resolve_optional_path(&mut config.logging.file_adapter, &base);
    if let Some(files) = &mut config.logging.files {
        match files {
            FileLogRoutes::Combined(file) => resolve_file_log_path(file, &base),
            FileLogRoutes::Selected { stdout, stderr } => {
                if let Some(file) = stdout {
                    resolve_file_log_path(file, &base);
                }
                if let Some(file) = stderr {
                    resolve_file_log_path(file, &base);
                }
            }
        }
    }
    if let Some(logger) = &mut config.logging.logger {
        resolve_argv_executable(logger, &base)?;
    }
    resolve_optional_path(&mut config.pid_files.supervisor, &base);
    resolve_optional_path(&mut config.pid_files.main, &base);
    Ok(())
}

fn resolve_file_log_path(file: &mut FileLogConfig, base: &Path) {
    file.file = normalize_absolute(base, &file.file);
}

fn resolve_optional_path(path: &mut Option<PathBuf>, base: &Path) {
    if let Some(value) = path {
        *value = normalize_absolute(base, value);
    }
}

fn resolve_argv_executable(argv: &mut [String], base: &Path) -> Result<(), ConfigError> {
    let Some(executable) = argv.first_mut() else {
        return Ok(());
    };
    if !executable.as_bytes().contains(&b'/') {
        return Ok(());
    }
    let resolved = normalize_absolute(base, Path::new(executable));
    *executable = resolved.into_os_string().into_string().map_err(|_| {
        ConfigError::Validation(vec![
            "resolved executable path is not valid UTF-8".to_owned(),
        ])
    })?;
    Ok(())
}

fn normalize_absolute(base: &Path, path: &Path) -> PathBuf {
    let candidate = if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    normalized
}

/// Confirm every resolved, existence-dependent path is actually usable.
///
/// # Errors
///
/// Returns an error when the resolved working directory is missing or is
/// not a directory.
pub(super) fn validate_resolved(config: &ServiceConfig) -> Result<(), ConfigError> {
    let mut errors = Vec::new();
    if let Some(directory) = &config.working_directory {
        match fs::metadata(directory) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => errors.push(format!(
                "working_directory `{}` is not a directory",
                directory.display()
            )),
            Err(error) => errors.push(format!(
                "working_directory `{}` is unavailable: {error}",
                directory.display()
            )),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::Validation(errors))
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, fs, io, path::Path};

    use crate::config::{EnvironmentMode, FileLogRoutes, parse_file, parse_str};

    use super::{resolve_paths, validate_resolved};

    #[test]
    fn file_parser_resolves_paths_before_daemonization() -> Result<(), Box<dyn Error>> {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = manifest.join("tests/fixtures/v2/relative.yml");
        let base = manifest.join("tests/fixtures/v2");
        let config = parse_file(&source)?;

        assert_eq!(config.environment_mode, EnvironmentMode::Clear);
        assert_eq!(config.working_directory, Some(base.clone()));
        assert_eq!(
            config.command.first().map(String::as_str),
            base.join("bin/api").to_str()
        );
        let Some(FileLogRoutes::Selected {
            stdout: Some(stdout),
            ..
        }) = &config.logging.files
        else {
            return Err("relative stdout log route is missing".into());
        };
        assert_eq!(stdout.file, base.join("logs/api.log"));
        assert_eq!(
            config.logging.file_adapter,
            Some(base.join("bin/immortallog"))
        );
        assert_eq!(config.pid_files.main, Some(base.join("run/main.pid")));
        assert_eq!(
            config
                .start_condition
                .as_ref()
                .and_then(|hook| hook.command.first())
                .map(String::as_str),
            base.join("checks/network-ready").to_str()
        );
        Ok(())
    }

    #[test]
    fn file_parser_rejects_missing_working_directory() -> Result<(), Box<dyn Error>> {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = manifest.join("tests/fixtures/v2/relative.yml");
        let yaml = fs::read_to_string(source)?.replace(
            "working_directory: .",
            "working_directory: definitely-missing-directory",
        );
        let mut config = parse_str(&yaml)?;
        let base = manifest.join("tests/fixtures/v2");
        resolve_paths(&mut config, &base)?;
        assert!(validate_resolved(&config).is_err());
        Ok(())
    }

    #[test]
    fn descriptor_tracking_hook_paths_resolve_from_definition_directory()
    -> Result<(), Box<dyn Error>> {
        let mut config = parse_str(
            "version: 2\ncommand: [/bin/true]\nprocess_mode: descriptor-tracking\ndescriptor_tracking:\n  stop:\n    command: [hooks/stop]\n    timeout_seconds: 1\n  reload:\n    command: [hooks/reload]\n    timeout_seconds: 1\n  lifetime_timeout_seconds: 1\n",
        )?;
        resolve_paths(&mut config, Path::new("/srv/immortal"))?;
        let tracking = config
            .descriptor_tracking
            .ok_or_else(|| io::Error::other("descriptor tracking configuration is absent"))?;
        assert_eq!(
            tracking.stop.command.first().map(String::as_str),
            Some("/srv/immortal/hooks/stop")
        );
        assert_eq!(
            tracking.reload.command.first().map(String::as_str),
            Some("/srv/immortal/hooks/reload")
        );
        Ok(())
    }
}
