//! Build-time package and source revision information.

use std::sync::OnceLock;

mod generated {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

/// Git commit used to build this workspace, when built from a repository.
pub const GIT_COMMIT_HASH: Option<&str> = generated::GIT_COMMIT_HASH;

/// Return the shared long version rendered by every Immortal executable.
#[must_use]
pub fn long_version() -> &'static str {
    static LONG_VERSION: OnceLock<String> = OnceLock::new();

    LONG_VERSION
        .get_or_init(|| {
            format!(
                "{} - {}",
                env!("CARGO_PKG_VERSION"),
                GIT_COMMIT_HASH.unwrap_or("unknown")
            )
        })
        .as_str()
}

#[cfg(test)]
mod tests {
    use super::{GIT_COMMIT_HASH, long_version};

    #[test]
    fn long_version_contains_package_version_and_revision() {
        let version = long_version();
        assert!(version.starts_with(env!("CARGO_PKG_VERSION")));
        assert!(version.contains(" - "));
        assert!(version.ends_with(GIT_COMMIT_HASH.unwrap_or("unknown")));
    }
}
