//! Best-effort diagnostics on standard error.
//!
//! The supervisor and its process broker report survivable conditions on
//! standard error: a broker answer that is late, a group guard that was slow to
//! release, an adopted orphan, a missing platform capability. Such a report must
//! never end supervision itself. `eprintln!` panics when the write fails, which
//! happens once a foreground supervisor's standard-error reader has gone away
//! (`EPIPE`) or the descriptor was closed, so a supervisor waiting patiently for
//! a stalled broker would die on its first report. A failed write is ignored
//! here instead: the condition being reported is handled either way.

use std::{
    fmt,
    io::{self, Write},
};

/// Write one diagnostic line to standard error, ignoring a failed write.
pub(crate) fn report(message: fmt::Arguments<'_>) {
    report_to(&mut io::stderr().lock(), message);
}

/// Write one diagnostic line to `writer`, ignoring a failed write.
fn report_to<W: Write>(writer: &mut W, message: fmt::Arguments<'_>) {
    let _ = writeln!(writer, "{message}");
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};

    use super::report_to;

    /// A writer whose reader has gone away, like a closed standard-error pipe.
    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
    }

    /// Regression: diagnostics written with `eprintln!` panicked once standard
    /// error was a pipe whose reader had gone, ending supervision over a report.
    #[test]
    fn a_failed_diagnostic_write_is_ignored() {
        report_to(&mut ClosedPipe, format_args!("still waiting"));
    }
}
