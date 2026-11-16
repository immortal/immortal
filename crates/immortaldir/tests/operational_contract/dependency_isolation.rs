//! Real one-shot passes over a dependency graph that cannot be fully met.
//!
//! One unresolvable requirement, or one invalid definition, must cost only
//! the services it affects: every pass still drains stops, starts unrelated
//! services, and reports the problem in its result afterwards. The passes run
//! against real supervisors, each with a broker created before the Tokio
//! runtime, and every supervisor is halted on success and failure.

use std::{error::Error, fs, os::unix::fs::PermissionsExt, path::Path};

use immortal_core::{
    exit::ExitClass,
    process::{ProcessBrokerEndpoint, start_process_broker},
    reconcile::LaunchConcurrency,
    status::ServiceState,
};
use immortaldir::cli::actions::{ActionError, ReconcileAction as DirectoryAction, reconcile};
use tokio::runtime::Builder;

use super::{
    deletion_confirmation::wait_for_first_deletion_confirmation, halt_if_present,
    wait_for_ready_command, wait_for_service_absence, wait_for_service_state,
};

/// Regression: one unresolvable `requires` must not wedge the whole pass.
///
/// `dependency_plan` used to fail on the first unavailable dependency, and the
/// `?` aborted reconciliation before `apply_stops`. Every unrelated service
/// then stayed unstarted and every pending stop stayed pending, on that pass
/// and on every later one, because the graph never healed on its own.
pub(super) fn prove_dependency_isolation(root: &Path) -> Result<(), Box<dyn Error>> {
    let definitions = root.join("dependency-definitions");
    let runtime = root.join("dependency-runtime");
    fs::create_dir(&definitions)?;
    fs::set_permissions(&definitions, fs::Permissions::from_mode(0o755))?;
    fs::create_dir(&runtime)?;
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))?;
    for name in ["db", "web", "retired"] {
        fs::write(
            definitions.join(format!("{name}.yml")),
            "version: 2\ncommand: [/bin/sleep, '30']\n",
        )?;
    }
    fs::write(
        definitions.join("api.yml"),
        "version: 2\ncommand: [/bin/sleep, '30']\nrequires: [db]\n",
    )?;

    // Both brokers are created before any Tokio runtime exists, matching the
    // daemon-startup ordering the reconciler requires.
    let first = start_process_broker()?;
    let second = start_process_broker()?;
    let third = start_process_broker()?;
    let fourth = start_process_broker()?;
    let tokio = Builder::new_current_thread().enable_all().build()?;
    let result = tokio.block_on(run_dependency_contract(
        &definitions,
        &runtime,
        [first, second, third, fourth],
    ));
    let cleanup = tokio.block_on(async {
        for name in ["api", "db", "web", "retired", "extra"] {
            halt_if_present(&runtime, name).await?;
        }
        Ok::<(), Box<dyn Error>>(())
    });
    drop(tokio);
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
    }
}

async fn run_dependency_contract(
    definitions: &Path,
    runtime: &Path,
    endpoints: [ProcessBrokerEndpoint; 4],
) -> Result<(), Box<dyn Error>> {
    let [first, second, third, fourth] = endpoints;
    let action = dependency_action(definitions, runtime)?;
    reconcile::execute_with_endpoint(&action, Some(first)).await?;
    for name in ["api", "db", "web", "retired"] {
        wait_for_ready_command(runtime, name, "30").await?;
    }

    // Disable the requirement and delete an unrelated service in the same pass.
    // The deletion is the half that a fail-fast plan stranded, because the old
    // graph computation ran before stops drained.
    fs::write(
        definitions.join("db.yml"),
        "version: 2\ncommand: [/bin/sleep, '30']\nenabled: false\n",
    )?;
    fs::remove_file(definitions.join("retired.yml"))?;

    let action = dependency_action(definitions, runtime)?;
    let failures = match reconcile::execute_with_endpoint(&action, Some(second)).await {
        Err(ActionError::Partial(failures)) => failures,
        Ok(()) => return Err("unresolvable dependency was not reported".into()),
        Err(error) => {
            return Err(format!("unresolvable dependency aborted the pass: {error}").into());
        }
    };
    let reported: Vec<String> = failures
        .iter()
        .map(ToString::to_string)
        .filter(|failure| failure.contains("api"))
        .collect();
    if reported.len() != 1 {
        return Err(format!("expected exactly one isolated `api` failure: {failures:?}").into());
    }
    if !reported
        .iter()
        .any(|failure| failure.contains("unavailable service `db`"))
    {
        return Err(format!("`api` failure did not name its requirement: {reported:?}").into());
    }

    // The unrelated service keeps running and both stops still drained: the
    // disabled requirement is stopped in place, and the deleted service is
    // stopped and its deletion confirmed rather than stranded behind the
    // broken graph.
    wait_for_ready_command(runtime, "web", "30").await?;
    wait_for_service_state(runtime, "db", ServiceState::Down).await?;
    wait_for_first_deletion_confirmation(runtime, "retired").await?;

    // A confirmed deletion is completed by the next authoritative pass, which
    // must still report only the isolated `api` failure.
    let action = dependency_action(definitions, runtime)?;
    match reconcile::execute_with_endpoint(&action, Some(third)).await {
        Err(ActionError::Partial(failures))
            if failures
                .iter()
                .all(|failure| failure.to_string().contains("api")) => {}
        Ok(()) => return Err("unresolvable dependency stopped being reported".into()),
        Err(error) => {
            return Err(format!("confirming pass reported unrelated failures: {error}").into());
        }
    }
    wait_for_service_absence(runtime, "retired").await?;
    wait_for_ready_command(runtime, "web", "30").await?;

    prove_invalid_definition_fails_after_valid_work(definitions, runtime, fourth).await
}

/// Regression: a real one-shot pass that skipped an invalid definition
/// reported success. It must still apply every valid change first and then
/// fail with the invalid-definition result rather than a service failure.
async fn prove_invalid_definition_fails_after_valid_work(
    definitions: &Path,
    runtime: &Path,
    endpoint: ProcessBrokerEndpoint,
) -> Result<(), Box<dyn Error>> {
    // Restoring the requirement leaves no service failure, so the invalid
    // definition alone decides the result.
    fs::write(
        definitions.join("db.yml"),
        "version: 2\ncommand: [/bin/sleep, '30']\n",
    )?;
    fs::write(
        definitions.join("extra.yml"),
        "version: 2\ncommand: [/bin/sleep, '30']\n",
    )?;
    fs::write(definitions.join("broken.yml"), "version: 2\ncommand: []\n")?;

    let action = dependency_action(definitions, runtime)?;
    match reconcile::execute_with_endpoint(&action, Some(endpoint)).await {
        Err(error @ ActionError::InvalidDefinitions(1)) => {
            if error.exit_class() != ExitClass::Configuration {
                return Err(format!("invalid definition mapped to the wrong exit: {error}").into());
            }
        }
        Ok(()) => return Err("a skipped invalid definition was reported as success".into()),
        Err(error) => {
            return Err(format!("invalid definition was not the pass result: {error}").into());
        }
    }
    for name in ["api", "db", "web", "extra"] {
        wait_for_ready_command(runtime, name, "30").await?;
    }
    Ok(())
}

fn dependency_action(
    definitions: &Path,
    runtime: &Path,
) -> Result<DirectoryAction, Box<dyn Error>> {
    Ok(DirectoryAction {
        directory: definitions.to_path_buf(),
        runtime_directory: runtime.to_path_buf(),
        scan_interval_seconds: 1,
        supervisor_binary: std::env::current_exe()?,
        launch_concurrency: LaunchConcurrency::new(8)?,
        once: true,
        dry_run: false,
    })
}
