//! Streaming file-adapter execution.
//!
//! The handler copies stdin through the rotating core sink and optionally
//! preserves the original bytes on stdout. Timestamping remains line-oriented
//! without buffering unbounded records, and all writes are synchronized before
//! successful completion.

use std::{
    io::{self, Read, Write},
    time::{SystemTime, UNIX_EPOCH},
};

use immortal_core::logging::RotatingFile;

use super::{ActionError, WriteAction};

const COPY_BUFFER_BYTES: usize = 64 * 1024;

/// Stream stdin into one rotating destination.
///
/// # Errors
///
/// Returns an error for destination, input, output, sync, or rotation failure.
pub fn execute(action: &WriteAction) -> Result<(), ActionError> {
    let mut sink =
        RotatingFile::open(&action.file, action.rotation).map_err(ActionError::Adapter)?;
    let stdin = io::stdin();
    let mut input = stdin.lock();
    if action.passthrough {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        copy_stream(&mut input, &mut sink, Some(&mut output), action.timestamp)
            .map_err(ActionError::Adapter)
    } else {
        copy_stream::<_, _, io::Sink>(&mut input, &mut sink, None, action.timestamp)
            .map_err(ActionError::Adapter)
    }
}

trait LogSink {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn write_parts(&mut self, parts: &[&[u8]]) -> io::Result<()>;
    fn sync(&mut self) -> io::Result<()>;
}

impl LogSink for RotatingFile {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        Self::write_all(self, bytes)
    }

    fn write_parts(&mut self, parts: &[&[u8]]) -> io::Result<()> {
        Self::write_parts(self, parts)
    }

    fn sync(&mut self) -> io::Result<()> {
        Self::sync(self)
    }
}

fn copy_stream<R, S, W>(
    input: &mut R,
    sink: &mut S,
    mut passthrough: Option<&mut W>,
    timestamp: bool,
) -> io::Result<()>
where
    R: Read,
    S: LogSink,
    W: Write,
{
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES].into_boxed_slice();
    let mut line_start = true;
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let bytes = buffer
            .get(..count)
            .ok_or_else(|| io::Error::other("input returned an invalid byte count"))?;
        if timestamp {
            write_timestamped(sink, bytes, &mut line_start)?;
        } else {
            sink.write_all(bytes)?;
        }
        if let Some(output) = passthrough.as_deref_mut() {
            output.write_all(bytes)?;
        }
    }
    sink.sync()?;
    if let Some(output) = passthrough {
        output.flush()?;
    }
    Ok(())
}

fn write_timestamped<S>(sink: &mut S, bytes: &[u8], line_start: &mut bool) -> io::Result<()>
where
    S: LogSink,
{
    for segment in bytes.split_inclusive(|byte| *byte == b'\n') {
        if *line_start {
            let prefix = timestamp_prefix();
            sink.write_parts(&[prefix.as_bytes(), segment])?;
        } else {
            sink.write_all(segment)?;
        }
        *line_start = segment.ends_with(b"\n");
    }
    Ok(())
}

fn timestamp_prefix() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("[{}.{:09}] ", elapsed.as_secs(), elapsed.subsec_nanos())
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fs,
        io::{self, Cursor, Write},
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use immortal_core::logging::{RotatingFile, RotationPolicy};

    use super::{LogSink, copy_stream};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Result<Self, Box<dyn Error>> {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "immortallog-action-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn passthrough_preserves_original_partial_lines() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("api.log");
        let mut sink = RotatingFile::open(&path, RotationPolicy::default())?;
        let original = b"first line\npartial";
        let mut input = Cursor::new(original);
        let mut output = Vec::new();
        copy_stream(&mut input, &mut sink, Some(&mut output), false)?;

        assert_eq!(fs::read(path)?, original);
        assert_eq!(output, original);
        Ok(())
    }

    #[test]
    fn timestamping_is_line_oriented_without_changing_passthrough() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("api.log");
        let mut sink = RotatingFile::open(&path, RotationPolicy::default())?;
        let original = b"one\ntwo\npartial";
        let mut input = Cursor::new(original);
        let mut output = Vec::new();
        copy_stream(&mut input, &mut sink, Some(&mut output), true)?;

        let file = String::from_utf8(fs::read(path)?)?;
        assert_eq!(file.matches("] ").count(), 3);
        assert!(file.contains("] one\n"));
        assert!(file.contains("] two\n"));
        assert!(file.ends_with("] partial"));
        assert_eq!(output, original);
        Ok(())
    }

    #[test]
    fn input_read_failure_is_propagated() -> Result<(), Box<dyn Error>> {
        struct BrokenReader;

        impl io::Read for BrokenReader {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("injected read failure"))
            }
        }

        let directory = TestDirectory::new()?;
        let path = directory.path().join("api.log");
        let mut sink = RotatingFile::open(&path, RotationPolicy::default())?;
        let mut reader = BrokenReader;
        assert!(copy_stream::<_, _, io::Sink>(&mut reader, &mut sink, None, false).is_err());
        Ok(())
    }

    #[test]
    fn sink_and_sync_failures_are_propagated() {
        struct BrokenSink {
            fail_sync: bool,
        }

        impl LogSink for BrokenSink {
            fn write_all(&mut self, _bytes: &[u8]) -> io::Result<()> {
                if self.fail_sync {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "injected disk-full failure",
                    ))
                }
            }

            fn write_parts(&mut self, parts: &[&[u8]]) -> io::Result<()> {
                for part in parts {
                    self.write_all(part)?;
                }
                Ok(())
            }

            fn sync(&mut self) -> io::Result<()> {
                if self.fail_sync {
                    Err(io::Error::other("injected sync failure"))
                } else {
                    Ok(())
                }
            }
        }

        let mut input = Cursor::new(b"record");
        let mut write_failure = BrokenSink { fail_sync: false };
        assert!(
            copy_stream::<_, _, io::Sink>(&mut input, &mut write_failure, None, false).is_err()
        );

        let mut input = Cursor::new(b"record");
        let mut sync_failure = BrokenSink { fail_sync: true };
        assert!(copy_stream::<_, _, io::Sink>(&mut input, &mut sync_failure, None, false).is_err());
    }

    #[test]
    fn downstream_broken_pipe_is_propagated() {
        struct BrokenPipe;

        impl Write for BrokenPipe {
            fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected downstream close",
                ))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut input = Cursor::new(b"record");
        let mut sink = BrokenSink::default();
        let mut downstream = BrokenPipe;
        let error = copy_stream(&mut input, &mut sink, Some(&mut downstream), false)
            .err()
            .map(|error| error.kind());
        assert_eq!(error, Some(io::ErrorKind::BrokenPipe));
    }

    #[test]
    fn huge_partial_line_is_streamed_without_a_line_buffer() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("huge.log");
        let mut sink = RotatingFile::open(&path, RotationPolicy::default())?;
        let original = vec![b'x'; 1024 * 1024 + 17];
        let mut input = Cursor::new(&original);
        let mut output = Vec::new();
        copy_stream(&mut input, &mut sink, Some(&mut output), true)?;

        let file = fs::read(path)?;
        assert!(file.len() > original.len());
        assert!(file.ends_with(&original));
        assert_eq!(output, original);
        Ok(())
    }

    #[derive(Default)]
    struct BrokenSink(Vec<u8>);

    impl LogSink for BrokenSink {
        fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.0.extend_from_slice(bytes);
            Ok(())
        }

        fn write_parts(&mut self, parts: &[&[u8]]) -> io::Result<()> {
            for part in parts {
                self.write_all(part)?;
            }
            Ok(())
        }

        fn sync(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
