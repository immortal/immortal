# Release-candidate procedure

Immortal binaries and `immortal-core` are one versioned product. A release
candidate is eligible only when every item below has direct evidence; a green
workflow definition or an unreviewed local measurement is not sufficient.

## External gates

- Linux, macOS, and FreeBSD each have at least ten retained lifecycle benchmark
  runs across three days at one commit, with reviewed per-platform ceilings.
- Installation and shutdown have been exercised on real target hosts using the
  examples in [INSTALL.md](INSTALL.md).
- The exact candidate has completed the 24-hour Linux, macOS, and FreeBSD fault
  campaigns plus the seven-day FreeBSD canary defined in
  [VALIDATION.md](VALIDATION.md), with reviewed cleanup and trend evidence.

These gates cannot be checked by elapsed time, intention, or generated text.

The process-library gate is complete. Immortal locks the published crates.io
`fork` 0.9.1 release, including the broker-death containment API, without a Git
or local patch override. The signed 0.9.1 tag and the combined Immortal
dependency checkpoint pass their native Linux, macOS, and FreeBSD matrices;
`Cargo.lock` records the registry source and checksum. Release eligibility
remains blocked by the external real-host, campaign, and drill gates above,
not by the process-library dependency.

## Candidate validation

1. Confirm a clean worktree and review every change since the previous tag.
2. Set one workspace version and keep every crate on `version.workspace = true`.
3. Run the required DevPod CI, FreeBSD cross-check, and a ten-iteration soak.
4. Require green native Linux, macOS, and FreeBSD lifecycle jobs, Security, and
   both fuzz targets for the exact candidate commit.
5. Run `cargo build --workspace --release --locked` and exercise every binary's
   short and long version output from the resulting artifacts.
6. Run `scripts/dev-ssh scripts/rehearse-upgrade` to validate the strict
   version 2 example with `immortal --check-config` and an
   `immortaldir --once --dry-run` upgrade rehearsal.
7. Verify that no test or rehearsal leaves a child, process group, socket, lock,
   PID file, runtime directory, or temporary definition behind.
8. Review the breaking-change and rollback instructions before creating a
   signed release-candidate tag.

Do not publish a crate, tag, GitHub release, package, or announcement from an
agent run unless the maintainer explicitly authorizes that external action.
