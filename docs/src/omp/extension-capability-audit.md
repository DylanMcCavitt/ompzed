---
title: OMP Extension Capability Audit
description: "Source-backed decision record: which OMP surfaces ship as a Zed extension vs a fork-native crate, and the required privilege boundary for each."
---

# OMP Extension Capability Audit

> **Status:** Internal engineering decision record (AGE-644 / ZED-07). Not part of the
> published end-user book; intentionally omitted from `SUMMARY.md`.
>
> **Deferred HITL (NOT in this change):** architecture and security signoff is
> required on this boundary **before any panel issue is implemented**
> (AGE-649, AGE-646, AGE-647, AGE-650, AGE-648, AGE-645, AGE-643).

## Why this exists

Every later OMP surface must land in one of two lanes:

- **Zed extension** — a WASM component, loaded by `extension_host`, running inside a
  WASI sandbox and reaching the host only through the capability-checked WIT API.
- **Fork-native crate** — Rust compiled into the Ompzed binary with full in-process
  access to GPUI, the workspace, the agent thread model, credentials, and the OS.

Choosing wrong is expensive: a surface routed to an extension that the sandbox cannot
actually serve has to be rebuilt fork-native later. This document grounds every verdict
in the extension API source so the decisions survive review.

The audit answers two questions per surface:

1. **Lane** — extension or fork-native?
2. **Privilege boundary** — what the implementation is allowed to touch, and what it
   must route through existing host infrastructure instead of re-implementing.

## What the extension sandbox actually is

Extensions are WASM components. The host builds their WASI context with a **single
preopened directory** — the per-extension scratch work dir — and nothing else; there is
no raw socket, no project filesystem, and no inherited credentials
(`crates/extension_host/src/wasm_host.rs` → `WasmHost::build_wasi_ctx`, lines ~729-751:
`ctx.preopened_dir(&path, ".", …)` over `self.work_dir.join(manifest.id)`).

Everything an extension can do beyond that scratch dir is an explicit WIT import in the
`extension` world (`crates/extension_api/wit/since_v0.8.0/extension.wit`, world `extension`,
lines 3-17): `context-server`, `dap`, `github`, `http-client`, `platform`, `process`,
`nodejs`. The exported entry points an extension may implement are the methods of the
`Extension` trait (`crates/extension_api/src/extension_api.rs`, lines 68-284): language
servers, LSP label formatting, slash commands, context servers (MCP), `/docs` indexing,
and debug adapters/locators.

There is **no** panel, view, render, webview, terminal/pty, keychain, or agent-thread
interface anywhere in that world or trait. That single fact decides most OMP surfaces.

Three host functions are guarded by the manifest capability allow-list; the rest are not.
The gate is `CapabilityGranter` (`crates/extension_host/src/capability_granter.rs`):
`grant_exec`, `grant_download_file`, `grant_npm_install_package`. Capabilities are
declared in `extension.toml` and modelled by `ExtensionCapability`
(`crates/extension/src/capabilities.rs`, lines 14-20) and can be further narrowed by the
user via `granted_extension_capabilities` (`docs/src/extensions/capabilities.md`).

## Capability matrix

For each capability: can a sandboxed extension do it, is a fork-native crate required for
the OMP use, and the source that proves the verdict (`file:symbol`).

| Capability | Extension-allowed? | Fork-native required? | Source (`file:symbol`) |
|---|---|---|---|
| **Rich panels / dockable UI / custom views** | **No** — no UI surface exists in the WIT world or trait | **Yes** | `crates/extension_api/wit/since_v0.8.0/extension.wit:world extension` (no panel/view import); `crates/extension_api/src/extension_api.rs:Extension` (no render/view method) |
| **Webview / embedded browser** | **No** | **Yes** | `crates/extension_api/wit/since_v0.8.0/extension.wit:world extension` (no webview interface) |
| **Interactive terminal / PTY / streaming stdin** | **No** — only run-to-completion | **Yes** | `crates/extension_api/wit/since_v0.8.0/process.wit:run-command` (returns final `output`, no pty/stream) |
| **Subprocess spawn (one-shot, capture output)** | **Yes — capability-gated** (`process:exec`) | No (host can also do it, unrestricted) | `crates/extension_api/src/process.rs:Command::output`; gate `crates/extension_host/src/wasm_host/wit/since_v0_8_0.rs:run_command`→`grant_exec` (line ~896); `crates/extension/src/capabilities/process_exec_capability.rs:ProcessExecCapability::allows`; `crates/extension/src/extension_manifest.rs:ExtensionManifest::allow_exec` |
| **HTTP egress (arbitrary host)** | **Yes — UNGATED** (no capability check) | No | `crates/extension_api/wit/since_v0.8.0/http-client.wit:fetch`; `crates/extension_host/src/wasm_host/wit/since_v0_8_0.rs:http_client::Host::fetch` (lines 629-645) calls `self.host.http_client.send` with **no `capability_granter` call** — contrast `download_file` |
| **File download to disk + extract** | **Yes — capability-gated** (`download_file`) | No | `crates/extension_api/wit/since_v0.8.0/extension.wit:download-file`; gate `crates/extension_host/src/wasm_host/wit/since_v0_8_0.rs:grant_download_file` (line ~1070); `crates/extension/src/capabilities/download_file_capability.rs:DownloadFileCapability::allows` |
| **Keychain / stored credentials (e.g. GitHub token, API keys)** | **No** — only plaintext declared settings | **Yes** | `crates/extension_api/wit/since_v0.8.0/extension.wit:world extension` (no credential import); settings read-only via `get-settings` (extension.wit line 51) |
| **Durable workspace / project state** | **No** — insert-only KV scoped to `/docs` indexing + ephemeral scratch dir only | **Yes** | `crates/extension_api/wit/since_v0.8.0/extension.wit:key-value-store` (insert-only, passed solely to `index-docs`, lines 87-91/170); `crates/extension_host/src/wasm_host.rs:build_wasi_ctx` (single preopened scratch dir) |
| **Worktree / project file read (read-only text)** | **Yes** | No | `crates/extension_api/wit/since_v0.8.0/extension.wit:worktree` (`read-text-file`, `which`, `shell-env`, lines 68-79); `project` (`worktree-ids`, lines 82-85) |
| **MCP / context-server registration** | **Yes** (extension supplies launch command + config; server runs as a host subprocess) | No | `crates/extension_api/src/extension_api.rs:Extension::context_server_command` / `context_server_configuration` (lines 183-198); `crates/extension_api/wit/since_v0.8.0/context-server.wit`; manifest `context_servers` (`crates/extension/src/extension_manifest.rs:ExtensionManifest.context_servers`) |
| **Slash commands (text-producing, sandboxed)** | **Yes** — output is text + sections, no panel/thread access | Depends on integration (see verdicts) | `crates/extension_api/src/extension_api.rs:Extension::run_slash_command` / `complete_slash_command_argument` (lines 163-180); `crates/extension_api/wit/since_v0.8.0/slash-command.wit`; manifest `slash_commands` |
| **Agent thread / transcript access** | **No** — the ACP thread model is host-only | **Yes** | `crates/acp_thread` (no extension binding); `crates/extension_api/wit/since_v0.8.0/extension.wit:world extension` (no thread interface) |
| **GitHub API** | **Release metadata only** via `github` interface; arbitrary GitHub HTTP only via ungated `fetch` (no auth) | **Yes** for an authenticated bridge | `crates/extension_api/wit/since_v0.8.0/github.wit:latest-github-release` / `github-release-by-tag-name` (releases only) |
| **External ACP agent server (the OMP agent itself)** | **Deprecated** for extensions | **Yes** (already shipped fork-native) | `docs/src/extensions/agent-servers.md` (ACP extensions deprecated as of v1.5.0, use the ACP Registry); fork precedent `crates/agent_servers/src/omp.rs:OmpAgentServer` (spawn lines 258-267, `--mode rpc-ui … --approval-mode`), registered as `crates/agent_ui/src/agent_ui.rs:Agent::Omp` / `agent_servers::OMP_AGENT_ID` |
| **Language servers / DAP / themes / languages / grammars / icon themes / snippets** | **Yes** — canonical extension lane (not an OMP surface) | No | `crates/extension/src/extension_manifest.rs:ExtensionManifest` (lines 98-118); `Extension` trait LSP/DAP methods |

### Two nuances that drive the verdicts

1. **HTTP `fetch` is ungated, but useless alone for a bridge.** An extension can reach
   any host over HTTP without declaring a capability. But it cannot read the user's
   stored GitHub/Linear credentials (no keychain), cannot render the result in a panel,
   and cannot inject it into the OMP agent thread. So "an extension that fetches Linear
   data" is technically possible yet cannot deliver any of the bridge surfaces on its own.
2. **The OMP runtime is already fork-native, and that is the supported direction.**
   Agent-server extensions are deprecated (`docs/src/extensions/agent-servers.md`); AGE-639
   shipped `OmpAgentServer` fork-native. Every surface that must read/write the OMP thread,
   render in the panel, or use stored credentials inherits that lane.

## Per-surface verdicts

| Surface | Lane | Privilege boundary |
|---|---|---|
| **GitHub context** | **Fork-native** | In-process; reuse Zed's existing git/GitHub credential provider (keychain-backed) — never re-store tokens; network egress restricted to `api.github.com`/`github.com`; results injected as OMP thread context via `crates/acp_thread`. Extension lane rejected: no keychain, no panel, no thread access. |
| **Linear context** | **Fork-native** | In-process; API key sourced from a secret Zed setting / keychain, never logged or echoed; egress restricted to `api.linear.app`; results injected as thread context. Extension lane rejected for the same reasons as GitHub. |
| **Session browser** | **Fork-native** | In-process; reads the OMP session/thread store (`crates/agent_ui`, `crates/acp_thread`); renders a dockable panel/list; local-only, no network egress. No extension API enumerates sessions or renders a list. |
| **MCP / skills** | **Hybrid (lane per piece)** | External standalone MCP servers → **extension** lane (`context_server_command`); the server runs as a host-launched subprocess declared in `extension.toml`. OMP's own skills/MCP status surfaced *in the OMP panel* → **fork-native** (panel + thread state). Boundary: extension-launched MCP servers honor user install consent; panel surfacing stays in-process. |
| **Terminal / tasks** | **Fork-native** | In-process; reuse Zed's terminal + task infrastructure; honor worktree-trust before spawning; subprocess inherits the workspace shell env. Extension `process:exec` is one-shot/run-to-completion only — it cannot back an interactive terminal. |
| **Browser / webview** | **Fork-native (hard requirement)** | Isolated webview process; navigation allow-list; page content treated as untrusted (no path from page → host process/credentials). No webview interface exists in the extension world. |
| **Transcript search** | **Fork-native** | In-process; reads the OMP transcript/thread store and renders results; local index; no network egress. No extension API can read agent transcripts. |

## Recommendation per later panel issue

Each downstream issue gets one assigned lane and a privilege boundary. These are the
inputs the deferred architecture/security signoff must ratify before implementation.

| Issue | Surface | Lane | Privilege boundary |
|---|---|---|---|
| **AGE-649** | GitHub bridge | **Fork-native** | In-process bridge crate. Authenticate with Zed's existing GitHub/git credentials (keychain) — do **not** introduce a second token store; egress allow-listed to GitHub hosts; surface results in the panel and inject into the OMP thread (`crates/acp_thread`). Network calls fail closed when unauthenticated. |
| **AGE-646** | Linear bridge | **Fork-native** | In-process bridge crate. Linear API key from a secret setting / keychain; never logged; egress allow-listed to `api.linear.app`; results injected as thread context. Mirror AGE-649's auth-and-egress discipline. |
| **AGE-647** | Browser boundary | **Fork-native** | Defines the trust boundary for embedded web content: isolated webview/renderer process, navigation/host allow-list, no script→host bridge that exposes credentials, OS, or arbitrary subprocess spawn. This boundary is a prerequisite for AGE-650. |
| **AGE-650** | Browser panel | **Fork-native** | Dockable GPUI panel hosting the webview defined by AGE-647; renderer isolation enforced; treat all loaded content as untrusted; no implicit filesystem/keychain reach from the page. |
| **AGE-648** | Terminal / tasks | **Fork-native** | Reuse Zed terminal + task runner; honor worktree-trust gating before any spawn; subprocess inherits workspace env; stream I/O through the existing terminal infra rather than `process:exec`. |
| **AGE-645** | Subagent telemetry | **Fork-native** | In-process; reads OMP subagent/thread state and persists metrics through Zed's telemetry/storage layer; respects the user's telemetry + privacy settings; no external egress beyond Zed's existing telemetry sink (off by default per settings). |
| **AGE-643** | Slash commands | **Fork-native (primary)** | OMP-panel slash commands must drive the OMP agent thread and may render structured results — neither is reachable from the WASM slash-command API, so they ship fork-native, invoking OMP/host capabilities directly. **Optional extension hybrid:** purely text-producing commands that need no panel/thread integration may use the sandboxed `run_slash_command` API (worktree-read + capability-gated egress); such commands stay inside the extension sandbox and never gain thread/credential access. |

## Summary

- **Fork-native:** GitHub context, Linear context, session browser, terminal/tasks,
  browser/webview, transcript search, subagent telemetry, and OMP-panel slash commands —
  all blocked from the extension lane by the absence of panel, webview, terminal/pty,
  keychain, and agent-thread interfaces in the extension API.
- **Extension-capable:** external MCP/context servers, and (optionally) text-only slash
  commands — the surfaces that fit the sandbox's gated process/HTTP/worktree model.
- **Cross-cutting rule:** any surface that must render in the panel, read/write the OMP
  thread, or use stored credentials is fork-native; the sandbox provides none of those.
