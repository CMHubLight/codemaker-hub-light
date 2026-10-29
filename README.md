# CodeMaker Hub Light

[![CI](https://github.com/CMHubLight/codemaker-hub-light/actions/workflows/ci.yml/badge.svg)](https://github.com/CMHubLight/codemaker-hub-light/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/CMHubLight/codemaker-hub-light)](https://github.com/CMHubLight/codemaker-hub-light/releases/latest)
[![License: GPL-3.0-only](https://img.shields.io/badge/license-GPL--3.0--only-blue)](LICENSE)
![Platform: Windows](https://img.shields.io/badge/platform-Windows-0078D4)

[中文文档](README.zh-CN.md) · [Documentation](docs/README.md)

A small Windows desktop app that gives you three things in one window: **account login**, a **local LLM proxy**, and **usage tracking**.

Sign in once, point Claude Code / Codex / any OpenAI- or Anthropic-compatible client at the local proxy, and every request is forwarded to your configured gateway with your account credentials injected — while tokens, cost and quota are collected locally.

> **Independent project.** This is a clean-room implementation and is not affiliated with, endorsed by, or derived from any vendor's product. It talks to a "compatible gateway" that you configure yourself; the repository ships no service endpoints.

```text
Claude Code ─┐
Codex        ├──→ http://127.0.0.1:15721 ───→ your gateway
OpenAI SDK  ─┘     (injects account credentials, records usage)
```

## What it does

- **Login** — system browser handles authentication, a deep link brings you back; if the browser cannot reach the app you can paste the callback link manually, or sign in with an account name + AuthKey directly from the login page (no browser). Credentials are encrypted at rest (AES-256-GCM, master key in the Windows Credential Manager) and renewed automatically. If the access token expires early, the app tries one `auth_key`-based renewal before treating the session as invalid; the settings page also exposes a manual "re-check / renew" action.
- **Local proxy** — listens on `127.0.0.1:15721` by default, falls back to `15722–15800` when the port is taken. Accepts Anthropic Messages, OpenAI Chat Completions and OpenAI Responses (streaming included). Client-supplied auth headers (`authorization`, `x-api-key`, `api-key`, `x-goog-api-key`, …) are stripped and replaced with the stored account credential. Only loopback `Host` is accepted, and requests carrying an `Origin` header are rejected with 403 — a browser page must not be able to read `/status` or your quota.
- **Usage** — per-request input/output tokens and cost, a 30-day local cost curve, a filterable request list with CSV export; plus account-level monthly quota, model distribution and a 24-hour curve fetched from the gateway.
- **Tray** — closing the window only hides it; the proxy keeps running. The tray tooltip shows this month's points.

**Not included:** agent orchestration, chat sessions, MCP/skill/plugin marketplaces, hooks, scheduled tasks, notifications, auto-update.

## Requirements

- Windows 10/11 x64 (the only validated platform; macOS/Linux are untested)
- A reachable **compatible gateway** (see [gateway contract](docs/gateway-contract.md))
- To build from source: Node.js ≥ 20.19, pnpm, Rust stable (MSVC), Visual Studio Build Tools + Windows SDK, WebView2 Runtime

## Build and run

```powershell
pnpm install
pnpm tauri dev                # dev: frontend on :5173 + Rust backend
pnpm tauri build              # NSIS installer
pnpm tauri build --no-bundle  # executable only
```

Artifacts:

```text
src-tauri/target/release/codemaker-hub-light.exe
src-tauri/target/release/bundle/nsis/CodeMaker Hub Light_0.2.0_x64-setup.exe
```

> Do **not** run `cargo build --release` on its own: Tauri needs the `custom-protocol` feature to embed the frontend. See [development notes](docs/development.md).

## First run

1. Start the app. On first launch it creates `%USERPROFILE%\.codemaker-hub-light\` with the SQLite database, `logs\` and `proxy.port`.
2. Open **Settings → Proxy & recording**, enter the base URL of a gateway you can reach, then save and click **Restart proxy**.
3. Click **Login** and finish in the browser. If the app does not come back by itself, expand *"Browser didn't return to the app?"* and paste the `codemakerhub-light://auth/callback?...` link.

The default gateway value (`https://api.example.com`) is a placeholder on purpose — nothing works until you configure a real one.

## Point your client at the proxy

Copy the **client base URL** from **Settings → Local proxy** (the port may differ from 15721 if it was taken). The API key is a placeholder: the proxy strips it and injects your account credential.

### Claude Code

```powershell
$env:ANTHROPIC_BASE_URL="http://127.0.0.1:15721"
$env:ANTHROPIC_AUTH_TOKEN="codemaker-hub-local"
claude
```

`/v1/messages` is translated to OpenAI Chat locally (some gateways reject native Anthropic reverse-proxying). Streaming and `tool_use` are supported; Anthropic-only fields such as `thinking`, `cache_control` and `documents` are dropped. `/v1/messages/count_tokens` returns a local estimate and never hits the gateway.

### Codex

```toml
# ~/.codex/config.toml
model_provider = "codemaker"

[model_providers.codemaker]
name = "CodeMaker Hub Light"
base_url = "http://127.0.0.1:15721/v1"
env_key = "OPENAI_API_KEY"
wire_api = "responses"
```

```powershell
$env:OPENAI_API_KEY="codemaker-hub-local"
codex
```

### OpenAI SDK

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:15721/v1", api_key="codemaker-hub-local")
resp = client.chat.completions.create(
    model="deepseek-flash",  # use an id returned by /v1/models
    messages=[{"role": "user", "content": "hi"}],
)
```

### Status codes

| Response | Meaning |
|---|---|
| `/health` 200 | Proxy is alive |
| `/v1/models` 503 | Not logged in yet, or the model catalog is not cached |
| forwarding 401 | Not logged in / credential expired — sign in again |
| 403 `origin_not_allowed` | Request carried an `Origin` header; use a CLI or SDK, not a web page |
| 404 | Path not supported |

Only requests that pass through the local proxy are recorded. Direct gateway calls are not.

## Data and privacy

Everything lives under `%USERPROFILE%\.codemaker-hub-light\`:

| Path | Content |
|---|---|
| `auth.bin` | AES-256-GCM encrypted credentials; master key in the Windows Credential Manager |
| `codemaker-hub-light.db` | SQLite (WAL): requests, cost, daily aggregates, quota snapshots, model catalog cache |
| `settings.json` | Only written after you change a setting |
| `proxy.port` | The port the proxy actually listens on (plain text) |
| `logs\hub-light.log.<date>` | Daily rolling logs |
| `logs\panic.log` | Panic location, message and backtrace (only on crash) |
| `logs\startup-error.log` | Fatal startup errors |
| `exports\usage_<from>_<to>.csv` | CSV exports from the Requests page |

- Tokens never reach the frontend, the logs or the database; if encryption is unavailable the app refuses to load credentials instead of falling back to plaintext.
- Request/response bodies are **not** recorded by default. If you enable recording, they are truncated to 64 KB and may contain source code.
- The proxy binds to loopback only and rejects requests with an `Origin` header.
- Request details are purged according to *usage retention days* (startup + every 24 h); exported CSVs are not.

## Code signing policy

Release installers are intended to be signed through SignPath Foundation: **free code signing provided by [SignPath.io](https://about.signpath.io), certificate by [SignPath Foundation](https://signpath.org)**.

| Version | Installer |
|---|---|
| `v0.1.0` | **Unsigned** — verify with the SHA-256 in the release notes, or build from the tag |
| `v0.2.0` | **Unsigned** — verify with the SHA-256 in the release notes, or build from the tag |
| Next releases | Signed by SignPath Foundation after manual approval |

The certificate is issued to SignPath Foundation, so Windows shows `SignPath Foundation` as the publisher. Verify a download with `Get-AuthenticodeSignature` or `signtool verify /pa`, or compare the SHA-256 published with each release.

Roles (solo maintainer), privacy statement, system changes and the exact signing process are documented in [docs/code-signing-policy.md](docs/code-signing-policy.md).

## Known limitations

- Windows only, no auto-update.
- Native Anthropic upstream is not supported; `/v1/messages` always goes through local translation, so some Anthropic-specific fields are dropped and the model is chosen by the gateway.
- If the upstream response carries no cost field, cost is recorded as 0 (`cost_source=none`); there is no local price table.
- The Requests page has no detail popover, and the "client" column is not linked to sessions yet (always `unknown`).
- "Today"/"This month" use the machine's local day, based on request start time.
- Account-level numbers come from the gateway and may include usage from other devices; they cannot be reconciled with the local request list.

## Documentation

| Document | Content |
|---|---|
| [docs/README.md](docs/README.md) | Documentation index and reading order |
| [Architecture](docs/architecture.md) | Processes, modules, startup order, data flows, invariants |
| [Authentication](docs/authentication.md) | Login sequence, deep link, credential storage, renewal |
| [Local proxy](docs/proxy.md) | Routes, header handling, translation, limits, status codes |
| [Gateway contract](docs/gateway-contract.md) | Endpoints a compatible gateway must implement |
| [Usage](docs/usage.md) | Token/cost accounting, aggregation, retention, CSV |
| [Settings](docs/settings.md) | Every `settings.json` key and its validation |
| [Data model](docs/data-model.md) | Tables, indexes, migrations |
| [IPC API](docs/ipc-api.md) | Tauri commands and events |
| [Development](docs/development.md) | Scripts, gates, CI, packaging pitfalls |
| [Troubleshooting](docs/troubleshooting.md) | Symptom → cause → action |
| [Code signing policy](docs/code-signing-policy.md) | Signing provider, verification, roles, privacy, system changes |

Doc prose is in Chinese; commands and identifiers stay in English.

## Contributing

Issues and pull requests are welcome. Before opening a PR, run `pnpm verify` and the Rust gates listed in [CONTRIBUTING.md](CONTRIBUTING.md); the `docs:check` gate rejects stray credentials, internal hostnames and documentation that drifted from code. By participating you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## License

GPL-3.0-only — see [LICENSE](LICENSE). Third-party dependencies keep their own licenses (see `pnpm-lock.yaml` and `src-tauri/Cargo.lock`).
