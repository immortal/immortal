//! One fresh process proving the replaceable file adapter is broker-supervised.

#[path = "support/marker.rs"]
mod marker;

use std::{
    error::Error,
    ffi::OsString,
    fs,
    io::{self, Read, Write},
};

use immortal_core::{
    config::{RestartPolicy, ServiceConfig},
    executor::run_foreground,
    supervisor::{ChildResult, SupervisorState},
};

use marker::Marker;

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let mut destination = None;
    let mut passthrough = false;
    for argument in arguments {
        passthrough |= argument == "--passthrough";
        destination = Some(argument);
    }
    if let Some(destination) = destination {
        return run_test_adapter(destination, passthrough);
    }

    let output = Marker::new("file-adapter-output");
    let downstream = Marker::new("file-adapter-downstream");
    let downstream_path = downstream.path_string()?;
    let mut config = ServiceConfig::for_command(vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "printf 'file-adapter-output\\n'".to_owned(),
    ])?;
    config.logging.combine_stderr = true;
    config.logging.file_adapter = Some(std::env::current_exe()?);
    config.logging.stdout.file.file = Some(output.path_string()?.into());
    config.logging.stdout.logger = Some(vec![
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

fn run_test_adapter(destination: OsString, passthrough: bool) -> Result<(), Box<dyn Error>> {
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
