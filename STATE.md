# Becky Project State

Last updated: 2026-06-08

`STATE.md` is the authoritative snapshot of the repository's current condition. Use it together with `ROADMAP.md` for planning and prioritization. `ISSUES.md` is retired and should not be reintroduced.

## Project Summary

Becky is an early-stage Rust workspace for building compute orchestrators. The workspace has a provider-agnostic engine plus concrete providers for:

- QEMU virtual machines
- Docker containers
- ordinary host processes
- Rust function workers

The codebase is in active R&D. The architecture is clear enough to build against, but several contracts, persistence layers, and end-to-end lifecycle tests are still incomplete.

## Workspace Shape

- `becky-engine`: orchestration traits, control/state types, metadata/storage contracts, host registration, OS image metadata, and resource descriptions
- `becky-fx-docker`: Docker provider with container create/start/reattach/monitor and stats support
- `becky-fx-id`: stable identifiers for Becky-managed effects
- `becky-fx-qemu`: QEMU provider with VM allocation, storage creation, QMP/QGA monitoring, reattach, and metadata-backed reconciliation
- `becky-fx-rust-fn`: Rust-function worker provider with reattachable child-process execution
- `becky-fx-system-command`: provider for ordinary host processes
- `becky-utils`: shared process and async command helpers
- `examples/bare`: minimal example workspace consumer

## Maturity Assessment

### What is working well

- The workspace builds cleanly and had a fully green validation snapshot on 2026-05-26:
  - `cargo fmt --all --check`
  - `cargo check --workspace`
  - `cargo test --workspace`
  - `cargo clippy --workspace --all-targets --all-features`
- The provider split is coherent. Provider-specific behavior mostly stays out of `becky-engine`.
- QEMU provider coverage improved materially:
  - guest-agent wiring is present
  - QGA readiness/capability checks are implemented
  - stop/shutdown behavior is more explicit
  - desired-state reconstruction from metadata is substantially better
  - baseline user-mode networking is translated into QEMU args
  - unsupported boot/extra-option combinations now fail explicitly
  - important QMP events are surfaced
- System-command and Rust-function providers have clearer stop/reattach behavior than earlier revisions.
- Core lint posture is strong:
  - `unsafe_code = "deny"`
  - Clippy denies `unwrap_used` and `expect_used`

### What is still incomplete

- `becky-engine` still defines contracts ahead of complete in-tree implementations.
- Metadata persistence is not solved in-tree. The only current engine implementation is effectively no-op storage.
- Provider lifecycle coverage is still mostly unit-level rather than end-to-end integration-level.
- QEMU remains the deepest provider, but it still has unresolved runtime and packaging gaps.

## Current Constraints

### Engine

- Metadata traits support inventory-style operations, but there is no real persistent backend in-tree.
- Shared contract tests for provider lifecycle semantics do not exist yet.
- The engine should still be treated as a contract layer, not as a complete runtime stack.

### QEMU

- Live storage hotplug/detach is not implemented.
- Archive behavior is state-only and does not yet model complete disk capture/restore semantics.
- Fresh checkouts are constrained by the local path dependency on `../../../vmm-command-builders/qemu-command-builder`.
- Real `qemu-system-*` lifecycle integration tests are still missing.

### Docker

- Core flows exist, but reattach/monitor behavior is not protected by gated integration tests.

### Rust Function Provider

- Registry dispatch, pidfile-based reattach, and full worker lifecycle need integration coverage.

### System Command Provider

- Archive/checkpoint support is explicitly unsupported.
- Process identity and lifecycle semantics still need harder review before production use.

## Operational Notes

- The workspace uses Rust 2024 and currently declares `rust-version = "1.95.0"`.
- Dependencies are centralized in the root `Cargo.toml`.
- Some validation commands may need to run outside the sandbox because local toolchain wrappers can fail under sandbox restrictions.
- QEMU-related tests must stay gated on local binary/accelerator availability.

## Validation Status

Last recorded full-workspace validation snapshot: 2026-05-26.

- `cargo fmt --all --check`: passed
- `cargo check --workspace`: passed
- `cargo test --workspace`: passed
- `cargo clippy --workspace --all-targets --all-features`: passed

No newer validation run is recorded in this document yet.

## Bottom Line

Becky is past the stage of being only a crate skeleton, but it is not yet a production-ready orchestration framework. The strongest current path is:

1. finish the missing persistence and contract-testing foundations in `becky-engine`
2. harden provider lifecycle behavior with gated integration tests
3. close the remaining QEMU runtime gaps
