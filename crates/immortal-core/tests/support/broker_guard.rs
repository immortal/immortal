use std::{
    io, thread,
    time::{Duration, Instant},
};

use immortal_core::process::{
    ChildEvent, ProcessId, ProcessSignal, SignalTarget, reap_any_event, signal,
};

pub struct BrokerGuard {
    poll_interval: Duration,
    process: ProcessId,
    reaped: bool,
    timeout: Duration,
}

impl BrokerGuard {
    pub const fn new(process: ProcessId, timeout: Duration, poll_interval: Duration) -> Self {
        Self {
            poll_interval,
            process,
            reaped: false,
            timeout,
        }
    }

    pub fn wait(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + self.timeout;
        loop {
            match reap_any_event() {
                Ok(Some(event)) if event.pid() == self.process && event.is_terminal() => {
                    self.reaped = true;
                    return match event {
                        ChildEvent::Exited { code: 0, .. } => Ok(()),
                        _ => Err(io::Error::other(format!(
                            "broker terminated unsuccessfully: {event:?}"
                        ))),
                    };
                }
                Ok(Some(event)) => {
                    return Err(io::Error::other(format!(
                        "supervisor reaped unexpected child event {event:?}"
                    )));
                }
                Ok(None) => {}
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for process broker",
                ));
            }
            thread::sleep(self.poll_interval);
        }
    }
}

impl Drop for BrokerGuard {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        let _ = signal(SignalTarget::Process(self.process), ProcessSignal::Kill);
        let deadline = Instant::now() + self.timeout;
        while Instant::now() < deadline {
            match reap_any_event() {
                Ok(Some(event)) if event.pid() == self.process && event.is_terminal() => {
                    self.reaped = true;
                    return;
                }
                Ok(Some(_) | None) => thread::sleep(self.poll_interval),
                Err(_) => return,
            }
        }
    }
}
