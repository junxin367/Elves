# Elves

<p align="center"><img src="assets/harness/icons/elves-logo.svg" alt="Elves icon" width="128"></p>
<p align="center"><a href="README.md">中文</a> · English</p>

**One workspace for Codex, Claude, projects, conversations, and a local protocol proxy.**

Elves is a Windows desktop client. Opening it starts local Harness and enters the workspace without a remote address or access key. Harness independently handles execution and management: Codex uses its own app-server, and Claude uses the selected WSL instance's native CLI.

Browsers, PWAs, and Android connect through Harness's optional remote-session capability. Turning it off leaves desktop use and the local proxy available. Elves does not require Codex App and provides no page injection, desktop execution bridge, or separate manager application.

[Releases](https://github.com/junxin367/Elves/releases) · [Documentation](docs/README.md) · [Issues](https://github.com/junxin367/Elves/issues)

## Features

| Page | Features |
| --- | --- |
| Conversations | Browse projects and instances, continue conversations, choose models and reasoning depth, handle queues and approvals, and inspect files and changes |
| Proxy settings | Configure providers, models, protocols, and the local proxy; inspect status, request logs, and details |
| Conversation management | Browse, filter, and delete local conversations individually or in batches |
| Remote sessions | Configure web access, public routes, access keys, and device authorization |
| Settings | Manage instances, model defaults, session naming, permissions, instructions, Skills, MCP, hooks, and appearance |

Session naming uses one shared execution instance and model across the same Harness. Appearance is local to each client; other settings are shared through Harness. Drafts, attachments, and reading positions remain local. Model, reasoning, and extension capabilities depend on the selected native instance.

## Quick start

Requirements: Windows x64, Microsoft Edge WebView2 Runtime, and a configured Codex CLI or WSL Claude instance. Custom providers additionally need a Base URL, API key, and model configuration.

Check [Releases](https://github.com/junxin367/Elves/releases) for published packages, or build from source:

| File | Purpose |
| --- | --- |
| `Elves-<version>-windows-x64-setup.exe` | Installer containing one Elves application and its uninstaller |
| `Elves-<version>-windows-x64.exe` | Portable client |

1. Open **Elves** and wait for local Harness to prepare the workspace.
2. Open bottom **Settings**, detect the intended instances, and configure model defaults, reasoning depth, and permissions.
3. Return to **Conversations**, select an instance and project, then create or continue a conversation.
4. For a custom provider, save its configuration in **Proxy settings** and select **Use**.

When the active provider has its local proxy enabled, it starts automatically with Elves / Harness on default port `45221`. Responses API uses the configured Responses upstream directly; Chat Completions and Anthropic use their respective protocol conversions. Each model retains its protocol assignment.

Closing the main window minimizes it to the tray. Exiting from the tray safely shuts down the backend owned by this client. The Windows bottom settings button opens settings directly.

## Web and mobile access

Enable web access in **Remote sessions**, save an access key, and configure an address reachable from the other device. Browsers and PWAs sign in with that address and key. Android connects to the same Harness and has independent application versions and releases.

The local workspace defaults to `127.0.0.1:45223`; remote web access defaults to port `45222`. Desktop authorization is independent of remote devices and policies. Disabling remote access, changing its read-only policy, or revoking a remote device does not sign out the local desktop client. Keep the Harness computer running while using remote access.

See the [mobile guide](docs/remote-access-mobile.md) and [remote-session reference](docs/remote-access.md).

## Data and upgrades

- Shared settings, provider configuration, logs, remote keys, and device authorization retain the existing `~/.codex-session-delete/` directory.
- Native configuration and history remain with their respective Codex / Claude instance. Window state and WebView2 cache use the client's own data directory.
- Upgrade with a new Windows installer. Installation removes obsolete application entries while preserving user data; uninstalling also preserves settings, history, and login cache.
- When an operation's outcome is unknown, retain its receipt and check the conversation before repeating a send or deletion.

## Development

Install Node.js/npm, the Rust MSVC toolchain, Windows C++ build tools, and NSIS for the installer.

```powershell
npm --prefix apps/elves-management ci
npm --prefix apps/elves-management run check
cargo fmt --all -- --check
cargo check --workspace --locked
.\scripts\build-windows-installer.ps1
```

The installer script builds shared management assets, the client, installer, and portable executable under `dist/windows/`. It does not build Android. For a client-only build:

```powershell
npm --prefix apps/elves-management run vite:build
.\scripts\build-windows-client.ps1
```

`apps/elves/` contains the only Windows application; `crates/elves-harness/` contains Harness execution and management. `elves-core` and `elves-data` provide configuration, protocol proxying, and session data access. Workspace assets live in `assets/harness/`. Commit `assets/harness/management/` alongside changes to `apps/elves-management/`. Android lives in `apps/elves-android/`, with independent versions and signing.

See the [documentation index](docs/README.md), [product standards](docs/product-standards.md), and [Elves / Harness architecture](docs/harness-architecture.md).
