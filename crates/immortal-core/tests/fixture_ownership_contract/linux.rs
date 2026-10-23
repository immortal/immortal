//! Real-child ownership schedules, with interception only at signal delivery.

#[path = "../support/broker_guard.rs"]
mod broker_guard;
#[path = "../support/contract_process.rs"]
mod contract_process;
#[path = "../support/group_guard.rs"]
mod group_guard;

use std::{
    env,
    error::Error,
    fs, io,
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use immortal_core::process::{
    ChildEvent, ProcessGroupId, ProcessId, ProcessSignal, reap_any_event, reap_event, signal_child,
};

use self::{
    broker_guard::BrokerGuard,
    contract_process::{Scratch, run as command},
    group_guard::GroupGuard,
};

const BOUND: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(5);

/// Interpose in a fresh executable so process-wide state never races libtest.
pub(super) fn run() -> Result<(), Box<dyn Error>> {
    match env::var("IMMORTAL_GUARD_MODE").as_deref() {
        Ok("unwind") => return unwind(),
        Ok("reused") => return reused(),
        Ok("cached") => return cached_terminal(),
        Ok("release-parent") => return release_parent(),
        _ => {}
    }
    let scratch = Scratch::new("fixture-ownership")?;
    let source = scratch.0.join("check.c");
    let library = scratch.0.join("check.so");
    fs::write(&source, include_str!("signals.c"))?;
    assert!(
        command(
            Command::new("cc")
                .args(["-shared", "-fPIC", "-Wall", "-Werror"])
                .arg(&source)
                .arg("-o")
                .arg(&library),
            BOUND
        )?
        .success()
    );
    for mode in ["unwind", "reused", "cached"] {
        let log = scratch.0.join(mode);
        let status = command(
            Command::new(env::current_exe()?)
                .env("IMMORTAL_GUARD_MODE", mode)
                .env("LD_PRELOAD", &library)
                .stderr(Stdio::from(fs::File::create(&log)?)),
            BOUND,
        )?;
        let output = fs::read_to_string(log)?;
        eprintln!("fixture case {mode}: {status}: {output}");
        assert!(status.success(), "{mode}: {status}: {output}");
        assert!(!output.contains("UNSAFE"), "{mode}: {output}");
    }
    release_handshake(&scratch.0)?;
    Ok(())
}

/// The exact early-return drop order from the lifetime-release contract.
fn unwind() -> Result<(), Box<dyn Error>> {
    let outcome = (|| -> Result<(), Box<dyn Error>> {
        let broker = Command::new("/bin/true").process_group(0).spawn()?;
        let broker_id = ProcessId::try_from(i32::try_from(broker.id())?)?;
        let _broker_guard = BrokerGuard::new(broker_id, BOUND, POLL);
        let member = Command::new("/bin/sleep")
            .arg("5")
            .process_group(0)
            .spawn()?;
        let member_group = ProcessGroupId::try_from(i32::try_from(member.id())?)?;
        let _member_guard = GroupGuard::new(member_group, BOUND, POLL);
        let deadline = Instant::now() + BOUND;
        loop {
            let status = fs::read_to_string(format!("/proc/{broker_id}/status"))?;
            if status
                .lines()
                .any(|line| line.starts_with("State:") && line.contains('Z'))
            {
                break;
            }
            if Instant::now() >= deadline {
                return Err("broker did not exit".into());
            }
            thread::sleep(POLL);
        }
        Err("intentional early return".into())
    })();
    assert_eq!(
        outcome
            .err()
            .ok_or("missing intentional failure")?
            .to_string(),
        "intentional early return"
    );
    assert!(matches!(reap_any_event(), Err(error) if error.raw_os_error() == Some(libc::ECHILD)));
    Ok(())
}

/// Auxiliary STOP must report a consumed broker exit to its one fixture owner.
fn cached_terminal() -> Result<(), Box<dyn Error>> {
    let child = Command::new("/bin/true").spawn()?;
    let process = ProcessId::try_from(i32::try_from(child.id())?)?;
    let mut broker = BrokerGuard::new(process, BOUND, POLL);
    let deadline = Instant::now() + BOUND;
    loop {
        let status = fs::read_to_string(format!("/proc/{process}/status"))?;
        if status
            .lines()
            .any(|line| line.starts_with("State:") && line.contains('Z'))
        {
            break;
        }
        if Instant::now() >= deadline {
            return Err("broker did not exit".into());
        }
        thread::sleep(POLL);
    }
    assert!(
        matches!(broker.signal(ProcessSignal::Stop)?, Some(ChildEvent::Exited { pid, code: 0 }) if pid == process)
    );
    broker.wait()?;
    assert!(broker.signal(ProcessSignal::Kill)?.is_some());
    assert!(
        matches!(reap_event(process), Err(error) if error.raw_os_error() == Some(libc::ECHILD))
    );
    Ok(())
}

/// A successful null probe modeling reuse must never authorize a group signal.
fn reused() -> Result<(), Box<dyn Error>> {
    let mut child = Command::new("/bin/true").process_group(0).spawn()?;
    let group = ProcessGroupId::try_from(i32::try_from(child.id())?)?;
    let mut guard = GroupGuard::new(group, BOUND, POLL);
    child.wait()?;
    let _ = guard.kill_and_wait();
    guard.disarm();
    Ok(())
}

/// A private release handshake cleans an indirect member without signalling it.
fn release_handshake(directory: &Path) -> Result<(), Box<dyn Error>> {
    let mut parent = Command::new(env::current_exe()?)
        .env("IMMORTAL_GUARD_MODE", "release-parent")
        .env("FIXTURE_DIRECTORY", directory)
        .spawn()?;
    let pid = ProcessId::try_from(i32::try_from(parent.id())?)?;
    let mut cleanup = BrokerGuard::new(pid, BOUND, POLL);
    let deadline = Instant::now() + BOUND;
    let group = loop {
        if let Ok(raw) = fs::read_to_string(directory.join("ready")) {
            break ProcessGroupId::try_from(raw.parse::<i32>()?)?;
        }
        if Instant::now() >= deadline {
            return Err("member did not start".into());
        }
        thread::sleep(POLL);
    };
    GroupGuard::released(group, vec![directory.join("release")], BOUND, POLL).kill_and_wait()?;
    cleanup.wait()?;
    // std::Child is not another collector: the exact owner above already reaped it.
    assert!(parent.try_wait().is_err());
    assert!(matches!(reap_event(pid), Err(error) if error.raw_os_error() == Some(libc::ECHILD)));
    assert!(signal_child(pid, ProcessSignal::Kill).is_err());
    Ok(())
}

/// Own and collect the indirect fixture; a finite loop also bounds broken releases.
fn release_parent() -> Result<(), Box<dyn Error>> {
    let directory = env::var("FIXTURE_DIRECTORY")?;
    let mut member = Command::new("/bin/sh")
        .args([
            "-c",
            "i=0; while [ ! -e \"$RELEASE\" ] && [ $i -lt 200 ]; do i=$((i+1)); sleep 0.01; done; [ -e \"$RELEASE\" ]",
        ])
        .env("RELEASE", Path::new(&directory).join("release"))
        .process_group(0)
        .spawn()?;
    fs::write(Path::new(&directory).join("ready"), member.id().to_string())?;
    let result = member.wait()?;
    if !result.success() {
        return Err(io::Error::other("member failed").into());
    }
    Ok(())
}
