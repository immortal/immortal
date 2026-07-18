//! Schema-1 validation-evidence rows shared by the resource-soak contracts.
//!
//! A soak run records its memory, descriptor, child, and latency trends in the
//! same tab-separated `schema_version` 1 layout that
//! `scripts/validate-evidence.awk` enforces, so its output drops straight into
//! the retained `validation/evidence` convention. This module owns the
//! environment columns (commit and platform identity) and the row formatting;
//! each contract owns which metrics it measures. Environment detection is best
//! effort and always yields non-empty fields, so the emitted file satisfies the
//! validator on every supported platform even when a probe is unavailable.

use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

use immortal_core::build_info;

const HEADER: &str = "schema_version\tcommit\tplatform\tkernel\tcpu\ttoolchain\tsupervisor\tsupervisor_version\tscenario\tsample\tmetric\tvalue\tunit\toutcome\tcleanup";

const TOOLCHAIN_ENV: &str = "IMMORTAL_SOAK_TOOLCHAIN";

/// Whether one measured row met its bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Pass,
    Fail,
}

impl Outcome {
    /// Render the outcome as the exact token the evidence schema accepts.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
        }
    }
}

/// One measured evidence row's scenario-specific columns.
pub struct EvidenceRow {
    /// Positive, monotonically assigned sample ordinal.
    pub sample: u64,
    /// Stable metric name, for example `resident_kib`.
    pub metric: &'static str,
    /// Base-ten metric value already formatted for the schema.
    pub value: String,
    /// Metric unit, for example `kibibytes`.
    pub unit: &'static str,
    /// Whether this sample met its bound.
    pub outcome: Outcome,
}

/// Environment columns shared by every row of one evidence file.
pub struct EvidenceEnvironment {
    commit: String,
    platform: String,
    kernel: String,
    cpu: String,
    toolchain: String,
    supervisor_version: String,
}

impl EvidenceEnvironment {
    /// Detect the recording environment, falling back to non-empty defaults.
    #[must_use]
    pub fn detect() -> Self {
        Self {
            commit: build_info::GIT_COMMIT_HASH.unwrap_or("unknown").to_owned(),
            platform: uname("-s").unwrap_or_else(|| std::env::consts::OS.to_owned()),
            kernel: uname("-r").unwrap_or_else(|| "unknown".to_owned()),
            cpu: uname("-m").unwrap_or_else(|| std::env::consts::ARCH.to_owned()),
            toolchain: toolchain(),
            supervisor_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// Write a complete schema-1 evidence file for one soak run.
///
/// # Errors
///
/// Returns an I/O error when the evidence file cannot be written.
pub fn write_results(
    path: &Path,
    environment: &EvidenceEnvironment,
    scenario: &str,
    cleanup: &str,
    rows: &[EvidenceRow],
) -> io::Result<()> {
    let commit = &environment.commit;
    let platform = &environment.platform;
    let kernel = &environment.kernel;
    let cpu = &environment.cpu;
    let toolchain = &environment.toolchain;
    let version = &environment.supervisor_version;

    let header = format!("{HEADER}\n");
    let body: String = std::iter::once(header)
        .chain(rows.iter().map(|row| {
            let sample = row.sample;
            let metric = row.metric;
            let value = &row.value;
            let unit = row.unit;
            let outcome = row.outcome.as_str();
            format!(
                "1\t{commit}\t{platform}\t{kernel}\t{cpu}\t{toolchain}\timmortal\t{version}\t{scenario}\t{sample}\t{metric}\t{value}\t{unit}\t{outcome}\t{cleanup}\n"
            )
        }))
        .collect();
    fs::write(path, body)
}

/// Return one trimmed `uname` field, or `None` when it is empty or unavailable.
fn uname(flag: &str) -> Option<String> {
    command_trimmed_output(Command::new("uname").arg(flag))
}

/// Resolve the recording toolchain from the override or `rustc`, else `unknown`.
fn toolchain() -> String {
    if let Some(text) = env_toolchain() {
        return text;
    }
    command_trimmed_output(Command::new("rustc").arg("--version"))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Read the toolchain override, ignoring an unset, non-UTF-8, or empty value.
fn env_toolchain() -> Option<String> {
    let value = std::env::var_os(TOOLCHAIN_ENV)?;
    let text = value.into_string().ok()?;
    non_empty(text.trim())
}

/// Run one probe and return its trimmed standard output when it succeeds.
fn command_trimmed_output(command: &mut Command) -> Option<String> {
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    non_empty(text.trim())
}

/// Convert a trimmed string slice into an owned value only when it is non-empty.
fn non_empty(trimmed: &str) -> Option<String> {
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}
