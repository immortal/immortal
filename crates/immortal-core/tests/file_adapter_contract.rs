//! One fresh process proving the replaceable file adapter is broker-supervised.

#[path = "support/marker.rs"]
mod marker;

use std::{
    error::Error,
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{self, Read, Write},
};

use immortal_core::{
    config::{FileLogConfig, FileLogRoutes, RestartPolicy, ServiceConfig},
    executor::{SupervisionOutcome, run_foreground},
    supervisor::{ChildResult, SupervisorState},
};

use marker::Marker;

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let Some(first) = arguments.next() else {
        prove_combined_adapter_chain()?;
        prove_split_files_fan_into_one_logger()?;
        return prove_selected_stderr_and_combined_logger();
    };
    if first == OsStr::new("--test-logger") {
        return run_test_logger(&mut arguments);
    }
    run_test_adapter(std::iter::once(first).chain(arguments))
}

fn prove_combined_adapter_chain() -> Result<(), Box<dyn Error>> {
    let output = Marker::new("file-adapter-output");
    let downstream = Marker::new("file-adapter-downstream");
    let downstream_path = downstream.path_string()?;
    let mut config = ServiceConfig::for_command(vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "printf 'file-adapter-output\\n'".to_owned(),
    ])?;
    config.logging.file_adapter = Some(std::env::current_exe()?);
    config.logging.files = Some(FileLogRoutes::Combined(FileLogConfig {
        file: output.path_string()?.into(),
        max_age_seconds: None,
        keep: None,
        max_bytes: None,
        timestamp: false,
    }));
    config.logging.logger = Some(vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        format!("cat > '{downstream_path}'"),
    ]);
    config.restart.policy = RestartPolicy::Never;
    config.restart.exit_when_done = true;

    let outcome = run_foreground(&config)?;
    if outcome.state != SupervisorState::Exited
        || outcome.last_result != Some(ChildResult::Exited(0))
        || outcome.last_start_failed
        || outcome.last_readiness_failed
        || outcome.starts != 1
    {
        return Err(io::Error::other(format!(
            "unexpected file-adapter supervision outcome: {outcome:?}"
        ))
        .into());
    }
    if !output.exists() || !downstream.exists() {
        return Err(io::Error::other("file-adapter pipeline output is missing").into());
    }
    let actual = fs::read_to_string(output.path_string()?)?;
    let downstream_actual = fs::read_to_string(downstream_path)?;
    if actual != "file-adapter-output\n" || downstream_actual != actual {
        return Err(io::Error::other(format!(
            "unexpected adapter pipeline output: file={actual:?}, downstream={downstream_actual:?}"
        ))
        .into());
    }
    Ok(())
}

fn prove_split_files_fan_into_one_logger() -> Result<(), Box<dyn Error>> {
    let stdout = Marker::new("file-adapter-stdout");
    let stderr = Marker::new("file-adapter-stderr");
    let downstream = Marker::new("file-adapter-split-downstream");
    let logger_started = Marker::new("file-adapter-logger-started");
    let executable = std::env::current_exe()?;
    let executable_text = executable
        .to_str()
        .ok_or_else(|| io::Error::other("contract executable path is not UTF-8"))?
        .to_owned();
    let mut config = ServiceConfig::for_command(vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "printf 'stdout-record\\n'; printf 'stderr-record\\n' >&2".to_owned(),
    ])?;
    config.logging.file_adapter = Some(executable);
    config.logging.files = Some(FileLogRoutes::Selected {
        stdout: Some(file_config(stdout.path_string()?)),
        stderr: Some(file_config(stderr.path_string()?)),
    });
    config.logging.logger = Some(vec![
        executable_text,
        "--test-logger".to_owned(),
        downstream.path_string()?,
        logger_started.path_string()?,
    ]);
    config.logging.restart.max_retries = Some(0);
    config.restart.policy = RestartPolicy::Never;
    config.restart.exit_when_done = true;

    let outcome = run_foreground(&config)?;
    require_success(&outcome, "split file fan-in")?;
    if !stdout.exists() || !stderr.exists() || !downstream.exists() || !logger_started.exists() {
        return Err(io::Error::other("split fan-in output is missing").into());
    }
    let stdout_actual = fs::read_to_string(stdout.path_string()?)?;
    let stderr_actual = fs::read_to_string(stderr.path_string()?)?;
    let downstream_actual = fs::read_to_string(downstream.path_string()?)?;
    let mut downstream_lines: Vec<&str> = downstream_actual.lines().collect();
    downstream_lines.sort_unstable();
    if stdout_actual != "stdout-record\n"
        || stderr_actual != "stderr-record\n"
        || downstream_lines != ["stderr-record", "stdout-record"]
    {
        return Err(io::Error::other(format!(
            "unexpected split fan-in output: stdout={stdout_actual:?}, stderr={stderr_actual:?}, \
             downstream={downstream_actual:?}"
        ))
        .into());
    }
    Ok(())
}

fn prove_selected_stderr_and_combined_logger() -> Result<(), Box<dyn Error>> {
    let stderr = Marker::new("file-adapter-selected-stderr");
    let downstream = Marker::new("file-adapter-selected-downstream");
    let logger_started = Marker::new("file-adapter-selected-logger-started");
    let executable = std::env::current_exe()?;
    let executable_text = executable
        .to_str()
        .ok_or_else(|| io::Error::other("contract executable path is not UTF-8"))?
        .to_owned();
    let mut config = ServiceConfig::for_command(vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "printf 'selected-stdout\\n'; printf 'selected-stderr\\n' >&2".to_owned(),
    ])?;
    config.logging.file_adapter = Some(executable);
    config.logging.files = Some(FileLogRoutes::Selected {
        stdout: None,
        stderr: Some(file_config(stderr.path_string()?)),
    });
    config.logging.logger = Some(vec![
        executable_text,
        "--test-logger".to_owned(),
        downstream.path_string()?,
        logger_started.path_string()?,
    ]);
    config.logging.restart.max_retries = Some(0);
    config.restart.policy = RestartPolicy::Never;
    config.restart.exit_when_done = true;

    let outcome = run_foreground(&config)?;
    require_success(&outcome, "selected stderr and combined logger")?;
    if !stderr.exists() || !downstream.exists() || !logger_started.exists() {
        return Err(io::Error::other("selected route output is missing").into());
    }
    let stderr_actual = fs::read_to_string(stderr.path_string()?)?;
    let downstream_actual = fs::read_to_string(downstream.path_string()?)?;
    let mut downstream_lines: Vec<&str> = downstream_actual.lines().collect();
    downstream_lines.sort_unstable();
    if stderr_actual != "selected-stderr\n"
        || downstream_lines != ["selected-stderr", "selected-stdout"]
    {
        return Err(io::Error::other(format!(
            "unexpected selected routing: stderr={stderr_actual:?}, \
             downstream={downstream_actual:?}"
        ))
        .into());
    }
    Ok(())
}

fn file_config(path: String) -> FileLogConfig {
    FileLogConfig {
        file: path.into(),
        max_age_seconds: None,
        keep: None,
        max_bytes: None,
        timestamp: false,
    }
}

fn require_success(outcome: &SupervisionOutcome, context: &str) -> Result<(), Box<dyn Error>> {
    if outcome.state != SupervisorState::Exited
        || outcome.last_result != Some(ChildResult::Exited(0))
        || outcome.last_start_failed
        || outcome.last_readiness_failed
        || outcome.starts != 1
    {
        return Err(io::Error::other(format!(
            "unexpected {context} supervision outcome: {outcome:?}"
        ))
        .into());
    }
    Ok(())
}

fn run_test_logger(arguments: &mut impl Iterator<Item = OsString>) -> Result<(), Box<dyn Error>> {
    let output = arguments
        .next()
        .ok_or_else(|| io::Error::other("test logger output path is missing"))?;
    let started = arguments
        .next()
        .ok_or_else(|| io::Error::other("test logger start marker is missing"))?;
    if arguments.next().is_some() {
        return Err(io::Error::other("test logger received excess arguments").into());
    }
    let _started = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(started)?;
    let mut input = io::stdin().lock();
    let mut output = fs::File::create(output)?;
    let _bytes = io::copy(&mut input, &mut output)?;
    Ok(())
}

fn run_test_adapter(arguments: impl IntoIterator<Item = OsString>) -> Result<(), Box<dyn Error>> {
    let mut destination = None;
    let mut passthrough = false;
    for argument in arguments {
        passthrough |= argument == "--passthrough";
        destination = Some(argument);
    }
    let destination =
        destination.ok_or_else(|| io::Error::other("test adapter destination is missing"))?;
    let mut input = io::stdin().lock();
    let mut output = fs::File::create(destination)?;
    if !passthrough {
        let _bytes = io::copy(&mut input, &mut output)?;
        return Ok(());
    }
    let mut downstream = io::stdout().lock();
    let mut buffer = [0; 8 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let bytes = buffer
            .get(..read)
            .ok_or_else(|| io::Error::other("adapter read exceeded its buffer"))?;
        output.write_all(bytes)?;
        downstream.write_all(bytes)?;
    }
    Ok(())
}
