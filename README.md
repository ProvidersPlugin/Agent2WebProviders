# Agent2WebProviders

**English** · [简体中文](README.zh-CN.md)

**Web Providers** (网页提供商) is the official web-provider plugin for [Agent2API](https://github.com/aimod-cc/Agent2API).
It connects Zhipu Qingyan and DeepSeek web accounts to the gateway's unified model API.
The gateway runs the plugin as a separate process and manages its accounts, routing and usage reports.

Provider capabilities depend on the account and the integration. This plugin uses web-account credentials, not the providers' paid API keys.
For gateway installation and deployment, see the [Agent2API installation guide](https://github.com/aimod-cc/Agent2API/blob/main/docs/install.en.md).

## Supported Providers

The table describes the current implementation. It does not imply that every capability has been verified with a live account.

| Provider | LLM requests | Token auto-refresh | Model list | Balance query | Check-in | Claims |
| --- | :--: | :--: | :--: | :--: | :--: | :--: |
| Zhipu Qingyan | ✓ | ✓ | Online + fallback | Signed-in accounts | Signed-in accounts | Signed-in accounts |
| DeepSeek Web | Implemented; unverified | Undeclared | Static only | Undeclared | Undeclared | Undeclared |

“Undeclared” means the capability has not been confirmed or integrated; it does not establish that the upstream service lacks it.

### Zhipu Qingyan

- Supports guest accounts, pasted tokens and local Windows sign-in import.
- Fetches the online model catalog, with a built-in fallback.
- Renews signed-in credentials when a refresh token is available.
- Guests can chat and obtain replacement credentials, but have no points, check-in or reward system.
- Points queries, daily check-in, member rewards and task claims require a signed-in account.
- Existing accounts retain the provider ID `chatglm`.

### DeepSeek Web

- Accepts your own `chat.deepseek.com` `userToken.value`, with a Cookie when needed.
- Exposes `deepseek-web-flash` and `deepseek-web-pro` as static mappings to the web modes `default` and `expert`.
- Implements streamed text and reasoning, with prompt-based tool-call emulation and full-history feedback.
- Tool-call emulation is not native function calling. Incomplete responses are rejected before tool calls are returned.
- Image upload, generation and editing are not integrated.
- Paid API keys are not accepted. Human verification requires signing in manually on the official website.
- Live calls are untested. Online catalogs, credential renewal, balances, check-in and claims remain undeclared.

Tokens, cookies and account exports are sensitive credentials. Do not publish or share them.

## Development & Build

Requires Rust 1.88 or newer. The packaging script also requires Node.js.

Build and package for the current platform:

```bash
cargo build --release
node scripts/pack.mjs --skip-build
```

The output is `dist/agent2api-web-providers-<version>-<target>.zip`.
The package contains `pack.json`, the executable, icons and provider capability notes.

For another platform, prepare the corresponding toolchain first:

```bash
cargo build --release --target aarch64-unknown-linux-gnu
node scripts/pack.mjs --skip-build --target aarch64-unknown-linux-gnu
```

Each platform needs its own native executable. The manifest declares:

| Target | Platform |
| --- | --- |
| `x86_64-pc-windows-msvc` | Windows x64 |
| `x86_64-apple-darwin` | macOS Intel |
| `aarch64-apple-darwin` | macOS Apple Silicon |
| `x86_64-unknown-linux-gnu` | Linux / Docker amd64 |
| `aarch64-unknown-linux-gnu` | Linux / Docker arm64 |

## Repository Layout

- [`crates/providers/`](crates/providers/) — provider implementations and plugin executable.
- [`pack.json`](pack.json) — plugin identity, version, providers and platform entry points.
- [`ui/provider-docs.json`](ui/provider-docs.json) — capability notes in six languages.
- [`assets/`](assets/) — provider icons.
- [`scripts/pack.mjs`](scripts/pack.mjs) — native package creation.

The shared `agent2api-provider-protocol` and `agent2api-provider-sdk` crates come from [Agent2Providers](https://github.com/ProvidersPlugin/Agent2Providers).
Their Git dependencies are pinned by tag in [`crates/providers/Cargo.toml`](crates/providers/Cargo.toml).

## License

[GNU Affero General Public License v3.0 (AGPL-3.0)](LICENSE).
