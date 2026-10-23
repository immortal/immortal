//! Exec-inheritance and pre-fork failure contracts for the shared broker entry.

#[path = "../../../immortal-core/tests/support/contract_process.rs"]
mod contract_process;

use std::{
    env,
    error::Error,
    fs, io,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use immortal_core::{
    config::parse_file,
    executor::run_daemon,
    process::{
        ChildEvent, ProcessId, ProcessSignal, acquire_subreaper, reap_any_event, signal_child,
    },
};

use self::contract_process::{Scratch, run as command};

const BOUND: Duration = Duration::from_secs(20);

pub(super) fn run() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = env::args().collect();
    if arguments.get(1).map(String::as_str) == Some("--config") {
        let config = arguments.get(2).ok_or("missing launcher config")?;
        let control = arguments
            .get(4)
            .ok_or("missing launcher control directory")?;
        run_daemon(&parse_file(Path::new(config))?, Some(Path::new(control)))?;
        return Ok(());
    }
    acquire_subreaper()?;
    let scratch = Scratch::new("dir-policy")?;
    let _orphans = Orphans;
    run_scenarios(&scratch)
}

fn run_scenarios(scratch: &Scratch) -> Result<(), Box<dyn Error>> {
    let exec = scratch.0.join("policy_exec");
    let source = scratch.0.join("policy_exec.c");
    fs::write(
        &source,
        include_str!("../../../immortal/tests/waitable_policy_contract/policy_exec.c"),
    )?;
    assert!(
        command(
            Command::new("cc")
                .args(["-Wall", "-Werror"])
                .arg(&source)
                .arg("-o")
                .arg(&exec),
            BOUND
        )?
        .success()
    );
    let library = scratch.0.join("schedule.so");
    let source = scratch.0.join("schedule.c");
    fs::write(&source, include_str!("reset_error.c"))?;
    assert!(
        command(
            Command::new("cc")
                .args(["-shared", "-fPIC", "-Wall", "-Werror"])
                .arg(&source)
                .args(["-ldl", "-o"])
                .arg(&library),
            BOUND
        )?
        .success()
    );
    let definitions = scratch.0.join("definitions");
    fs::create_dir(&definitions)?;
    fs::set_permissions(&definitions, fs::Permissions::from_mode(0o755))?;
    let marker = scratch.0.join("ran");
    fs::write(
        definitions.join("finite.yml"),
        format!(
            "---\nversion: 2\ncommand: [/bin/sh, -c, 'printf ran > {}; sleep 2']\nrestart:\n  policy: never\n  exit_when_done: true\n",
            marker.display()
        ),
    )?;
    let mut failed = false;
    for mode in ["reconcile", "reset-error"] {
        let runtime = scratch.0.join(mode);
        fs::create_dir(&runtime)?;
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))?;
        let log = scratch.0.join(format!("{mode}.log"));
        let mut cli = Command::new(&exec);
        cli.arg(env!("CARGO_BIN_EXE_immortaldir"))
            .arg("--once")
            .arg("--runtime-dir")
            .arg(&runtime)
            .arg("--supervisor-binary")
            .arg(env::current_exe()?)
            .arg(&definitions)
            .env("RUST_LOG", "warn")
            .stderr(Stdio::from(fs::File::create(&log)?));
        if mode == "reset-error" {
            if marker.exists() {
                fs::remove_file(&marker)?;
            }
            cli.env("LD_PRELOAD", &library);
        }
        let status = command(&mut cli, BOUND)?;
        let events = Orphans::drain()?;
        let output = fs::read_to_string(&log)?;
        eprintln!("immortaldir {mode}: {status}; adopted={events:?}; {output}");
        let correct = if mode == "reconcile" {
            status.success()
                && marker.exists()
                && output.matches("restored inherited SIGCHLD").count() == 1
                && events
                    .iter()
                    .all(|event| matches!(event, ChildEvent::Exited { code: 0, .. }))
        } else {
            status.code() == Some(71)
                && output.contains("unable to start process broker")
                && output.contains("Operation not permitted")
                && !output.contains("UNEXPECTED FORK")
                && !output.contains("restored inherited SIGCHLD")
                && !marker.exists()
                && events.is_empty()
        };
        failed |= !correct;
    }
    if failed {
        Err("directory waitable-policy regression".into())
    } else {
        Ok(())
    }
}

struct Orphans;
impl Orphans {
    fn drain() -> io::Result<Vec<ChildEvent>> {
        let mut events = Vec::new();
        let deadline = Instant::now() + BOUND;
        loop {
            match reap_any_event() {
                Ok(Some(event)) if event.is_terminal() => events.push(event),
                Ok(_) => thread::sleep(Duration::from_millis(5)),
                // This module is Linux-only: ECHILD is 10 on this ABI.
                Err(error) if error.raw_os_error() == Some(10) => return Ok(events),
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "directory descendants remained",
                ));
            }
        }
    }
}
impl Drop for Orphans {
    fn drop(&mut self) {
        // Discovery supplies candidates, never signal authority. Cleanup checks
        // exact child ownership and policy; collection precedes scratch removal.
        if let Ok(raw) =
            fs::read_to_string(format!("/proc/self/task/{}/children", std::process::id()))
        {
            for child in raw
                .split_whitespace()
                .filter_map(|raw| raw.parse::<i32>().ok())
                .filter_map(ProcessId::new)
            {
                let _ = signal_child(child, ProcessSignal::Continue);
                let _ = signal_child(child, ProcessSignal::Kill);
            }
        }
        let _ = Self::drain();
    }
}
