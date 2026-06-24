---
title: Ompzed Divergence Ledger
description: "The running tally of every edit and removal Ompzed makes against the vendored Zed core — i.e. the rebase debt. Additions (lane 1) carry ~no rebase debt and are inventoried separately."
---

# Ompzed Divergence Ledger

> **Status:** Internal engineering decision record. Omitted from `SUMMARY.md`.
>
> Living record of every change against the vendored Zed core, classified by the
> lane doctrine in [`fork-strategy.md`](./fork-strategy.md). **Update this on
> every lane-2 / lane-3 change.** On each `git fetch upstream`, the lane-2/3 rows
> are the surfaces to review for conflicts — review the **HIGH** rows first.

## Lane 2 / 3 — rebase debt

| File / area | Lane | What we changed | Issues | Rebase risk |
|---|---|---|---|---|
| `agent_ui/src/agent_panel.rs` | 2+3 | Demote native Zed agent in the picker (gate to collab); remove onboarding card + trial upsell renders; wire OMP views | AGE-678 / 679 / 684 | **HIGH** — busiest agent-UI file upstream |
| `agent_ui/src/conversation_view/thread_view.rs` | 3 | Wire OMP views (GitHub / Linear / terminal / subagent tree) into the render chain + fields | AGE-645 / 646 / 648 / 649 / 664 | **MED-HIGH** — churny view file |
| `acp_thread/src/acp_thread.rs`, `connection.rs` | 3 | ACP thread extensions: approval / input / select round-trip, session lifecycle, context plumbing | AGE-640 / 641 | **MED-HIGH** — core ACP crate |
| `language_models/src/language_models.rs` | 2 | Gate the `zed.dev` cloud provider behind `OMPZED_ENABLE_ZED_CLOUD` (off by default) | AGE-682 | LOW-MED — clean single-chokepoint gate (the model pattern) |
| `assets/settings/default.json` | 2 | Edit-prediction provider → `none`; `auto_update` → false; add `omp` settings block | AGE-681 / 660 / 642 | LOW-MED — localized keys |
| `zed_actions/src/lib.rs` | 2 | Removed `agent::ResetOnboarding` action | AGE-684 | LOW-MED — cross-crate removal |
| `agent_ui/src/ui/end_trial_upsell.rs` (deleted) + `ui.rs` | 2 | Removed the trial-end upsell module + its `mod` decl | AGE-684 | LOW |
| `agent_ui/src/agent_ui.rs` | 1+2 | OMP `mod` decls (add) + removed upsell action defs | AGE-643 / 645 / 684 | LOW |
| `agent_servers/src/agent_servers.rs`, `Cargo.toml` | 2 | Export OMP modules; add `http_client`/`indoc` test-support deps | AGE-645 + | LOW |
| `settings_content/src/agent.rs`, `settings_content.rs` | 1+2 | `OmpSettings` fields (add) + wiring | AGE-642 / 648 | LOW |
| `settings/src/settings_store.rs`, `vscode_import.rs` | 2 | `auto_update` default test fix; minor | AGE-660 | LOW |
| `release_channel/src/lib.rs` | 2 | `app_identifier()` → `Ompzed-*`; `ZED_DOCS_URL` retained (documented) | AGE-660 | LOW — rebrand |
| `zed/src/main.rs` | 2 | Crash id → `dev.ompzed.Oops`; launch-failure copy | AGE-660 | LOW — rebrand |
| `auto_update/src/auto_update.rs` | 2 | Release-notes URLs → fork repo; auto-update off | AGE-660 | LOW — rebrand |
| `windows_resources/src/windows_resources.rs`, `explorer_command_injector/AppxManifest*.xml` (×3), `zed/resources/windows/zed.iss`, `zed/resources/info/DocumentTypes.plist`, `zed/resources/snap/snapcraft.yaml.in` | 2 | Windows / macOS / snap branding → Ompzed; snap stops pulling upstream binaries | AGE-660 | LOW — rebrand, spread across many files |
| `script/install.sh`, `uninstall.sh` | 2 | Rebrand + neutralize upstream binary download (`ZED_BUNDLE_PATH` path) | AGE-660 | LOW-MED |

## Lane 1 — additions (OMP-owned, ~no rebase debt)

These are new (or modified-in-place but OMP-owned) files; they don't conflict on
rebase and are already cleanly separable for a future hard fork.

- `agent_servers/src/omp.rs` — the OMP ACP agent server *(modified-in-place; OMP-owned)*
- `agent_servers/src/{github_context.rs, linear_context.rs, omp_terminal.rs}`
- `agent_ui/src/{github_context.rs, linear_context.rs, omp_terminal.rs, subagent_tree.rs, ui_request_prompt.rs}`
- `docs/src/omp/*.md`
- `.github/workflows/ci.yml` (draft)

## Notes
- Debt concentrates in **`agent_panel.rs` + `thread_view.rs` + `acp_thread`** — review these first on every upstream pull; everything else is rebrand/config and rarely conflicts.
- The pending GitHub-bridge reframe (`@`-mention context + OMP-owned panel) must stay **lane 1** — do not migrate it into `git_panel.rs`.
- As Zed ships more agent features, lane-2 gate surface grows. When reconciling them costs more than the editor gains from a pull, that's the "branch from it" trigger (see `fork-strategy.md`).
