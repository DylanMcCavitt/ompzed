---
title: Ompzed Fork Strategy (North Star)
description: "The load-bearing decision record for how Ompzed relates to upstream Zed: an agent-native layer on a vendored Zed core, add-first, with divergence kept cheap to rebase and cheap to harden into a standalone fork later."
---

# Ompzed Fork Strategy — North Star

> **Status:** Internal engineering decision record. Not part of the published
> end-user book; intentionally omitted from `SUMMARY.md` (like the other
> `docs/src/omp/` records).
>
> This is the doctrine future work — human or agent — routes against. The
> running tally of what we've diverged is in [`divergence-ledger.md`](./divergence-ledger.md).

## Decision

Ompzed is an **agent-native layer built on a *vendored* Zed core**, with the
Oh My Pi (OMP) harness as the first-class agent. Models stay configurable
(bring-your-own providers). We **add** our flavor, **gate** Zed's competing
surfaces, and keep the divergence cleanly separable so that — in the off chance
this grows into a product — a hard fork ("branch from it") is a *cheap pivot,
not a rewrite*.

We track upstream Zed **opportunistically** — rebase when an improvement earns
it — not continuously.

## Why this shape (grounded)

### What "built on top" actually means — the Cursor model
Cursor did **not** walk away from VSCode. Anysphere runs a *dedicated team*
that perpetually rebases onto VSCode, deliberately lagging a few versions for
stability, and diverges in the **core** only where deep AI integration demanded
it (Shadow Workspace, speculative edits — impossible as plugins). They keep
paying that maintenance tax because VSCode's **extension ecosystem** is the user
lock-in. Lesson: "built on top" = *built deeply on top **and** never stopped
rebasing* — they kept the editor and spent their effort on the AI layer.

### Two disanalogies that shape Ompzed
1. **License.** VSCode is **MIT** — Cursor could close-source it and build a
   ~$29B *proprietary* business. Zed is **GPL-3.0 (copyleft)** — Ompzed must
   stay open-source. The Cursor *business model* is unavailable on this base; a
   future commercial play monetizes the **OMP service/cloud**, not a closed
   client (Zed itself is open + commercial — same playbook).
2. **The base is a competitor.** Microsoft doesn't ship an AI-native VSCode that
   competes with Cursor. Zed **does** ship its own first-class agent — the exact
   category we differentiate against. So tracking upstream Zed carries a
   *recurring re-demotion tax* (this is literally what AGE-678/679/681/682 did).
   Cursor never paid this.

### Removal is the tax (Zedless / GRAM)
Zedless (privacy/local-first) and GRAM (AI-removed) are **removal-first** Zed
forks. Both lag upstream "due to the complexity of selectively removing features
while preserving core functionality"; Zedless is even building tree-sitter
**AST auto-patching** to re-apply its edits each rebase. Removing/reshaping Zed
is the expensive part; **additions are free.** Ompzed is **add-first** (we put a
better agent *on top*), which is the structurally healthy shape — and the Cursor
shape, the one that scaled.

## The lane doctrine (the operating rule)

Sort every change to the tree by its rebase cost, and default to the cheapest
lane that does the job:

| Lane | Change | Rule | Rebase cost |
|---|---|---|---|
| **1 — ADD** | new OMP-owned crates/files | **Default here.** All "flavor" lives here. | ~zero (never conflicts; already separable for a hard fork) |
| **2 — GATE** | remove/disable a Zed feature | Prefer **gate-off-by-default over deletion**; do it at a **single chokepoint**, not scattered edits. | cheap *if centralized*; compounds as Zed ships more to suppress |
| **3 — EDIT-CORE** | change Zed's own logic | Use Zed's **seams** — ACP, panel/dock registration, the mention/context system, settings, actions. Touch core only as a genuine last resort. | expensive + compounding |

The same discipline that keeps *rebasing* cheap is what keeps a future
*hard fork* cheap — both want divergence concentrated in lane 1 and minimized in
lanes 2–3.

## When to "branch from it"

The honest trigger to snapshot + harden into a standalone fork is **not** "when
it blows up." It's:

> when the per-rebase reconciliation cost exceeds the editor improvements gained
> from the pull.

Track that signal in the divergence ledger; don't guess it.

## Consequences / rules of thumb

- **New surfaces are additive + seam-based.** Worked example: the GitHub context
  bridge belongs as `@`-mention agent context + an **OMP-owned panel** (lane 1),
  **never grafted into `git_panel.rs`** (lane 3 — a ~9.9k-line, high-churn
  upstream file).
- **Removals become gates at one chokepoint.** Good pattern: the `zed.dev`
  provider gated behind `OMPZED_ENABLE_ZED_CLOUD` (AGE-682). Fragile pattern:
  scattered deletions (AGE-684) — minimize and centralize these.
- **GPL is permanent.** Keep `LICENSE` + copyright notices; never close-source;
  the moat is services, not the client binary.
- **Candidate capability — OMP maintains its own fork.** We have an agent and AST
  tooling; the highest-leverage move is agent-driven re-application of our
  gates/removals on each Zed rebase (cf. Zedless's AST auto-patching). That turns
  the maintenance tax into an agent task.

## References
- [`divergence-ledger.md`](./divergence-ledger.md) — the running rebase-debt tally
- [`distribution-identity.md`](./distribution-identity.md) — what's rebranded vs retained (GPL provenance)
- [`extension-capability-audit.md`](./extension-capability-audit.md) — panel vs fork-native boundary
- Epic **AGE-637** "OMP Native Zed"
