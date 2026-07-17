//! Deterministic command, environment, and identity preparation.
//!
//! `ProcessCommand` is the fully materialized, direct-exec request Immortal's
//! single-threaded process broker executes: an explicit executable path, argv,
//! environment snapshot, working directory, and any resolved numeric identity.
//! Building it here — before the broker forks — means every failure mode
//! (missing command, unresolved bare executable, unknown or forbidden account)
//! surfaces as a typed error in the supervisor rather than inside the child.
//!
//! Environment resolution never reads global process state implicitly; a
//! caller takes one explicit snapshot of the inherited environment before
//! daemonizing and reuses it for every subsequent generation, so behavior does
//! not depend on when during the supervisor's lifetime a command is built.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use crate::config::{EnvironmentMode, ServiceConfig};

/// Deterministic environment passed to a service or lifecycle hook.
pub type ProcessEnvironment = BTreeMap<OsString, OsString>;

/// Supplementary-group behavior resolved before daemonization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SupplementaryGroups {
    /// Preserve the caller's groups when an unprivileged supervisor remains the same user.
    Preserve,
    /// Replace the complete supplementary group list before setting GID and UID.
    Set(Vec<libc::gid_t>),
}

/// Numeric credentials transported to the single-threaded process broker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessCredentials {
    pub(super) user: libc::uid_t,
    pub(super) group: libc::gid_t,
    pub(super) supplementary_groups: SupplementaryGroups,
}

impl ProcessCredentials {
    /// Construct an explicit numeric identity transition.
    #[must_use]
    pub const fn new(
        user: libc::uid_t,
        group: libc::gid_t,
        supplementary_groups: SupplementaryGroups,
    ) -> Self {
        Self {
            user,
            group,
            supplementary_groups,
        }
    }

    /// Return the target UID.
    #[must_use]
    pub const fn user(&self) -> libc::uid_t {
        self.user
    }

    /// Return the target primary GID.
    #[must_use]
    pub const fn group(&self) -> libc::gid_t {
        self.group
    }

    /// Return the resolved supplementary-group policy.
    #[must_use]
    pub const fn supplementary_groups(&self) -> &SupplementaryGroups {
        &self.supplementary_groups
    }
}

/// Fully materialized direct-exec request passed to the process broker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessCommand {
    pub(super) program: OsString,
    pub(super) arguments: Vec<OsString>,
    pub(super) environment: ProcessEnvironment,
    pub(super) working_directory: Option<PathBuf>,
    pub(super) credentials: Option<ProcessCredentials>,
}

impl ProcessCommand {
    /// Build a command from one validated service and an explicit environment snapshot.
    ///
    /// The first configured argument is the executable. It is deliberately not
    /// resolved through `PATH`; configuration resolution must provide the exact
    /// executable expected by the operator.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` if the validated configuration unexpectedly has
    /// no executable.
    pub fn from_service(
        config: &ServiceConfig,
        inherited: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> io::Result<Self> {
        let (program, arguments) = config.command.split_first().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "service command is empty")
        })?;
        let environment = resolve_environment(config, inherited);
        Ok(Self {
            program: resolve_program(
                OsStr::new(program),
                &environment,
                config.working_directory.as_deref(),
            )?,
            arguments: arguments.iter().map(OsString::from).collect(),
            environment,
            working_directory: config.working_directory.clone(),
            credentials: resolve_credentials(config.user.as_deref())?,
        })
    }

    /// Build a lifecycle command with the service's resolved execution context.
    ///
    /// The environment, working directory, and credentials are copied once
    /// during supervisor preparation. Repeated attempts then use the same
    /// deterministic inputs as the service command.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` when the argv is empty, or `NotFound` when a bare
    /// executable cannot be resolved from the service environment.
    pub(crate) fn from_lifecycle(command: &[String], service: &Self) -> io::Result<Self> {
        let (program, arguments) = command.split_first().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "lifecycle command is empty")
        })?;
        Self::from_lifecycle_os(
            OsStr::new(program),
            arguments.iter().map(OsString::from).collect(),
            service,
        )
    }

    pub(crate) fn from_lifecycle_os(
        program: &OsStr,
        arguments: Vec<OsString>,
        service: &Self,
    ) -> io::Result<Self> {
        Ok(Self {
            program: resolve_program(
                program,
                &service.environment,
                service.working_directory.as_deref(),
            )?,
            arguments,
            environment: service.environment.clone(),
            working_directory: service.working_directory.clone(),
            credentials: service.credentials.clone(),
        })
    }

    /// Build an explicit command, primarily for hooks and lifecycle contracts.
    #[must_use]
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            environment: ProcessEnvironment::new(),
            working_directory: None,
            credentials: None,
        }
    }

    /// Append one direct argument without shell interpretation.
    pub fn argument(&mut self, argument: impl Into<OsString>) -> &mut Self {
        self.arguments.push(argument.into());
        self
    }

    /// Replace the complete environment passed to the child.
    pub fn environment(&mut self, environment: ProcessEnvironment) -> &mut Self {
        self.environment = environment;
        self
    }

    /// Insert one broker-owned environment field after command materialization.
    pub(crate) fn environment_variable(
        &mut self,
        key: impl Into<OsString>,
        value: impl Into<OsString>,
    ) {
        self.environment.insert(key.into(), value.into());
    }

    /// Set the directory entered immediately before execution.
    pub fn working_directory(&mut self, directory: impl Into<PathBuf>) -> &mut Self {
        self.working_directory = Some(directory.into());
        self
    }

    /// Apply an already-resolved numeric identity to the child.
    pub fn credentials(&mut self, credentials: ProcessCredentials) -> &mut Self {
        self.credentials = Some(credentials);
        self
    }

    /// Return the exact executable path.
    #[must_use]
    pub fn program(&self) -> &OsStr {
        &self.program
    }

    /// Return the direct argument vector excluding argv zero.
    #[must_use]
    pub fn arguments(&self) -> &[OsString] {
        &self.arguments
    }

    /// Return the complete child environment.
    #[must_use]
    pub fn resolved_environment(&self) -> &ProcessEnvironment {
        &self.environment
    }

    /// Return the requested child working directory.
    #[must_use]
    pub fn requested_working_directory(&self) -> Option<&std::path::Path> {
        self.working_directory.as_deref()
    }

    /// Return the requested numeric identity transition.
    #[must_use]
    pub const fn requested_credentials(&self) -> Option<&ProcessCredentials> {
        self.credentials.as_ref()
    }
}

fn resolve_credentials(user: Option<&str>) -> io::Result<Option<ProcessCredentials>> {
    let Some(user) = user else {
        return Ok(None);
    };
    let identity = crate::platform::resolve_account(user)?;
    let effective_user = nix::unistd::geteuid().as_raw();
    let effective_group = nix::unistd::getegid().as_raw();
    let supplementary_groups = if effective_user == 0 {
        SupplementaryGroups::Set(identity.supplementary_groups)
    } else if identity.user == effective_user && identity.group == effective_group {
        SupplementaryGroups::Preserve
    } else {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "an unprivileged supervisor may only select its current account and primary group",
        ));
    };
    Ok(Some(ProcessCredentials::new(
        identity.user,
        identity.group,
        supplementary_groups,
    )))
}

fn resolve_program(
    program: &OsStr,
    environment: &ProcessEnvironment,
    working_directory: Option<&std::path::Path>,
) -> io::Result<OsString> {
    if program.as_encoded_bytes().contains(&b'/') {
        return Ok(program.to_os_string());
    }
    let path = environment.get(OsStr::new("PATH")).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "bare executable requires PATH in the resolved environment",
        )
    })?;
    let base =
        working_directory.map_or_else(std::env::current_dir, |path| Ok(path.to_path_buf()))?;
    for directory in std::env::split_paths(path) {
        let directory = if directory.as_os_str().is_empty() {
            base.clone()
        } else if directory.is_absolute() {
            directory
        } else {
            base.join(directory)
        };
        let candidate = directory.join(program);
        if candidate
            .metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        {
            return Ok(candidate.into_os_string());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "executable `{}` was not found in PATH",
            program.to_string_lossy()
        ),
    ))
}

/// Resolve the process environment without reading global state implicitly.
///
/// In inherited mode, entries are copied in iterator order and later duplicate
/// keys replace earlier ones. Configured UTF-8 values are then applied last. In
/// clear mode, only configured values are present. A caller can therefore take
/// one explicit snapshot of `std::env::vars_os()` before daemonization and use
/// the same inputs for every generation.
#[must_use]
pub fn resolve_environment(
    config: &ServiceConfig,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> ProcessEnvironment {
    let mut resolved = if config.environment_mode == EnvironmentMode::Inherit {
        inherited.into_iter().collect()
    } else {
        ProcessEnvironment::new()
    };
    resolved.extend(
        config
            .environment
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value))),
    );
    resolved
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::ffi::{OsStr, OsString};
    use std::io;

    use crate::config::{EnvironmentMode, ServiceConfig, parse_str};

    use super::{ProcessCommand, ProcessEnvironment, resolve_environment};

    #[test]
    fn configured_values_override_one_explicit_inherited_snapshot()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = parse_str(
            "version: 2\ncommand: [service]\nenvironment:\n  KEEP: configured\n  NEW: value\n",
        )?;
        let environment = resolve_environment(
            &config,
            [
                (OsString::from("KEEP"), OsString::from("old")),
                (OsString::from("BASE"), OsString::from("base")),
            ],
        );
        assert_eq!(
            environment.get(OsStr::new("KEEP")),
            Some(&OsString::from("configured"))
        );
        assert_eq!(
            environment.get(OsStr::new("BASE")),
            Some(&OsString::from("base"))
        );
        assert_eq!(
            environment.get(OsStr::new("NEW")),
            Some(&OsString::from("value"))
        );
        Ok(())
    }

    #[test]
    fn clear_mode_discards_every_inherited_entry() -> Result<(), Box<dyn Error>> {
        let mut config = parse_str("version: 2\ncommand: [service]\n")?;
        config.environment_mode = EnvironmentMode::Clear;
        config
            .environment
            .insert("ONLY".to_owned(), "configured".to_owned());
        let environment = resolve_environment(
            &config,
            [(OsString::from("SECRET"), OsString::from("inherited"))],
        );
        assert_eq!(environment.len(), 1);
        assert_eq!(
            environment.get(OsStr::new("ONLY")),
            Some(&OsString::from("configured"))
        );
        Ok(())
    }

    #[test]
    fn service_command_materializes_direct_execution_inputs() -> Result<(), Box<dyn Error>> {
        let config =
            parse_str("version: 2\ncommand: [/bin/sleep, '5']\nenvironment:\n  MODE: test\n")?;
        let command = ProcessCommand::from_service(
            &config,
            [(OsString::from("INHERITED"), OsString::from("yes"))],
        )?;
        assert_eq!(command.program(), OsStr::new("/bin/sleep"));
        assert_eq!(command.arguments(), [OsString::from("5")]);
        assert_eq!(
            command.resolved_environment().get(OsStr::new("MODE")),
            Some(&OsString::from("test"))
        );
        assert_eq!(
            command.resolved_environment().get(OsStr::new("INHERITED")),
            Some(&OsString::from("yes"))
        );
        assert_eq!(command.requested_working_directory(), None);
        Ok(())
    }

    #[test]
    fn configured_account_is_resolved_before_broker_start() -> Result<(), Box<dyn Error>> {
        let effective = nix::unistd::geteuid();
        let account = nix::unistd::User::from_uid(effective)?.ok_or_else(|| {
            io::Error::other("effective account is absent from the user database")
        })?;
        let mut config = ServiceConfig::for_command(vec!["/bin/true".to_owned()])?;
        config.user = Some(account.name);
        let command = ProcessCommand::from_service(&config, ProcessEnvironment::new())?;
        let credentials = command
            .requested_credentials()
            .ok_or_else(|| io::Error::other("configured account was not materialized"))?;
        assert_eq!(credentials.user(), effective.as_raw());
        assert_eq!(credentials.group(), account.gid.as_raw());
        Ok(())
    }

    #[test]
    fn unknown_account_fails_before_broker_start() -> Result<(), Box<dyn Error>> {
        let mut config = ServiceConfig::for_command(vec!["/bin/true".to_owned()])?;
        config.user = Some("immortal-account-that-must-not-exist-7f9b".to_owned());
        let error = ProcessCommand::from_service(&config, ProcessEnvironment::new())
            .err()
            .ok_or_else(|| io::Error::other("unknown account unexpectedly resolved"))?;
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        Ok(())
    }

    #[test]
    fn bare_service_program_is_resolved_before_broker_start() -> Result<(), Box<dyn Error>> {
        let config = parse_str("version: 2\ncommand: [sh, -c, 'exit 0']\n")?;
        let command = ProcessCommand::from_service(
            &config,
            [(OsString::from("PATH"), OsString::from("/bin:/usr/bin"))],
        )?;
        assert!(std::path::Path::new(command.program()).is_absolute());
        assert!(command.program().to_string_lossy().ends_with("/sh"));
        Ok(())
    }

    #[test]
    fn bare_program_without_path_fails_before_broker_start() -> Result<(), Box<dyn Error>> {
        let config = parse_str(
            "version: 2\ncommand: [definitely-not-an-immortal-command]\nenvironment_mode: clear\n",
        )?;
        let error = ProcessCommand::from_service(&config, ProcessEnvironment::new())
            .err()
            .ok_or_else(|| io::Error::other("missing bare program unexpectedly resolved"))?;
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        Ok(())
    }
}
