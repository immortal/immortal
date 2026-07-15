//! Bounded black-box contract for archive discovery, rendering, and failures.

use std::{
    error::Error,
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read},
    os::unix::{ffi::OsStringExt, fs::symlink},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use immortal_core::exit::ExitClass;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

fn main() -> Result<(), Box<dyn Error>> {
    let binary = Path::new(env!("CARGO_BIN_EXE_immortallog"));
    let directory = TemporaryDirectory::new()?;

    prove_write_and_startup_routes(binary, directory.path())?;
    prove_table_and_json_output(binary, directory.path())?;
    prove_empty_namespace(binary, directory.path())?;
    prove_stable_failures(binary, directory.path())
}

fn prove_write_and_startup_routes(binary: &Path, directory: &Path) -> Result<(), Box<dyn Error>> {
    let live = directory.join("write.log");
    let write = run(binary, &[live.as_os_str()])?;
    require_status(write.status, ExitClass::Success, "write route")?;
    if !live.is_file() {
        return Err("write action did not create its destination".into());
    }

    let invalid = run(binary, &[OsStr::new("archives")])?;
    require_status(
        invalid.status,
        ExitClass::Usage,
        "missing archive namespace",
    )
}

fn prove_table_and_json_output(binary: &Path, directory: &Path) -> Result<(), Box<dyn Error>> {
    let live = directory.join("app.log");
    let oldest = directory.join("app.log.@2.20.10");
    let newest = directory.join("app.log.@10.3.2");
    fs::write(&oldest, b"a")?;
    fs::write(&newest, b"new")?;

    fs::write(directory.join("app.log.@2.20"), b"truncated")?;
    fs::write(directory.join("app.log.@2.20.10.extra"), b"extra")?;
    fs::write(directory.join("app.log.@nonnumeric.20.10"), b"invalid")?;
    fs::write(
        directory.join("app.log.immortal-archive.1.1.1"),
        b"prototype",
    )?;
    fs::write(directory.join("unrelated.log.@1.1.1"), b"unrelated")?;
    fs::create_dir(directory.join("app.log.@3.1.1"))?;
    symlink(&oldest, directory.join("app.log.@4.1.1"))?;

    let table = run(binary, &[OsStr::new("archives"), live.as_os_str()])?;
    require_status(table.status, ExitClass::Success, "table output")?;
    let oldest_text = utf8_path(&oldest)?;
    let newest_text = utf8_path(&newest)?;
    let path_width = oldest_text.chars().count().max(newest_text.chars().count());
    let expected_table = format!(
        "{:<path_width$}  {:<30}  {:>5}  {:>3}\n\
         {oldest_text:<path_width$}  {:<30}  {:>5}  {:>3}\n\
         {newest_text:<path_width$}  {:<30}  {:>5}  {:>3}\n",
        "PATH",
        "ROTATED_AT_UTC",
        "BYTES",
        "PID",
        "1970-01-01T00:00:00.000000002Z",
        1,
        20,
        "1970-01-01T00:00:00.000000010Z",
        3,
        3,
    );
    let actual_table = std::str::from_utf8(&table.stdout)?;
    if actual_table != expected_table {
        return Err(format!(
            "unexpected archive table:\nexpected: {expected_table:?}\nactual: {actual_table:?}"
        )
        .into());
    }
    require_empty_stderr(&table, "table output")?;

    let json = run(
        binary,
        &[
            OsStr::new("archives"),
            OsStr::new("--output"),
            OsStr::new("json"),
            live.as_os_str(),
        ],
    )?;
    require_status(json.status, ExitClass::Success, "JSON output")?;
    let actual_json: serde_json::Value = serde_json::from_slice(&json.stdout)?;
    let expected_json = serde_json::json!([
        {
            "rotated_at_utc": "1970-01-01T00:00:00.000000002Z",
            "unix_nanoseconds": "2",
            "bytes": 1,
            "pid": 20,
            "sequence": 10,
            "path": oldest_text,
        },
        {
            "rotated_at_utc": "1970-01-01T00:00:00.000000010Z",
            "unix_nanoseconds": "10",
            "bytes": 3,
            "pid": 3,
            "sequence": 2,
            "path": newest_text,
        }
    ]);
    if actual_json != expected_json {
        return Err(format!("unexpected archive JSON: {actual_json}").into());
    }
    require_empty_stderr(&json, "JSON output")
}

fn prove_empty_namespace(binary: &Path, directory: &Path) -> Result<(), Box<dyn Error>> {
    let live = directory.join("empty.log");
    let table = run(binary, &[OsStr::new("archives"), live.as_os_str()])?;
    require_status(table.status, ExitClass::Success, "empty table")?;
    let expected_table = format!(
        "{:<4}  {:<30}  {:>5}  {:>3}\n",
        "PATH", "ROTATED_AT_UTC", "BYTES", "PID"
    );
    if table.stdout != expected_table.as_bytes() {
        return Err("empty archive table did not contain only its header".into());
    }

    let json = run(
        binary,
        &[
            OsStr::new("archives"),
            OsStr::new("-o"),
            OsStr::new("json"),
            live.as_os_str(),
        ],
    )?;
    require_status(json.status, ExitClass::Success, "empty JSON")?;
    if json.stdout != b"[]\n" {
        return Err("empty archive JSON was not an empty array".into());
    }
    Ok(())
}

fn prove_stable_failures(binary: &Path, directory: &Path) -> Result<(), Box<dyn Error>> {
    let missing = directory.join("missing").join("app.log");
    let missing_output = run(binary, &[OsStr::new("archives"), missing.as_os_str()])?;
    require_status(
        missing_output.status,
        ExitClass::NotFound,
        "missing archive directory",
    )?;
    require_diagnostic(&missing_output, "archive inspection failed")?;

    let range_live = directory.join("range.log");
    fs::write(
        directory.join(format!("range.log.@{}.1.1", u128::MAX)),
        b"range",
    )?;
    let range_output = run(binary, &[OsStr::new("archives"), range_live.as_os_str()])?;
    require_status(
        range_output.status,
        ExitClass::Data,
        "unrepresentable archive timestamp",
    )?;
    require_diagnostic(&range_output, "outside the supported UTC range")?;

    let non_utf8_parent = directory.join(OsString::from_vec(b"non-utf8-\xff".to_vec()));
    fs::create_dir(&non_utf8_parent)?;
    fs::write(non_utf8_parent.join("app.log.@1.1.1"), b"path")?;
    let non_utf8_live = non_utf8_parent.join("app.log");
    let non_utf8_output = run(binary, &[OsStr::new("archives"), non_utf8_live.as_os_str()])?;
    require_status(
        non_utf8_output.status,
        ExitClass::Data,
        "non-UTF-8 archive path",
    )?;
    require_diagnostic(&non_utf8_output, "archive path is not valid UTF-8")
}

fn run(binary: &Path, arguments: &[&OsStr]) -> Result<Output, Box<dyn Error>> {
    let child = Command::new(binary)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    ChildGuard::new(child)
        .wait_with_output(COMMAND_TIMEOUT)
        .map_err(Into::into)
}

fn require_status(
    status: ExitStatus,
    expected: ExitClass,
    context: &str,
) -> Result<(), Box<dyn Error>> {
    if status.code() == Some(i32::from(expected.value())) {
        Ok(())
    } else {
        Err(format!(
            "{context} returned {status}; expected exit {}",
            expected.value()
        )
        .into())
    }
}

fn require_empty_stderr(output: &Output, context: &str) -> Result<(), Box<dyn Error>> {
    if output.stderr.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{context} wrote unexpected stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into())
    }
}

fn require_diagnostic(output: &Output, expected: &str) -> Result<(), Box<dyn Error>> {
    let stderr = std::str::from_utf8(&output.stderr)?;
    if stderr.contains(expected) {
        Ok(())
    } else {
        Err(format!("diagnostic {stderr:?} did not contain {expected:?}").into())
    }
}

fn utf8_path(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| "contract path is not valid UTF-8".into())
}

struct ChildGuard {
    child: Child,
    reaped: bool,
}

impl ChildGuard {
    const fn new(child: Child) -> Self {
        Self {
            child,
            reaped: false,
        }
    }

    fn wait_with_output(mut self, timeout: Duration) -> io::Result<Output> {
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = self.child.try_wait()? {
                self.reaped = true;
                break status;
            }
            if Instant::now() >= deadline {
                self.child.kill()?;
                let _status = self.child.wait()?;
                self.reaped = true;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "immortallog archives contract exceeded its deadline",
                ));
            }
            thread::sleep(POLL_INTERVAL);
        };

        let mut stdout = Vec::new();
        self.child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("archive command stdout was not piped"))?
            .read_to_end(&mut stdout)?;
        let mut stderr = Vec::new();
        self.child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("archive command stderr was not piped"))?
            .read_to_end(&mut stderr)?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        let _killed = self.child.kill();
        let _status = self.child.wait();
        self.reaped = true;
    }
}

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn new() -> Result<Self, Box<dyn Error>> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "immortallog-archives-contract-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _removed = fs::remove_dir_all(&self.0);
    }
}
