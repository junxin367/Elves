# Elves

<p align="center"><img src="docs/images/icon.png" alt="Elves icon" width="128"></p>
<p align="center"><a href="README.md">中文</a> | English</p>

Elves is the desktop client for Codex and Claude conversations, providers, and the local protocol proxy. It opens its local workspace automatically. Harness is the independent execution and management product behind that workspace: Codex runs through an independent app-server, and Claude uses the selected instance's native CLI. Remote sessions are one Harness capability for browsers, PWAs, and Android; disabling them does not affect Elves or the local proxy. Codex App injection has been removed.

[Downloads](https://github.com/junxin367/Elves/releases) · [Windows client](docs/windows-remote-client.md) · [Mobile guide](docs/remote-access-mobile.md)

## Navigation

The 52px left rail uses 36px icon buttons with 8px on each side. Hover displays each name. Existing project, Codex, and Claude icons remain in use.

| Page | Features |
| --- | --- |
| Conversations | Existing conversation list, projects, and chat workspace; switching pages preserves the current conversation and draft |
| Proxy settings | Provider and model configuration, protocol selection, local proxy status, request logs, and details |
| Conversation management | Browse and filter local sessions; delete individual sessions or a filtered batch; historical repair and compression settings are removed |
| Remote sessions | Web service, public routes, access keys, login lock, and device authorization |
| Bottom settings | General, appearance, instances, models, reasoning depth, session naming, permissions, Skills, MCP, and hooks |

On Windows, the bottom settings button opens settings directly. Other clients show only Settings and Log out. Appearance is stored per client; other preferences are shared through the same service. Stale saves are rejected, preserving unsaved input for review.

## Install and start

Download `Elves-<version>-windows-x64-setup.exe` or the portable `Elves-<version>-windows-x64.exe`. Microsoft Edge WebView2 Runtime is required. The server needs a configured Codex CLI or Claude instance.

The installer deploys one application, `elves.exe`, plus its uninstaller. The same executable hosts Harness in the background. Upgrades remove the previous manager, launcher, task-board application, and their shortcuts while preserving user data.

1. Open **Elves**. It starts local Harness and opens the workspace without a server address or access key.
2. Desktop authorization is independent from remote devices, keys, read-only policy, and the remote-session switch.
3. Configure providers and models in **Proxy settings**. An enabled provider's local proxy starts automatically with Elves; its default port remains `45221`.
4. Return to **Conversations**. Configure public access and device authorization in **Remote sessions** to connect from another device.

Closing the main window minimizes it to the tray and keeps Harness running. Exiting from the tray safely shuts down the backend owned by this client.

## Data and updates

Shared settings, credentials, authorization, and logs remain under `~/.codex-session-delete/`. Window state and WebView2 data use `%LOCALAPPDATA%\com.elves.app\`, with compatibility for the previous data directory. Elves does not save or restore remote connection addresses. Uninstalling preserves user data.

The client embeds web resources and verifies updates with SHA-256. Complete updates take effect on the next navigation. Native upgrades use the Windows installer and briefly close the client and its backend; the former manager-only hot-update procedure does not support this installation structure.

Responses API requests use their configured upstream directly. Chat Completions and Anthropic conversions remain available. Model access and reasoning depth depend on the selected instance and provider. Session deletion is permanent and retains native execution and pending-write protections.

## Development

```powershell
npm --prefix apps/elves-management ci
npm --prefix apps/elves-management run check
npm --prefix apps/elves-management run vite:build
cargo fmt --all -- --check
cargo check --workspace
.\scripts\build-windows-installer.ps1
```

`apps/elves/` builds the only desktop application. `crates/elves-harness/` contains Harness execution and management, `elves-core` provides configuration and protocol proxying, and `elves-data` provides session storage access. `apps/elves-management/` contains shared web management components; it no longer builds a desktop manager. Commit generated `assets/harness/management/` alongside its source. The Windows build does not build Android. Android versions and signed releases remain independent.

See the [product standards](docs/product-standards.md), [web architecture](docs/harness-web-architecture.md), and [release documentation](docs/android-release.md) for the detailed contracts.
