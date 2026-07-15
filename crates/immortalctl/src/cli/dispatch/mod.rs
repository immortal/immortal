//! Conversion from CLI matches into typed control operations.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    path::PathBuf,
    time::Duration,
};

use clap::ArgMatches;
use immortal_core::control::{Operation, Signal, SignalScope};

use crate::cli::actions::{
    Action, ControlAction, OutputFormat, RuntimeDiscovery, RuntimeScope, Target,
};

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
    let discovery = matches.get_one::<String>("runtime-dir").map_or_else(
        || automatic_discovery(matches),
        |path| Ok(RuntimeDiscovery::Custom(PathBuf::from(path))),
    )?;
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
        return Ok(Action::from_operation(
            Operation::Signal,
            ControlAction {
                discovery,
                output,
                no_header,
                wait_timeout,
                no_wait,
                target: parse_target(target),
                scope: legacy_scope(signal),
                signal: Some(signal),
            },
        ));
    }

    let Some((name, subcommand)) = matches.subcommand() else {
        return Ok(Action::from_operation(
            Operation::Status,
            ControlAction {
                discovery,
                output,
                no_header,
                wait_timeout,
                no_wait,
                target: Target::All,
                scope: SignalScope::Main,
                signal: None,
            },
        ));
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
    let (signal, scope) = signal_fields(operation, subcommand)?;
    Ok(Action::from_operation(
        operation,
        ControlAction {
            discovery,
            output,
            no_header,
            wait_timeout,
            no_wait,
            target,
            scope,
            signal,
        },
    ))
}

fn signal_fields(
    operation: Operation,
    subcommand: &ArgMatches,
) -> Result<(Option<Signal>, SignalScope), DispatchError> {
    if operation != Operation::Signal {
        return Ok((None, SignalScope::Main));
    }
    let name = subcommand
        .get_one::<String>("signal")
        .map(String::as_str)
        .ok_or_else(|| DispatchError("signal operation requires a signal".to_owned()))?;
    let signal =
        Signal::from_name(name).ok_or_else(|| DispatchError(format!("unknown signal `{name}`")))?;
    let scope = match subcommand.get_one::<String>("scope").map(String::as_str) {
        Some("group") => SignalScope::Group,
        Some("main") => SignalScope::Main,
        Some(value) => return Err(DispatchError(format!("unknown signal scope `{value}`"))),
        None => {
            return Err(DispatchError(
                "signal operation requires a scope".to_owned(),
            ));
        }
    };
    Ok((Some(signal), scope))
}

fn automatic_discovery(matches: &ArgMatches) -> Result<RuntimeDiscovery, DispatchError> {
    let scope = match matches
        .get_one::<String>("runtime-scope")
        .map(String::as_str)
    {
        Some("all") => RuntimeScope::All,
        Some("system") => RuntimeScope::System,
        Some("user") => RuntimeScope::User,
        Some(value) => return Err(DispatchError(format!("unknown runtime scope `{value}`"))),
        None => return Err(DispatchError("missing runtime scope".to_owned())),
    };
    Ok(RuntimeDiscovery::Automatic(scope))
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

    use super::action;
    use crate::cli::{
        actions::{Action, ControlAction, OutputFormat, RuntimeDiscovery, RuntimeScope, Target},
        commands,
    };

    fn parts(action: &Action) -> (Operation, &ControlAction) {
        match action {
            Action::Status(control) => (Operation::Status, control),
            Action::Start(control) => (Operation::Start, control),
            Action::Stop(control) => (Operation::Stop, control),
            Action::Restart(control) => (Operation::Restart, control),
            Action::Once(control) => (Operation::Once, control),
            Action::Exit(control) => (Operation::Exit, control),
            Action::Halt(control) => (Operation::Halt, control),
            Action::Signal(control) => (Operation::Signal, control),
        }
    }

    fn control(action: &Action) -> &ControlAction {
        parts(action).1
    }

    #[test]
    fn default_is_status_for_all() -> Result<(), Box<dyn Error>> {
        let matches = commands::try_get_matches_from(["immortalctl"])?;
        let action = action(&matches)?;
        let (operation, control) = parts(&action);
        assert_eq!(operation, Operation::Status);
        assert_eq!(control.target, Target::All);
        assert_eq!(control.output, OutputFormat::Table);
        assert_eq!(control.wait_timeout, std::time::Duration::from_secs(30));
        assert!(!control.no_wait);
        assert_eq!(
            control.discovery,
            RuntimeDiscovery::Automatic(RuntimeScope::All)
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
        let (operation, control) = parts(&action);
        assert_eq!(operation, Operation::Signal);
        assert_eq!(control.signal, Some(Signal::User2));
        assert_eq!(control.scope, SignalScope::Group);
        assert_eq!(control.target, Target::Service("api".to_owned()));
        Ok(())
    }

    #[test]
    fn non_signal_subcommands_do_not_access_signal_arguments() -> Result<(), Box<dyn Error>> {
        for operation in ["status", "start", "stop", "restart", "once", "exit", "halt"] {
            let matches = commands::try_get_matches_from(["immortalctl", operation, "api"])?;
            let action = action(&matches)?;
            assert_eq!(control(&action).signal, None);
            assert_eq!(control(&action).scope, SignalScope::Main);
        }
        Ok(())
    }

    #[test]
    fn selects_automatic_user_or_exact_custom_runtime_roots() -> Result<(), Box<dyn Error>> {
        let user = commands::try_get_matches_from(["immortalctl", "--runtime-scope", "user"])?;
        assert_eq!(
            control(&action(&user)?).discovery,
            RuntimeDiscovery::Automatic(RuntimeScope::User)
        );

        let custom =
            commands::try_get_matches_from(["immortalctl", "--runtime-dir", "/tmp/immortal"])?;
        assert_eq!(
            control(&action(&custom)?).discovery,
            RuntimeDiscovery::Custom(std::path::PathBuf::from("/tmp/immortal"))
        );
        Ok(())
    }

    #[test]
    fn legacy_kill_targets_group_but_other_signals_target_main() -> Result<(), Box<dyn Error>> {
        let kill = commands::try_get_matches_from(["immortalctl", "-k", "api"])?;
        assert_eq!(control(&action(&kill)?).scope, SignalScope::Group);
        let hangup = commands::try_get_matches_from(["immortalctl", "-h", "api"])?;
        assert_eq!(control(&action(&hangup)?).scope, SignalScope::Main);
        Ok(())
    }

    #[test]
    fn legacy_star_means_all() -> Result<(), Box<dyn Error>> {
        let matches = commands::try_get_matches_from(["immortalctl", "-t", "*"])?;
        assert_eq!(control(&action(&matches)?).target, Target::All);
        Ok(())
    }

    #[test]
    fn rejects_conflicting_legacy_signals() -> Result<(), Box<dyn Error>> {
        let matches = commands::try_get_matches_from(["immortalctl", "-h", "-t", "api"])?;
        assert!(action(&matches).is_err());
        Ok(())
    }
}
