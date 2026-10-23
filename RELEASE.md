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

The process-library dependency is the published crates.io `fork` 0.11.0
release, built from fork commit `bf240d69e96b6f5f3ee58861307cdbb623f02b07` and
locked in `Cargo.lock` with its registry source and checksum
`9b8b9dc1312e1668be5ffa350e3c0adc9b501b2eeebb1b31206477240cc96554`, without a
Git or local patch override. It adds the process-group probe, the anchor-pinned
guard group with helper-mediated delivery, and the waitable-children policy
APIs that COR-008 and COR-009 rely on; its own native Linux, macOS, and FreeBSD
matrix is recorded in [VALIDATION.md](VALIDATION.md). The process-library gate
is complete: the Immortal checkpoint locked to it passes its native Linux,
macOS, and FreeBSD matrices, recorded under COR-008 and COR-009 in
VALIDATION.md. Release eligibility remains blocked by the external real-host,
campaign, and drill gates above.

## Candidate validation

1. Confirm a clean worktree and review every change since the previous tag.
2. Set one workspace version and keep every crate on `version.workspace = true`.
3. Run the required CI in a validation environment from `AGENTS.md`, the
   FreeBSD cross-check, and a ten-iteration soak.
4. Require green native Linux, macOS, and FreeBSD lifecycle jobs, Security, and
   both fuzz targets for the exact candidate commit.
5. Run `cargo build --workspace --release --locked` and exercise every binary's
   short and long version output from the resulting artifacts.
6. Run `scripts/rehearse-upgrade` to validate the strict
   version 2 example with `immortal --check-config` and an
   `immortaldir --once --dry-run` upgrade rehearsal.
7. Verify that no test or rehearsal leaves a child, process group, socket, lock,
   PID file, runtime directory, or temporary definition behind.
8. Review the breaking-change and rollback instructions before creating a
   signed release-candidate tag.

Do not publish a crate, tag, GitHub release, package, or announcement from an
agent run unless the maintainer explicitly authorizes that external action.
