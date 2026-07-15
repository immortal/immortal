//! Deterministic stdout and stderr workload for hands-on Immortal testing.
//!
//! The probe emits paired, flushed records on a monotonic cadence. Optional
//! timed exit and status controls make service restarts visible without shell
//! interpretation or application dependencies.

use std::{
    env,
    error::Error,
    ffi::{OsStr, OsString},
    fmt::{self, Display, Formatter},
    io::{self, Write},
    process::ExitCode,
    thread,
    time::{Duration, Instant},
};

const DEFAULT_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_EXIT_CODE: u8 = 1;
const EXIT_IO_ERROR: u8 = 74;
const EXIT_SOFTWARE: u8 = 70;
const EXIT_USAGE: u8 = 64;
const HELP: &str = "\
Usage: immortal-log-probe [--interval DURATION]\n\
                          [--exit-after DURATION [--exit-code CODE]]\n\
\n\
Write one flushed record to stdout and stderr immediately, then at each\n\
interval. With --exit-after, write a final record and return CODE.\n\
\n\
Options:\n\
  --interval DURATION    Output cadence; default: 1s\n\
  --exit-after DURATION Exit after this duration\n\
  --exit-code CODE       Exit status from 0 through 255; default: 1\n\
  -h, --help             Print this help\n\
\n\
Durations are positive whole numbers followed by ms, s, m, or h.\n";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Settings {
    interval: Duration,
    exit_after: Option<Duration>,
    exit_code: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParsedCommand {
    Help,
    Run(Settings),
}

#[derive(Debug, Eq, PartialEq)]
struct UsageError(String);

impl Display for UsageError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for UsageError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordEvent {
    Tick,
    Exit(u8),
}

#[derive(Debug)]
enum ProbeError {
    ClockRange(&'static str),
    Output {
        stream: &'static str,
        source: io::Error,
    },
    SequenceExhausted,
}

impl ProbeError {
    const fn exit_code(&self) -> u8 {
        match self {
            Self::Output { .. } => EXIT_IO_ERROR,
            Self::ClockRange(_) | Self::SequenceExhausted => EXIT_SOFTWARE,
        }
    }
}

impl Display for ProbeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClockRange(name) => {
                write!(formatter, "{name} exceeds the monotonic clock range")
            }
            Self::Output { stream, source } => {
                write!(formatter, "unable to write {stream}: {source}")
            }
            Self::SequenceExhausted => formatter.write_str("record sequence is exhausted"),
        }
    }
}

impl Error for ProbeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Output { source, .. } => Some(source),
            Self::ClockRange(_) | Self::SequenceExhausted => None,
        }
    }
}

fn main() -> ExitCode {
    ExitCode::from(entrypoint())
}

fn entrypoint() -> u8 {
    let command = match parse_arguments(env::args_os().skip(1)) {
        Ok(command) => command,
        Err(error) => {
            write_diagnostic(&error);
            write_diagnostic_text("try `immortal-log-probe --help` for usage");
            return EXIT_USAGE;
        }
    };

    match command {
        ParsedCommand::Help => match write_help() {
            Ok(()) => 0,
            Err(error) => {
                write_diagnostic_text(&format!("unable to write help: {error}"));
                EXIT_IO_ERROR
            }
        },
        ParsedCommand::Run(settings) => {
            let result = {
                let stdout = io::stdout();
                let stderr = io::stderr();
                run_probe(
                    settings,
                    &mut stdout.lock(),
                    &mut stderr.lock(),
                    Instant::now(),
                )
            };
            match result {
                Ok(code) => code,
                Err(error) => {
                    let code = error.exit_code();
                    write_diagnostic(&error);
                    code
                }
            }
        }
    }
}

fn parse_arguments<I, T>(arguments: I) -> Result<ParsedCommand, UsageError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let mut arguments = arguments.into_iter().map(Into::into);
    let mut interval = None;
    let mut exit_after = None;
    let mut exit_code = None;
    let mut help = None;

    while let Some(argument) = arguments.next() {
        let text = argument
            .to_str()
            .ok_or_else(|| UsageError("arguments must be valid UTF-8".to_owned()))?;
        match text {
            "-h" | "--help" => set_once(&mut help, (), "--help")?,
            "--interval" => {
                let value = required_value(&mut arguments, "--interval")?;
                set_once(
                    &mut interval,
                    parse_duration(&value, "--interval")?,
                    "--interval",
                )?;
            }
            "--exit-after" => {
                let value = required_value(&mut arguments, "--exit-after")?;
                set_once(
                    &mut exit_after,
                    parse_duration(&value, "--exit-after")?,
                    "--exit-after",
                )?;
            }
            "--exit-code" => {
                let value = required_value(&mut arguments, "--exit-code")?;
                set_once(&mut exit_code, parse_exit_code(&value)?, "--exit-code")?;
            }
            _ => {
                return Err(UsageError(format!(
                    "unexpected argument `{}`",
                    argument.to_string_lossy()
                )));
            }
        }
    }

    if exit_code.is_some() && exit_after.is_none() {
        return Err(UsageError("--exit-code requires --exit-after".to_owned()));
    }
    if help.is_some() {
        return Ok(ParsedCommand::Help);
    }
    Ok(ParsedCommand::Run(Settings {
        interval: interval.unwrap_or(DEFAULT_INTERVAL),
        exit_after,
        exit_code: exit_code.unwrap_or(DEFAULT_EXIT_CODE),
    }))
}

fn required_value(
    arguments: &mut impl Iterator<Item = OsString>,
    option: &str,
) -> Result<OsString, UsageError> {
    arguments
        .next()
        .ok_or_else(|| UsageError(format!("{option} requires a value")))
}

fn set_once<T>(slot: &mut Option<T>, value: T, option: &str) -> Result<(), UsageError> {
    if slot.replace(value).is_some() {
        Err(UsageError(format!("{option} may be specified only once")))
    } else {
        Ok(())
    }
}

fn parse_duration(value: &OsStr, option: &str) -> Result<Duration, UsageError> {
    let text = value
        .to_str()
        .ok_or_else(|| UsageError(format!("{option} must be valid UTF-8")))?;
    let (digits, multiplier) = [("ms", 1_u64), ("s", 1_000), ("m", 60_000), ("h", 3_600_000)]
        .into_iter()
        .find_map(|(suffix, multiplier)| {
            text.strip_suffix(suffix).map(|digits| (digits, multiplier))
        })
        .ok_or_else(|| {
            UsageError(format!(
                "{option} must be a positive whole number followed by ms, s, m, or h"
            ))
        })?;
    if digits.is_empty() || !digits.as_bytes().iter().all(u8::is_ascii_digit) {
        return Err(UsageError(format!(
            "{option} must be a positive whole number followed by ms, s, m, or h"
        )));
    }
    let quantity = digits
        .parse::<u64>()
        .map_err(|_| UsageError(format!("{option} exceeds the supported duration range")))?;
    if quantity == 0 {
        return Err(UsageError(format!("{option} must be greater than zero")));
    }
    let milliseconds = quantity
        .checked_mul(multiplier)
        .ok_or_else(|| UsageError(format!("{option} exceeds the supported duration range")))?;
    let duration = Duration::from_millis(milliseconds);
    if Instant::now().checked_add(duration).is_none() {
        return Err(UsageError(format!(
            "{option} exceeds the monotonic clock range"
        )));
    }
    Ok(duration)
}

fn parse_exit_code(value: &OsStr) -> Result<u8, UsageError> {
    let text = value
        .to_str()
        .ok_or_else(|| UsageError("--exit-code must be valid UTF-8".to_owned()))?;
    text.parse::<u8>()
        .map_err(|_| UsageError("--exit-code must be an integer from 0 through 255".to_owned()))
}

fn run_probe(
    settings: Settings,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    started: Instant,
) -> Result<u8, ProbeError> {
    let deadline = settings
        .exit_after
        .map(|duration| {
            started
                .checked_add(duration)
                .ok_or(ProbeError::ClockRange("--exit-after"))
        })
        .transpose()?;
    let process = std::process::id();
    let mut sequence = 0_u64;
    emit_record_pair(
        stdout,
        stderr,
        RecordEvent::Tick,
        &mut sequence,
        process,
        started,
    )?;
    let mut next_tick = Instant::now()
        .checked_add(settings.interval)
        .ok_or(ProbeError::ClockRange("--interval"))?;

    loop {
        let now = Instant::now();
        if deadline.is_some_and(|deadline| now >= deadline) {
            emit_record_pair(
                stdout,
                stderr,
                RecordEvent::Exit(settings.exit_code),
                &mut sequence,
                process,
                started,
            )?;
            return Ok(settings.exit_code);
        }
        if now >= next_tick {
            emit_record_pair(
                stdout,
                stderr,
                RecordEvent::Tick,
                &mut sequence,
                process,
                started,
            )?;
            next_tick = Instant::now()
                .checked_add(settings.interval)
                .ok_or(ProbeError::ClockRange("--interval"))?;
            continue;
        }
        let wake_at = deadline.map_or(next_tick, |deadline| deadline.min(next_tick));
        thread::sleep(wake_at.saturating_duration_since(now));
    }
}

fn emit_record_pair(
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    event: RecordEvent,
    sequence: &mut u64,
    process: u32,
    started: Instant,
) -> Result<(), ProbeError> {
    *sequence = sequence
        .checked_add(1)
        .ok_or(ProbeError::SequenceExhausted)?;
    let elapsed_milliseconds = started.elapsed().as_millis();
    write_record(
        stdout,
        "stdout",
        event,
        *sequence,
        process,
        elapsed_milliseconds,
    )
    .map_err(|source| ProbeError::Output {
        stream: "stdout",
        source,
    })?;
    write_record(
        stderr,
        "stderr",
        event,
        *sequence,
        process,
        elapsed_milliseconds,
    )
    .map_err(|source| ProbeError::Output {
        stream: "stderr",
        source,
    })
}

fn write_record(
    writer: &mut impl Write,
    stream: &str,
    event: RecordEvent,
    sequence: u64,
    process: u32,
    elapsed_milliseconds: u128,
) -> io::Result<()> {
    match event {
        RecordEvent::Tick => writeln!(
            writer,
            "stream={stream} event=tick sequence={sequence} pid={process} \
             elapsed_ms={elapsed_milliseconds}"
        )?,
        RecordEvent::Exit(code) => writeln!(
            writer,
            "stream={stream} event=exit sequence={sequence} pid={process} \
             elapsed_ms={elapsed_milliseconds} exit_code={code}"
        )?,
    }
    writer.flush()
}

fn write_help() -> io::Result<()> {
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    writer.write_all(HELP.as_bytes())?;
    writer.flush()
}

fn write_diagnostic(error: &impl Display) {
    write_diagnostic_text(&error.to_string());
}

fn write_diagnostic_text(message: &str) {
    let stderr = io::stderr();
    let mut writer = stderr.lock();
    let _ignored = writeln!(writer, "immortal-log-probe: {message}");
    let _ignored = writer.flush();
}

#[cfg(test)]
mod tests {
    use std::{error::Error, ffi::OsStr, str, time::Duration};

    use super::{
        DEFAULT_EXIT_CODE, DEFAULT_INTERVAL, ParsedCommand, ProbeError, RecordEvent, Settings,
        emit_record_pair, parse_arguments, parse_duration, write_record,
    };

    #[test]
    fn arguments_default_to_one_second_without_exit() -> Result<(), Box<dyn Error>> {
        let command = parse_arguments(std::iter::empty::<&str>())?;
        assert_eq!(
            command,
            ParsedCommand::Run(Settings {
                interval: DEFAULT_INTERVAL,
                exit_after: None,
                exit_code: DEFAULT_EXIT_CODE,
            })
        );
        Ok(())
    }

    #[test]
    fn arguments_parse_interval_deadline_and_exit_code() -> Result<(), Box<dyn Error>> {
        let command = parse_arguments([
            "--interval",
            "25ms",
            "--exit-after",
            "2m",
            "--exit-code",
            "23",
        ])?;
        assert_eq!(
            command,
            ParsedCommand::Run(Settings {
                interval: Duration::from_millis(25),
                exit_after: Some(Duration::from_mins(2)),
                exit_code: 23,
            })
        );
        Ok(())
    }

    #[test]
    fn arguments_accept_help_but_still_reject_unknown_values() -> Result<(), Box<dyn Error>> {
        assert_eq!(parse_arguments(["--help"])?, ParsedCommand::Help);
        assert!(parse_arguments(["--help", "--unknown"]).is_err());
        assert!(parse_arguments(["-h", "--help"]).is_err());
        Ok(())
    }

    #[test]
    fn duration_parser_accepts_every_documented_unit() -> Result<(), Box<dyn Error>> {
        for (value, expected) in [
            ("2ms", Duration::from_millis(2)),
            ("3s", Duration::from_secs(3)),
            ("4m", Duration::from_mins(4)),
            ("5h", Duration::from_hours(5)),
        ] {
            assert_eq!(parse_duration(OsStr::new(value), "--interval")?, expected);
        }
        Ok(())
    }

    #[test]
    fn arguments_reject_ambiguous_or_unbounded_values() {
        for arguments in [
            vec!["--interval", "0s"],
            vec!["--interval", "1"],
            vec!["--interval", "1.5s"],
            vec!["--interval", "1d"],
            vec!["--interval", "18446744073709551615h"],
            vec!["--interval"],
            vec!["--interval", "1s", "--interval", "2s"],
            vec!["--exit-after", "1s", "--exit-after", "2s"],
            vec!["--exit-code", "1"],
            vec!["--exit-after", "1s", "--exit-code", "256"],
            vec!["--exit-after", "1s", "--exit-code", "failure"],
            vec!["--unknown"],
            vec!["positional"],
        ] {
            assert!(
                parse_arguments(arguments).is_err(),
                "unexpected valid arguments"
            );
        }
    }

    #[test]
    fn record_format_identifies_stream_event_and_process() -> Result<(), Box<dyn Error>> {
        let mut output = Vec::new();
        write_record(&mut output, "stdout", RecordEvent::Tick, 7, 42, 125)?;
        write_record(&mut output, "stdout", RecordEvent::Exit(3), 8, 42, 250)?;
        assert_eq!(
            str::from_utf8(&output)?,
            "stream=stdout event=tick sequence=7 pid=42 elapsed_ms=125\n\
             stream=stdout event=exit sequence=8 pid=42 elapsed_ms=250 exit_code=3\n"
        );
        Ok(())
    }

    #[test]
    fn record_sequence_exhaustion_writes_nothing() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut sequence = u64::MAX;
        let result = emit_record_pair(
            &mut stdout,
            &mut stderr,
            RecordEvent::Tick,
            &mut sequence,
            1,
            std::time::Instant::now(),
        );
        assert!(matches!(result, Err(ProbeError::SequenceExhausted)));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }
}
