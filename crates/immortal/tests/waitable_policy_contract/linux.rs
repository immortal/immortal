//! Linux exec-inheritance, late exit, and fail-closed cleanup checks.

#[path = "../../../immortal-core/tests/support/contract_process.rs"]
mod contract_process;

use std::{
    env,
    error::Error,
    fs, io,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use immortal_core::process::{
    ChildEvent, ProcessId, ProcessSignal, acquire_subreaper, reap_any_event, signal_child,
};

use self::contract_process::{Scratch, run as command};

const BOUND: Duration = Duration::from_secs(15);

/// Compile private shims and exercise the actual immortal executable.
pub(super) fn run() -> Result<(), Box<dyn Error>> {
    acquire_subreaper()?;
    let scratch = Scratch::new("waitable-policy")?;
    let _orphans = Orphans;
    let exec = scratch.0.join("policy_exec");
    let source = scratch.0.join("policy_exec.c");
    fs::write(&source, include_str!("policy_exec.c"))?;
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
    fs::write(&source, include_str!("schedule.c"))?;
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
    let config = scratch.0.join("service.yml");
    fs::write(
        &config,
        "---\nversion: 2\ncommand: [/bin/true]\nrestart:\n  policy: never\n  exit_when_done: true\n",
    )?;
    for foreground in [true, false] {
        run_cli(&scratch.0, &exec, &config, None, foreground, 0)?;
    }
    run_cli(
        &scratch.0,
        &exec,
        &config,
        Some((&library, "deadline")),
        true,
        70,
    )?;
    // Mutation of policy after the last idle wait is a contract violation; the
    // cleanup recheck must still refuse delivery rather than trust a stale PID.
    run_cli(
        &scratch.0,
        &exec,
        &config,
        Some((&library, "changed")),
        true,
        71,
    )?;
    run_cli(
        &scratch.0,
        &exec,
        &config,
        Some((&library, "stolen")),
        true,
        71,
    )?;
    run_cli(
        &scratch.0,
        &exec,
        &config,
        Some((&library, "reset-error")),
        true,
        71,
    )?;
    Ok(())
}

/// Every child is reaped before checking assertions, including on a red run.
fn run_cli(
    directory: &Path,
    exec: &Path,
    config: &Path,
    preload: Option<(&Path, &str)>,
    foreground: bool,
    expected: i32,
) -> Result<(), Box<dyn Error>> {
    let mode = preload.map_or(
        if foreground { "foreground" } else { "daemon" },
        |(_, mode)| mode,
    );
    if env::var("IMMORTAL_POLICY_CASE").is_ok_and(|selected| selected != mode) {
        return Ok(());
    }
    let log = directory.join(format!("{mode}.log"));
    let mut cli = Command::new(exec);
    cli.arg(env!("CARGO_BIN_EXE_immortal"))
        .args(["--config"])
        .arg(config)
        .arg("--control-dir")
        .arg(directory.join(mode))
        .env("RUST_LOG", "warn")
        .stderr(Stdio::from(fs::File::create(&log)?));
    if foreground {
        cli.arg("--foreground");
    }
    if let Some((library, mode)) = preload {
        cli.env("LD_PRELOAD", library)
            .env("IMMORTAL_POLICY_SCHEDULE", mode);
    }
    let status = command(&mut cli, BOUND);
    let adopted = Orphans::drain()?;
    let output = fs::read_to_string(&log)?;
    eprintln!("policy case {mode}: {output}");
    assert_eq!(status?.code(), Some(expected), "{mode}: {output}");
    if !foreground {
        assert!(!adopted.is_empty(), "detached supervisor was not collected");
        assert!(
            adopted
                .iter()
                .all(|event| matches!(event, ChildEvent::Exited { code: 0, .. })),
            "detached supervision failed: {adopted:?}"
        );
    }
    assert!(!output.contains("STALE"), "{mode}: {output}");
    if mode == "reset-error" {
        assert!(
            output.contains("Operation not permitted"),
            "reset error was not propagated: {output}"
        );
        assert!(
            !output.contains("UNEXPECTED FORK"),
            "reset failure created children: {output}"
        );
    }
    if mode != "reset-error" {
        assert_eq!(
            output.matches("restored inherited SIGCHLD").count(),
            1,
            "{mode}: {output}"
        );
    }
    if mode == "deadline" {
        assert!(
            output.contains("PINNED"),
            "schedule missed deadline: {output}"
        );
    }
    if mode == "changed" || mode == "stolen" {
        assert!(
            output.contains("INJECTED"),
            "schedule did not run: {output}"
        );
        assert!(
            !output.contains("DELIVERY"),
            "cleanup signalled after ownership loss: {output}"
        );
    }
    Ok(())
}

/// All descendants are finite fixtures; drain only after the invoker is collected.
struct Orphans;
impl Orphans {
    /// Retain terminal outcomes from this finite fixture so daemon success means
    /// completed supervision, not merely the original invoker's ready handshake.
    fn drain() -> io::Result<Vec<ChildEvent>> {
        let mut events = Vec::new();
        let deadline = Instant::now() + BOUND;
        loop {
            match reap_any_event() {
                Ok(Some(event)) => {
                    if event.is_terminal() {
                        events.push(event);
                    }
                }
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(error) if error.raw_os_error() == Some(nix::errno::Errno::ECHILD as i32) => {
                    return Ok(events);
                }
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "supervisor descendants remained",
                ));
            }
        }
    }
}
impl Drop for Orphans {
    fn drop(&mut self) {
        // Linux discovery is used only to find candidate direct children;
        // checked delivery, not procfs membership, decides signal authority.
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
