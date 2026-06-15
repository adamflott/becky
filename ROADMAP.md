# Becky Roadmap

Last updated: 2026-06-08

`ROADMAP.md` is the authoritative prioritization document for Becky. It should describe what matters next, why it matters, and what "done" looks like. Pair it with `STATE.md`, which records the current repository condition.

## Roadmap Principles

- Keep `becky-engine` provider-agnostic.
- Prefer contract hardening before feature sprawl.
- Land deterministic unit coverage first, then gated integration tests.
- Make unsupported behavior explicit instead of silently partial.
- Preserve public names unless a deliberate breaking change is being made.

## Phase 1: Foundation Hardening

This is the current highest-leverage work. Becky needs stronger core contracts before expanding feature scope.

### 1. Add a real metadata backend to `becky-engine`

Why:

- Reattach and reconciliation are central Becky behaviors.
- In-tree examples and tests cannot exercise those flows meaningfully while metadata is effectively discarded.

Definition of done:

- a simple persistent metadata backend exists in-tree
- provider examples can use it without bespoke glue
- at least one end-to-end reattach path uses it in tests or examples

### 2. Add shared engine contract tests

Why:

- Providers currently prove their own helpers, but not enough shared lifecycle semantics.
- Drift between providers is likely unless the engine defines executable behavioral expectations.

Definition of done:

- reusable tests or helpers cover allocate/start/status/stop/destroy semantics
- metadata inventory/update behavior is covered
- storage create/check/open/close/resize behavior is covered
- unsupported-operation behavior is asserted consistently

## Phase 2: Provider Lifecycle Confidence

Once the engine contracts are firmer, the next priority is proving providers under realistic lifecycle conditions.

### 3. Add gated provider integration tests

Priority order:

1. `becky-fx-system-command`
2. `becky-fx-rust-fn`
3. `becky-fx-docker`
4. `becky-fx-qemu`

Why:

- These providers all depend on process/container/VM lifecycle behavior that unit tests only approximate.

Definition of done:

- each provider has at least one gated happy-path lifecycle test
- tests skip cleanly when platform prerequisites are missing
- tests verify reattach semantics where the provider claims to support them

## Phase 3: QEMU Completion Work

QEMU is currently the most ambitious provider and remains the most strategically important runtime backend.

### 4. Implement live storage control as a separate runtime path

Why:

- Offline storage lifecycle methods are not enough for running VMs.
- Overloading offline storage APIs for live block device control will blur contracts.

Definition of done:

- live attach/detach behavior is modeled explicitly
- QMP-based add/remove flows are implemented
- metadata and runtime state stay consistent after attach/detach

### 5. Redesign archive policy beyond state-only snapshots

Why:

- Saving VM state without modeling disk state is not a complete archival story.

Definition of done:

- archive policy distinguishes state-only, state-plus-disk, and disabled modes
- unsupported storage/archive combinations fail explicitly
- restore expectations are documented

### 6. Remove or reduce the local `qemu-command-builder` checkout requirement

Why:

- The current sibling-path dependency makes fresh checkout setup fragile.

Definition of done:

- the dependency is vendored, published, or otherwise documented as an intentional setup requirement
- fresh-checkout build instructions are unambiguous

### 7. Add gated `qemu-system-*` lifecycle integration coverage

Why:

- QMP/QGA/process supervision issues are hard to trust without a real process-level test.

Definition of done:

- a gated test boots a minimal QEMU process
- the test attaches to QMP and exercises lifecycle shutdown
- the test skips cleanly when the required binary or host capability is absent

## Phase 4: Secondary Provider Improvements

### 8. Harden Docker reattach and monitor behavior

Definition of done:

- gated Docker tests verify create/start/reattach/status/destroy flows
- runtime accounting and name/id matching behavior are asserted

### 9. Harden Rust-function worker process contracts

Definition of done:

- integration coverage verifies registry dispatch
- pidfile-based reattach is exercised
- stop behavior and pidfile cleanup are asserted

### 10. Reassess system-command archive support

Why:

- Archive is currently unsupported, which is acceptable unless Becky needs process checkpointing.

Definition of done:

- either explicit non-goal documentation remains in place
- or an optional CRIU-backed design is specified before implementation starts

## Explicit Non-Priorities Right Now

- broad feature expansion in `becky-engine` before metadata/contracts are hardened
- pretending archive/checkpoint semantics exist where they do not
- ungated integration tests that assume Docker, KVM, HVF, or local QEMU builder checkouts
- cosmetic cleanup that does not reduce behavioral risk

## Maintenance Rules

- Update `STATE.md` when the repository's actual condition changes.
- Update `ROADMAP.md` when priorities or definitions of done change.
- Do not recreate `ISSUES.md`; fold issue tracking into these two documents.
