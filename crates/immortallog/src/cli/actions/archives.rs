//! Archive discovery and stable table or JSON rendering.
//!
//! The handler queries the core archive catalog used by retention, validates
//! timestamp and path representations, and writes one deterministic result.

use std::io::{self, Write};

use immortal_core::logging::{Archive, archives as list_archives};
use jiff::Timestamp;
use serde::Serialize;

use super::{ActionError, ArchivesAction, OutputFormat};

const ROTATED_AT_UTC_WIDTH: usize = 30;

/// Inspect and render one live-file archive namespace.
///
/// # Errors
///
/// Returns an error for discovery, timestamp conversion, path encoding, output,
/// or JSON serialization failure.
pub fn execute(action: &ArchivesAction) -> Result<(), ActionError> {
    let archives = list_archives(&action.file).map_err(ActionError::Archives)?;
    let records = archive_records(&archives)?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    render_archives(&mut output, &records, action.output)
}

#[derive(Debug, Serialize)]
struct ArchiveRecord<'a> {
    rotated_at_utc: String,
    unix_nanoseconds: String,
    bytes: u64,
    pid: u32,
    sequence: u64,
    path: &'a str,
}

fn archive_records(archives: &[Archive]) -> Result<Vec<ArchiveRecord<'_>>, ActionError> {
    archives.iter().map(archive_record).collect()
}

fn archive_record(archive: &Archive) -> Result<ArchiveRecord<'_>, ActionError> {
    let unix_nanoseconds = archive.unix_nanoseconds();
    let signed_nanoseconds = i128::try_from(unix_nanoseconds)
        .map_err(|_| ActionError::TimestampRange(unix_nanoseconds))?;
    let timestamp = Timestamp::from_nanosecond(signed_nanoseconds).map_err(|source| {
        ActionError::Timestamp {
            unix_nanoseconds,
            source,
        }
    })?;
    let path = archive.path().to_str().ok_or(ActionError::NonUtf8Path)?;
    Ok(ArchiveRecord {
        rotated_at_utc: format!("{timestamp:.9}"),
        unix_nanoseconds: unix_nanoseconds.to_string(),
        bytes: archive.byte_length(),
        pid: archive.adapter_pid(),
        sequence: archive.sequence(),
        path,
    })
}

fn render_archives(
    output: &mut impl Write,
    records: &[ArchiveRecord<'_>],
    format: OutputFormat,
) -> Result<(), ActionError> {
    match format {
        OutputFormat::Table => render_table(output, records),
        OutputFormat::Json => {
            serde_json::to_writer(&mut *output, records).map_err(ActionError::Json)?;
            writeln!(output).map_err(ActionError::Archives)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TableWidths {
    path: usize,
    bytes: usize,
    pid: usize,
}

impl TableWidths {
    fn for_records(records: &[ArchiveRecord<'_>]) -> Self {
        Self {
            path: records
                .iter()
                .map(|record| record.path.chars().count())
                .max()
                .unwrap_or(0)
                .max("PATH".len()),
            bytes: records
                .iter()
                .map(|record| record.bytes.to_string().len())
                .max()
                .unwrap_or(0)
                .max("BYTES".len()),
            pid: records
                .iter()
                .map(|record| record.pid.to_string().len())
                .max()
                .unwrap_or(0)
                .max("PID".len()),
        }
    }
}

fn render_table(output: &mut impl Write, records: &[ArchiveRecord<'_>]) -> Result<(), ActionError> {
    let widths = TableWidths::for_records(records);
    writeln!(
        output,
        "{:<path_width$}  {:<ROTATED_AT_UTC_WIDTH$}  {:>bytes_width$}  {:>pid_width$}",
        "PATH",
        "ROTATED_AT_UTC",
        "BYTES",
        "PID",
        path_width = widths.path,
        bytes_width = widths.bytes,
        pid_width = widths.pid,
    )
    .map_err(ActionError::Archives)?;
    for record in records {
        writeln!(
            output,
            "{:<path_width$}  {:<ROTATED_AT_UTC_WIDTH$}  {:>bytes_width$}  {:>pid_width$}",
            record.path,
            record.rotated_at_utc,
            record.bytes,
            record.pid,
            path_width = widths.path,
            bytes_width = widths.bytes,
            pid_width = widths.pid,
        )
        .map_err(ActionError::Archives)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use immortal_core::logging::archives as list_archives;

    use super::{OutputFormat, ROTATED_AT_UTC_WIDTH, archive_records, render_archives};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Result<Self, Box<dyn Error>> {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "immortallog-archives-action-{}-{sequence}",
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
    fn archive_records_render_as_utc_table_and_exact_json() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let live = directory.path().join("api.log");
        let archive = directory
            .path()
            .join("api.log.@1784103427741753038.297839.1");
        fs::write(&archive, vec![b'x'; 1024])?;
        let archives = list_archives(&live)?;
        let records = archive_records(&archives)?;

        let mut table = Vec::new();
        render_archives(&mut table, &records, OutputFormat::Table)?;
        let archive_text = archive
            .to_str()
            .ok_or("archive test path is not valid UTF-8")?;
        let path_width = archive_text.chars().count();
        assert_eq!(
            String::from_utf8(table)?,
            format!(
                "{:<path_width$}  {:<ROTATED_AT_UTC_WIDTH$}  {:>5}  {:>6}\n\
                 {:<path_width$}  {:<ROTATED_AT_UTC_WIDTH$}  {:>5}  {:>6}\n",
                "PATH",
                "ROTATED_AT_UTC",
                "BYTES",
                "PID",
                archive_text,
                "2026-07-15T08:17:07.741753038Z",
                1024,
                297_839,
            )
        );

        let mut json = Vec::new();
        render_archives(&mut json, &records, OutputFormat::Json)?;
        assert_eq!(
            String::from_utf8(json)?,
            format!(
                concat!(
                    "[{{\"rotated_at_utc\":\"2026-07-15T08:17:07.741753038Z\",",
                    "\"unix_nanoseconds\":\"1784103427741753038\",\"bytes\":1024,",
                    "\"pid\":297839,\"sequence\":1,\"path\":\"{}\"}}]\n"
                ),
                archive.display()
            )
        );
        Ok(())
    }

    #[test]
    fn empty_archive_records_have_stable_output() -> Result<(), Box<dyn Error>> {
        let mut table = Vec::new();
        render_archives(&mut table, &[], OutputFormat::Table)?;
        assert_eq!(
            String::from_utf8(table)?,
            format!(
                "{:<4}  {:<ROTATED_AT_UTC_WIDTH$}  {:>5}  {:>3}\n",
                "PATH", "ROTATED_AT_UTC", "BYTES", "PID"
            )
        );

        let mut json = Vec::new();
        render_archives(&mut json, &[], OutputFormat::Json)?;
        assert_eq!(json, b"[]\n");
        Ok(())
    }
}
