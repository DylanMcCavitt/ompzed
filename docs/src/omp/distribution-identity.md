---
title: Ompzed Distribution Identity
description: "Record of which distribution-time identity surfaces were rebranded to Ompzed, and which stock-Zed strings are intentionally retained (with rationale)."
---

# Ompzed Distribution Identity

> **Status:** Internal engineering decision record (AGE-660). Not part of the
> published end-user book; intentionally omitted from `SUMMARY.md`.
>
> Completes the distribution-time identity surfaces deferred from AGE-651
> (app identity & product shell). Canonical identity: product name **Ompzed**,
> app id `dev.ompzed.Ompzed[-Dev|-Nightly|-Preview]`, repo
> `github.com/DylanMcCavitt/ompzed`.

## Why this exists

A shipped artifact must not pull stock-Zed identity or, worse, upstream Zed
**release binaries**. This record lists every distribution surface that still
carried stock identity, what it became, and — for the surfaces that cannot be
rebranded without infrastructure that does not yet exist (a release server, a
docs host, a signing certificate) — why the stock string is intentionally
retained.

## Rebranded surfaces

| Surface | File | Change |
|---|---|---|
| Windows app identifier | `crates/release_channel/src/lib.rs` `app_identifier()` | `Zed-Editor-*` → `Ompzed-*` |
| Finder doc-type label | `crates/zed/resources/info/DocumentTypes.plist` | `Zed Text Document` → `Ompzed Text Document` |
| Linux crash-notification id + title | `crates/zed/src/main.rs` | `dev.zed.Oops` → `dev.ompzed.Oops`; "Zed failed to launch" → "Ompzed failed to launch" |
| Snap package | `crates/zed/resources/snap/snapcraft.yaml.in` | name/title/summary/description/links → Ompzed; `common-id` → `dev.ompzed.Ompzed`; `source:` repointed to the fork's releases (no longer pulls an upstream binary) |
| Linux/macOS installer | `script/install.sh` | header + echoes → Ompzed; appids → `dev.ompzed.Ompzed*`; **upstream `cloud.zed.dev` download removed** — installs only from a locally built `ZED_BUNDLE_PATH` |
| Linux/macOS uninstaller | `script/uninstall.sh` | header/prompt/echo → Ompzed; appids → `dev.ompzed.Ompzed*`; macOS bundle `Zed*.app` → `Ompzed*.app`; data dirs → `Application Support/Ompzed`, `Logs/Ompzed`, `~/.local/share/ompzed` (APP_NAME-keyed) |
| Windows Inno Setup | `crates/zed/resources/windows/zed.iss` | `AppPublisher` → Ompzed; publisher/support/updates URLs → the fork |
| Explorer command injector (Appx) | `crates/explorer_command_injector/AppxManifest{,-Preview,-Nightly}.xml` | `DisplayName`, `PublisherDisplayName`, app `DisplayName`/`Description`, `SurrogateServer DisplayName` → Ompzed |
| Windows version-info resource | `crates/windows_resources/src/windows_resources.rs` | `ProductName`/`FileDescription` → Ompzed*; `CompanyName` → Ompzed |
| Auto-update default | `assets/settings/default.json` | `auto_update` default flipped **`true` → `false`** so a packaged Ompzed never checks zed.dev for updates |
| Release-notes URLs | `crates/auto_update/src/auto_update.rs` | Nightly/Dev commit URLs repointed from `zed-industries/zed` to `DylanMcCavitt/ompzed` |

## No upstream binary pull

The hard constraint — *no shipped installer/package pulls upstream Zed release
binaries* — is met by:

- **`install.sh`**: the `cloud.zed.dev/releases/...` download is removed on both
  Linux and macOS. Installation requires a locally built bundle via
  `ZED_BUNDLE_PATH`; absent it, the script errors with guidance.
- **`snapcraft.yaml.in`**: `source:` points at the Ompzed fork's release assets,
  not `zed-industries/zed`. (The fork must publish that artifact.)
- **`auto_update`**: defaulted to `false`, so the in-app updater does not call
  the default `server_url` (`https://zed.dev`) at runtime.

## Intentionally retained (with rationale)

| Retained string | Where | Why |
|---|---|---|
| `ZED_DOCS_URL = https://zed.dev/docs` | `crates/release_channel/src/lib.rs` | Ompzed has no docs host; Zed's docs remain the closest reference. Re-point when an Ompzed docs site exists. |
| `server_url` default `https://zed.dev` | `assets/settings/default.json` | Deployment/collab infrastructure URL, also used by the cloud LLM provider. Neutralized for updates by `auto_update = false`; not an installer binary URL. |
| Auto-update cloud endpoint path | `crates/auto_update/src/auto_update.rs` | Derives the asset URL from the client cloud-server URL (deployment infra), not a hardcoded installer URL; inert while `auto_update = false`. |
| `Zed.exe`, `Application Id="Zed*"`, `zed_explorer_command_injector.dll`, linux `zed*.app`, `usr/bin/zed`, macOS `Contents/MacOS/cli`, `zed-<channel>.sock` filename | installers, `zed.iss`, Appx manifests, snap | **Build-output / hardcoded** names produced by the bundling pipeline. Renaming requires changing the build outputs, not these consumers. |
| `Identity Name="ZedIndustries.Zed*"`, `Publisher="CN=Zed Industries…"` | Appx manifests | Bound to the code-signing certificate. macOS/Windows signing is a separate, deferred non-goal. |
| `LegalCopyright "Copyright … Zed Industries, Inc."` | `crates/windows_resources/src/windows_resources.rs` | GPL-fork provenance; the upstream copyright is retained by license. |
| `.zed_server` | `script/uninstall.sh` | Hardcoded remote-host server directory (`paths::remote_server_dir_relative`), not APP_NAME-keyed. |
| `zed://` URL scheme / protocol ids | `zed.iss`, bundle config (`osx_url_schemes`) | Explicit non-goal (AGE-660 / AGE-651): the scheme is intentionally kept. |

## Follow-ups requiring infrastructure (HITL)

- Stand up an Ompzed **release server** (or keep `ZED_BUNDLE_PATH`-only installs)
  and an Ompzed **docs host**, then re-point `server_url` / `ZED_DOCS_URL` and
  consider re-enabling `auto_update`.
- macOS/Windows **code signing & notarization** (separate tracked work) — once a
  signing identity exists, update the Appx `Identity`/`Publisher`.
- Publish the snap release artifact referenced by `snapcraft.yaml.in`.
