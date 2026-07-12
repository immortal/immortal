# Release-candidate procedure

Immortal binaries and `immortal-core` are one versioned product. A release
candidate is eligible only when every item below has direct evidence; a green
workflow definition or an unreviewed local measurement is not sufficient.

## External gates

- `immortal/fork#16` is reviewed, merged by its maintainer, and released as the
  expected registry version.
- The Git revision dependency is replaced by that exact registry version and
  `Cargo.lock` records the registry source and checksum.
- Linux, macOS, and FreeBSD each have at least ten retained lifecycle benchmark
  runs across three days at one commit, with reviewed per-platform ceilings.
- Installation and shutdown have been exercised on real target hosts using the
  examples in [INSTALL.md](INSTALL.md).

These gates cannot be checked by elapsed time, intention, or generated text.

## Candidate validation

1. Confirm a clean worktree and review every change since the previous tag.
2. Set one workspace version and keep every crate on `version.workspace = true`.
3. Run the required DevPod CI, FreeBSD cross-check, and a ten-iteration soak.
4. Require green native Linux, macOS, and FreeBSD lifecycle jobs, Security, and
   both fuzz targets for the exact candidate commit.
5. Run `cargo build --workspace --release --locked` and exercise every binary's
   short and long version output from the resulting artifacts.
6. Validate strict version 2 examples with `immortal --check-config` and run an
   `immortaldir --once --dry-run` upgrade rehearsal.
7. Verify that no test or rehearsal leaves a child, process group, socket, lock,
   PID file, runtime directory, or temporary definition behind.
8. Review the breaking-change and rollback instructions before creating a
   signed release-candidate tag.

Do not publish a crate, tag, GitHub release, package, or announcement from an
agent run unless the maintainer explicitly authorizes that external action.
