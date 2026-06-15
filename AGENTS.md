# AGENTS.md

## Project Overview

Becky is an early-stage Rust workspace for building compute orchestrators. The workspace currently contains:

- `becky-engine`: core orchestration traits, state types, metadata/storage contracts, host registration, OS image metadata, and resource descriptions.
- `becky-fx-docker`: Docker provider for image allocation, container create/start/reattach/monitor flows, and container stats.
- `becky-fx-id`: stable identifiers for Becky-managed effects.
- `becky-fx-qemu`: QEMU VM provider for VM allocation, storage creation, QMP/QGA monitoring, reattachment, and metadata-backed reconciliation.
- `becky-fx-rust-fn`: Rust-function provider that runs registered functions as reattachable worker processes.
- `becky-fx-system-command`: a provider that manages ordinary host processes as Becky effects.
- `becky-utils`: shared process and async command helpers.

## Authoritative Project Docs

- `STATE.md`: the authoritative snapshot of the repository's current state, constraints, and latest recorded validation status.
- `ROADMAP.md`: the authoritative prioritization document for upcoming work and definitions of done.

`ISSUES.md` is retired and should not be recreated. When project status or priorities change, update `STATE.md` and `ROADMAP.md` instead.

## Repository Commands

Run these from the repository root:

```sh
cargo fmt --all
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features
```

The workspace uses Rust 2024 and currently declares `rust-version = "1.95.0"`.

## Coding Guidelines

- Prefer existing crate boundaries and trait shapes. `becky-engine` should remain provider-agnostic; concrete process or VM behavior belongs in provider crates.
- Keep public APIs documented. Add `///` comments for public traits, structs, enums, associated types, and methods when adding or changing them.
- Do not introduce `unwrap()` or `expect()` in production code. Workspace lints deny both through Clippy.
- Do not add unsafe code. The workspace denies unsafe code; existing provider-specific exceptions should be reviewed carefully before expanding them.
- Use `async_trait` consistently with existing async trait APIs.
- Keep no-op implementations explicit and small. The `Metadataless`, `Storageless`, and placeholder resource types are intended for tests, examples, and minimal providers.
- Preserve public names unless intentionally making a breaking API change.

## Testing Notes

- Add focused unit tests near the code they cover.
- For process-management changes in `becky-fx-system-command`, test both command-line matching and lifecycle state transitions when practical.
- For provider changes, prefer pure unit tests around command construction, metadata records, path generation, parsing, and state mapping before adding gated integration tests.
- QEMU integration tests should be gated on the relevant local binaries and accelerators. Do not assume `/dev/kvm`, HVF, Docker, or a sibling QEMU builder checkout is available in every environment.
- Prefer deterministic commands in tests. Existing Unix-only tests use `/bin/sh` and are guarded with `#[cfg(unix)]`.
- If a test needs platform-specific behavior, gate it with `#[cfg(...)]` instead of relying on runtime detection.

## Known Sharp Edges

- `becky-engine` is still a skeleton in several areas. Many traits define contracts before concrete backends exist.
- `becky-fx-system-command` process lifecycle semantics need review before production use, especially status reporting and process identity matching.
- `becky-fx-qemu` depends on a local `qemu-command-builder` path (`../../../vmm-command-builders/qemu-command-builder`) in this checkout.
- QEMU guest-agent capability discovery is intentionally not complete yet; do not assume QGA commands are available just because the guest-agent socket exists.

## Current Priorities

- Add a real persistent metadata backend in `becky-engine`.
- Add shared engine contract tests for lifecycle, metadata, and storage semantics.
- Add gated integration tests for providers, starting with system-command and Rust-function flows.
- Close the remaining QEMU gaps around live storage control, archival semantics, packaging, and real-process integration coverage.

## Dependency Guidance

- Workspace dependencies are centralized in the root `Cargo.toml`; add new dependency versions there when multiple crates may use them.
- Network access is restricted in some agent environments, so prefer checks that use the existing lockfile and cached dependencies.
