//! Shell fixture handshakes have their own finite wall-clock deadline, even if
//! the test process dies without running any cleanup. Only portable utilities
//! are used; each subshell inherits the same absolute deadline.

pub const PREFIX: &str = concat!(
    "deadline=$(($(date +%s) + 20)); ",
    "wait_for() { while [ ! -e \"$1\" ]; do ",
    "[ \"$(date +%s)\" -lt \"$deadline\" ] || exit 124; ",
    "sleep 0.02; done; }; ",
);
