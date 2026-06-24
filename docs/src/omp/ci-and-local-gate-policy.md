---
title: Ompzed CI & Local-Gate Policy
description: "Record of the enforced pre-merge proof for the Ompzed fork: a targeted local gate (per-crate test + debug clippy + ompzed build) that stands in until GitHub Actions is enabled, and an explicit non-goal of the upstream Zed release CI matrix."
---

# Ompzed CI & Local-Gate Policy

> **Status:** Internal engineering decision record (AGE-662). Not part of the
> published end-user book; intentionally omitted from `SUMMARY.md`.
>
> **HITL (NOT in this change):** enabling GitHub Actions on the fork is
> human-owned — it requires turning on Actions in the repository settings and
> provisioning any secrets the chosen jobs need. Until that happens, the local
> gate described below is the **required, enforced** pre-merge proof. A minimal
> draft workflow ships alongside this record at `.github/workflows/ci.yml`,
> clearly labelled as a draft pending that enablement.

## Why this exists

This fork has **no automated CI gate running today** — not because the workflows
are absent, but because the inherited ones no-op here. The 40+ upstream Zed
workflows in `.github/workflows/` (`run_tests.yml`, `release.yml`,
`release_nightly.yml`, `run_bundling.yml`, …) come from `zed-industries/zed`, and
their jobs are guarded by an **owner gate**:
`if: (github.repository_owner == 'zed-industries' || github.repository_owner == 'zed-extensions')`
(e.g. `run_tests.yml` line 19, repeated on every job). `run_tests.yml` *does*
trigger on `pull_request` (line 10), but on this fork — owner `DylanMcCavitt` —
the condition is false, so every job is skipped and nothing actually runs.
They also assume upstream's runners, caches, and secrets. Relying on them as a
merge gate here would be a fiction.

So the only proof a change actually compiles, lints clean, and passes its tests
is a **targeted local run** performed by the author before merge. This record
documents that gate precisely so every PR is verified the same way, and so the
discipline survives review and hand-off between agents.

## The enforced local gate

For **every crate a PR changes**, the author runs all three of the following and
records the exact commands plus their results in the PR body. (`zed` is the
crate name; `ompzed` is its primary binary — see
`crates/zed/Cargo.toml` `[[bin]] name = "ompzed"`.)

1. **Targeted tests for the changed crate**

   ```bash
   cargo test -p <crate> [<filter>]
   ```

   Run the tests that cover the change, scoped to the crate with `-p`. Use a
   name filter when only specific tests are relevant. Do not run the whole
   workspace test suite as a gate — it is slow and mostly unrelated to the
   change.

2. **Lints for the changed crate (debug, per-crate)**

   ```bash
   cargo clippy -p <crate> --all-targets -- -D warnings
   ```

   **Debug, not release, and per-crate, not workspace** — this is deliberate.
   The repo's own `script/clippy` forces `--release --all-targets --all-features`
   across the whole workspace (`script/clippy` line 10), which is a multi-GB,
   long-running build that is impractical to run on every change. The practical,
   enforced gate is a per-crate **debug** clippy with warnings denied; it catches
   the same lint violations on the changed code without the release-build cost.
   Run the heavier `script/clippy` only when a change specifically warrants a
   release-mode or whole-workspace lint pass.

3. **The product binary still builds**

   ```bash
   cargo build -p zed --bin ompzed
   ```

   This is the integration check: a per-crate change can still break the binary
   that links it. Building `ompzed` proves the workspace still produces a
   shippable editor after the change.

A change touching multiple crates runs steps 1 and 2 for each changed crate, and
step 3 once.

### What every PR body must record

Until GitHub Actions is enabled on the fork, the PR description is the record of
proof. Each PR body **must** include, verbatim:

- the exact `cargo test`, `cargo clippy`, and `cargo build` commands that were
  run (with the `-p <crate>` / filters actually used), and
- their results (pass/fail, and the relevant tail of output for any failure that
  was then fixed).

A PR without this recorded local-gate evidence is not ready to merge.

## Non-goal

This policy intentionally does **not** reproduce the upstream Zed CI surface:

- the full **cross-platform matrix** (Linux/macOS/Windows runners),
- **release / nightly bundling, signing, and notarization**,
- workspace-wide release-mode clippy, license checks, `cargo machete`, miri,
  WASM checks, and the other jobs in `run_tests.yml`.

Those belong to a later, infrastructure-dependent effort once Actions is enabled
and the fork's release pipeline is owned. The gate here is the minimum that
proves a change is correct for merge into `main`; it is not a substitute for that
broader matrix.

## Relationship to the draft workflow

`.github/workflows/ci.yml` is a **minimal draft** (single Ubuntu job: checkout,
pinned toolchain from `rust-toolchain.toml`, build `ompzed`, one small test). It
is committed but framed as a draft because enabling it is the human-owned step
described in the Status block above. When Actions is enabled, that workflow — or
an evolution of it — should encode the local gate above so the proof becomes
automated rather than author-attested. Until then, **the local gate is the
gate.**
