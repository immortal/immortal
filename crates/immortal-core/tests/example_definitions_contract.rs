//! Black-box contract proving the shipped example definitions stay valid.
//!
//! `examples/services/*.yml` is what README and INSTALL tell an operator to run
//! first, and the documented commands (`immortal --check-config` and
//! `immortaldir --once --dry-run`) parse them for real. Only yamllint used to
//! read these files, so a schema change could leave the first thing a new
//! operator runs broken while every test still passed. This asserts they parse
//! through the same public entry point the supervisor uses.
//!
//! `examples/conditions/*.yml` are standalone start-condition examples, such as
//! waiting for a configured network (issue 68); each must also keep its
//! `start_condition`.

use std::{error::Error, fs, path::PathBuf};

use immortal_core::config::{self, ConfigError, ServiceConfig};

fn main() -> Result<(), Box<dyn Error>> {
    let examples = examples_directory()?;
    parse_examples(&examples.join("services"))?;
    for (path, config) in parse_examples(&examples.join("conditions"))? {
        if config.start_condition.is_none() {
            return Err(format!("{} must define a start_condition", path.display()).into());
        }
    }

    prove_a_broken_example_is_rejected(&examples.join("services"))
}

/// Parse every `*.yml` definition in `directory`, which must contain at least one.
fn parse_examples(
    directory: &std::path::Path,
) -> Result<Vec<(PathBuf, ServiceConfig)>, Box<dyn Error>> {
    let mut names: Vec<PathBuf> = fs::read_dir(directory)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "yml"))
        .collect();
    names.sort();

    if names.is_empty() {
        return Err(format!("no example definitions found in {}", directory.display()).into());
    }

    let mut parsed = Vec::with_capacity(names.len());
    for path in names {
        let config = config::parse_file(&path)
            .map_err(|error| format!("{} must parse: {error}", path.display()))?;
        if config.command.is_empty() {
            return Err(format!("{} must define a command", path.display()).into());
        }
        parsed.push((path, config));
    }
    Ok(parsed)
}

/// The contract only means something if the same entry point rejects bad input.
fn prove_a_broken_example_is_rejected(directory: &std::path::Path) -> Result<(), Box<dyn Error>> {
    let missing = directory.join("this-file-does-not-exist.yml");
    match config::parse_file(&missing) {
        Err(ConfigError::Io(_)) => {}
        Err(error) => return Err(format!("expected a read failure, got {error}").into()),
        Ok(_) => return Err("a missing definition must not parse".into()),
    }
    Ok(())
}

fn examples_directory() -> Result<PathBuf, Box<dyn Error>> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest
        .parent()
        .and_then(|crates| crates.parent())
        .ok_or("workspace root is not reachable from the manifest directory")?;
    Ok(root.join("examples"))
}
