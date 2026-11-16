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

The process-library dependency is the published crates.io `fork` 0.12.0
release, built from fork commit `dd7fec5e87ade5a5200d840611cc693d3f3bd7ae` and
locked in `Cargo.lock` with its registry source and checksum
`5d11ade641d9b2dee55b7704802a27aa5208d968165355a281fc72ab2e9b2f01`, without a
Git or local patch override. It keeps the process-group probe, the
anchor-pinned guard group with helper-mediated delivery, and the
waitable-children policy APIs that COR-008 and COR-009 rely on, and adds the
guard helper's signal reset and orderly cleanup that 0.11.0 lacked; its public
API is checked against 0.11.0 by CI, and its own native Linux, macOS, FreeBSD,
and musl matrix passed for the release commit in
[fork CI run 43914125390](https://github.com/immortal/fork/actions/runs/43914125390).
The process-library gate is complete once the Immortal checkpoint locked to
this release passes its native Linux, macOS, and FreeBSD matrices, recorded
under COR-008 and COR-009 in VALIDATION.md. Release eligibility remains
blocked by that checkpoint and by the external real-host, campaign, and drill
gates above.

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
