use tracing_subscriber::EnvFilter;

use crate::cli::commands;

/// Parse the command line and initialize local diagnostics.
pub fn start() {
    let matches = commands::new().get_matches();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));

    drop(
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init(),
    );

    tracing::debug!(
        arguments = matches.ids().count(),
        "parsed immortalctl command line"
    );
}
