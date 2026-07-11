//! Conversion from CLI matches into typed control operations.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    path::PathBuf,
    time::Duration,
};

use clap::ArgMatches;
use immortal_core::control::{Operation, Signal, SignalScope};

/// Target selected by an operator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    /// One named service.
    Service(String),
    /// Every safely discovered service.
    All,
}

/// Stable operator output format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputFormat {
    /// Human-readable fixed columns.
    Table,
    /// Machine-readable JSON array.
    Json,
}

/// Fully typed control action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Action {
    /// Permission-validated runtime discovery root.
    pub runtime_directory: PathBuf,
    /// Output representation.
    pub output: OutputFormat,
    /// Omit the table header.
    pub no_header: bool,
    /// Hard lifecycle completion deadline.
    pub wait_timeout: Duration,
    /// Return after request acceptance instead of polling completion.
    pub no_wait: bool,
    /// Lifecycle or signal operation.
    pub operation: Operation,
    /// One service or the discovery set.
    pub target: Target,
    /// Explicit raw-signal target.
    pub scope: SignalScope,
    /// Raw signal, present only for [`Operation::Signal`].
    pub signal: Option<Signal>,
}

/// CLI matches violated a control-action invariant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DispatchError(String);

impl Display for DispatchError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for DispatchError {}

/// Convert matches into one typed control action.
///
/// # Errors
///
/// Returns an error for multiple legacy signal flags, invalid signal names,
/// or missing parser invariants.
pub fn action(matches: &ArgMatches) -> Result<Action, DispatchError> {
    let runtime_directory = matches
        .get_one::<String>("runtime-dir")
        .map(PathBuf::from)
        .ok_or_else(|| DispatchError("missing runtime directory".to_owned()))?;
    let output = output_format(matches)?;
    let no_header = matches.get_flag("no-header");
    let wait_timeout = matches
        .get_one::<u64>("timeout")
        .copied()
        .map(Duration::from_secs)
        .ok_or_else(|| DispatchError("missing lifecycle timeout".to_owned()))?;
    let no_wait = matches.get_flag("no-wait");
    if let Some(signals) = matches.get_many::<String>("legacy-signal") {
        let values: Vec<&str> = signals.map(String::as_str).collect();
        if values.len() != 1 {
            return Err(DispatchError(
                "exactly one legacy signal flag may be supplied".to_owned(),
            ));
        }
        let signal = values
            .first()
            .and_then(|name| Signal::from_name(name))
            .ok_or_else(|| DispatchError("invalid legacy signal".to_owned()))?;
        let target = matches
            .get_one::<String>("legacy-target")
            .ok_or_else(|| DispatchError("legacy signal requires a service".to_owned()))?;
        return Ok(Action {
            runtime_directory,
            output,
            no_header,
            wait_timeout,
            no_wait,
            operation: Operation::Signal,
            target: parse_target(target),
            scope: legacy_scope(signal),
            signal: Some(signal),
        });
    }

    let Some((name, subcommand)) = matches.subcommand() else {
        return Ok(Action {
            runtime_directory,
            output,
            no_header,
            wait_timeout,
            no_wait,
            operation: Operation::Status,
            target: Target::All,
            scope: SignalScope::Main,
            signal: None,
        });
    };
    let operation = match name {
        "status" => Operation::Status,
        "start" => Operation::Start,
        "stop" => Operation::Stop,
        "restart" => Operation::Restart,
        "once" => Operation::Once,
        "exit" => Operation::Exit,
        "halt" => Operation::Halt,
        "signal" => Operation::Signal,
        unknown => {
            return Err(DispatchError(format!(
                "unknown control operation `{unknown}`"
            )));
        }
    };
    let target = if subcommand.get_flag("all") {
        Target::All
    } else if let Some(service) = subcommand.get_one::<String>("service") {
        parse_target(service)
    } else if operation == Operation::Status {
        Target::All
    } else {
        return Err(DispatchError(
            "control operation requires a target".to_owned(),
        ));
    };
    let signal = subcommand
        .get_one::<String>("signal")
        .map(String::as_str)
        .map(|name| {
            Signal::from_name(name).ok_or_else(|| DispatchError(format!("unknown signal `{name}`")))
        })
        .transpose()?;
    let scope = match subcommand.get_one::<String>("scope").map(String::as_str) {
        Some("group") => SignalScope::Group,
        Some("main") | None => SignalScope::Main,
        Some(value) => return Err(DispatchError(format!("unknown signal scope `{value}`"))),
    };
    Ok(Action {
        runtime_directory,
        output,
        no_header,
        wait_timeout,
        no_wait,
        operation,
        target,
        scope,
        signal,
    })
}

fn output_format(matches: &ArgMatches) -> Result<OutputFormat, DispatchError> {
    match matches.get_one::<String>("output").map(String::as_str) {
        Some("table") => Ok(OutputFormat::Table),
        Some("json") => Ok(OutputFormat::Json),
        Some(value) => Err(DispatchError(format!("unknown output format `{value}`"))),
        None => Err(DispatchError("missing output format".to_owned())),
    }
}

fn parse_target(service: &str) -> Target {
    if service == "*" {
        Target::All
    } else {
        Target::Service(service.to_owned())
    }
}

const fn legacy_scope(signal: Signal) -> SignalScope {
    if matches!(signal, Signal::Kill) {
        SignalScope::Group
    } else {
        SignalScope::Main
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use immortal_core::control::{Operation, Signal, SignalScope};

    use super::{OutputFormat, Target, action};
    use crate::cli::commands;

    #[test]
    fn default_is_status_for_all() -> Result<(), Box<dyn Error>> {
        let matches = commands::try_get_matches_from(["immortalctl"])?;
        let action = action(&matches)?;
        assert_eq!(action.operation, Operation::Status);
        assert_eq!(action.target, Target::All);
        assert_eq!(action.output, OutputFormat::Table);
        assert_eq!(action.wait_timeout, std::time::Duration::from_secs(30));
        assert!(!action.no_wait);
        assert_eq!(
            action.runtime_directory,
            std::path::PathBuf::from("/var/run/immortal")
        );
        Ok(())
    }

    #[test]
    fn maps_modern_signal_and_scope() -> Result<(), Box<dyn Error>> {
        let matches = commands::try_get_matches_from([
            "immortalctl",
            "signal",
            "usr2",
            "api",
            "--scope",
            "group",
        ])?;
        let action = action(&matches)?;
        assert_eq!(action.signal, Some(Signal::User2));
        assert_eq!(action.scope, SignalScope::Group);
        assert_eq!(action.target, Target::Service("api".to_owned()));
        Ok(())
    }

    #[test]
    fn legacy_kill_targets_group_but_other_signals_target_main() -> Result<(), Box<dyn Error>> {
        let kill = commands::try_get_matches_from(["immortalctl", "-k", "api"])?;
        assert_eq!(action(&kill)?.scope, SignalScope::Group);
        let hangup = commands::try_get_matches_from(["immortalctl", "-h", "api"])?;
        assert_eq!(action(&hangup)?.scope, SignalScope::Main);
        Ok(())
    }

    #[test]
    fn legacy_star_means_all() -> Result<(), Box<dyn Error>> {
        let matches = commands::try_get_matches_from(["immortalctl", "-t", "*"])?;
        assert_eq!(action(&matches)?.target, Target::All);
        Ok(())
    }

    #[test]
    fn rejects_conflicting_legacy_signals() -> Result<(), Box<dyn Error>> {
        let matches = commands::try_get_matches_from(["immortalctl", "-h", "-t", "api"])?;
        assert!(action(&matches).is_err());
        Ok(())
    }
}
